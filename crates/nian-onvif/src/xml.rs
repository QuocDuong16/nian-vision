use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, Reader, XmlVersion};

use crate::types::{
    DeviceInformation, MediaProfile, MediaServiceKind, ProbeMatch, PtzConfigurationOptions,
    PtzProfileAssociation, PtzVelocityRange, ServiceEndpoint,
};
use crate::{
    MAX_ENDPOINT_REFERENCE_BYTES, MAX_PROFILE_NAME_BYTES, MAX_PROFILE_TOKEN_BYTES, MAX_PROFILES,
    MAX_PTZ_CONFIGURATION_TOKEN_BYTES, MAX_SCOPE_BYTES, MAX_SCOPES_PER_DEVICE,
    MAX_SOAP_RESPONSE_BYTES, MAX_XADDRS_PER_DEVICE, MAX_XML_DEPTH, MAX_XML_TEXT_BYTES, OnvifError,
};

fn local_name(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).to_owned()
}

fn attribute(start: &BytesStart<'_>, name: &str) -> Result<Option<String>, OnvifError> {
    for attr in start.attributes().with_checks(true) {
        let attr = attr.map_err(|_| OnvifError::Protocol)?;
        if local_name(attr.key.as_ref()) == name {
            let value = attr
                .normalized_value(XmlVersion::Explicit1_0)
                .map_err(|_| OnvifError::Protocol)?
                .into_owned();
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn decode_text(text: quick_xml::events::BytesText<'_>) -> Result<String, OnvifError> {
    let decoded = text.xml10_content();
    let value = quick_xml::escape::unescape(&decoded)
        .map_err(|_| OnvifError::Protocol)?
        .into_owned();
    if value.len() > MAX_XML_TEXT_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    Ok(value)
}

fn ensure_body_limit(xml: &[u8]) -> Result<(), OnvifError> {
    if xml.len() > MAX_SOAP_RESPONSE_BYTES {
        Err(OnvifError::ResponseTooLarge)
    } else {
        Ok(())
    }
}

fn reader(xml: &[u8]) -> Result<Reader<&[u8]>, OnvifError> {
    ensure_body_limit(xml)?;
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    Ok(reader)
}

fn push_start(stack: &mut Vec<String>, name: String) -> Result<(), OnvifError> {
    if stack.len() >= MAX_XML_DEPTH {
        return Err(OnvifError::Protocol);
    }
    stack.push(name);
    Ok(())
}

fn prohibited(event: &Event<'_>) -> bool {
    matches!(event, Event::DocType(_) | Event::GeneralRef(_))
}

const MAX_NAMESPACE_BINDINGS: usize = 64;

const ALLOWED_ONVIF_NAMESPACES: &[&str] = &[
    "http://www.w3.org/2003/05/soap-envelope",
    "http://schemas.xmlsoap.org/ws/2004/08/addressing",
    "http://www.w3.org/2005/08/addressing",
    "http://schemas.xmlsoap.org/ws/2005/04/discovery",
    "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01",
    "http://www.onvif.org/ver10/network/wsdl",
    "http://www.onvif.org/ver10/device/wsdl",
    "http://www.onvif.org/ver10/media/wsdl",
    "http://www.onvif.org/ver20/media/wsdl",
    "http://www.onvif.org/ver20/ptz/wsdl",
    "http://www.onvif.org/ver10/schema",
];

fn recognized_onvif_field(name: &str) -> bool {
    matches!(
        name,
        "ProbeMatch"
            | "ProbeMatches"
            | "EndpointReference"
            | "Address"
            | "Types"
            | "XAddrs"
            | "Scopes"
            | "Manufacturer"
            | "Model"
            | "FirmwareVersion"
            | "SerialNumber"
            | "HardwareId"
            | "HostnameInformation"
            | "Name"
            | "Service"
            | "Namespace"
            | "XAddr"
            | "Profiles"
            | "Profile"
            | "VideoEncoderConfiguration"
            | "AudioEncoderConfiguration"
            | "Encoding"
            | "Resolution"
            | "Width"
            | "Height"
            | "RateControl"
            | "FrameRateLimit"
            | "BitrateLimit"
            | "Uri"
            | "PTZConfiguration"
            | "Configurations"
            | "PTZ"
            | "Spaces"
            | "ContinuousPanTiltVelocitySpace"
            | "ContinuousZoomVelocitySpace"
            | "XRange"
            | "YRange"
            | "Min"
            | "Max"
    )
}

fn validate_recognized_namespaces(xml: &[u8]) -> Result<(), OnvifError> {
    ensure_body_limit(xml)?;
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut namespace_bindings = 0usize;

    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OnvifError::Protocol)?;
        if matches!(event, Event::DocType(_)) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) | Event::Empty(start) => {
                for attr in start.attributes().with_checks(true) {
                    let attr = attr.map_err(|_| OnvifError::Protocol)?;
                    let key = attr.key.as_ref();
                    if key == "xmlns" || key.starts_with("xmlns:") {
                        namespace_bindings = namespace_bindings.saturating_add(1);
                        if namespace_bindings > MAX_NAMESPACE_BINDINGS {
                            return Err(OnvifError::ResponseTooLarge);
                        }
                    }
                }

                let local_name = start.local_name();
                let name = local_name.as_ref();
                if !recognized_onvif_field(name) {
                    continue;
                }
                match resolved {
                    ResolveResult::Bound(namespace) => {
                        let namespace = namespace.as_ref();
                        if !ALLOWED_ONVIF_NAMESPACES.contains(&namespace) {
                            return Err(OnvifError::Protocol);
                        }
                    }
                    ResolveResult::Unbound => {}
                    ResolveResult::Unknown(_) => return Err(OnvifError::Protocol),
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

pub(crate) fn parse_probe_matches(xml: &[u8]) -> Result<Vec<ProbeMatch>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut current: Option<ProbeMatch> = None;
    let mut matches = Vec::new();

    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if name == "ProbeMatch" {
                    current = Some(ProbeMatch {
                        endpoint_reference: String::new(),
                        xaddrs: Vec::new(),
                        scopes: Vec::new(),
                        is_network_video_transmitter: false,
                    });
                }
                push_start(&mut stack, name)?;
            }
            Event::Text(text) => {
                let Some(current) = current.as_mut() else {
                    continue;
                };
                let value = decode_text(text)?;
                match stack.last().map(String::as_str) {
                    Some("Address") if stack.iter().any(|name| name == "EndpointReference") => {
                        if value.len() > MAX_ENDPOINT_REFERENCE_BYTES {
                            return Err(OnvifError::ResponseTooLarge);
                        }
                        current.endpoint_reference = value;
                    }
                    Some("XAddrs") => {
                        for xaddr in value.split_whitespace() {
                            if current.xaddrs.len() >= MAX_XADDRS_PER_DEVICE {
                                break;
                            }
                            current.xaddrs.push(xaddr.to_owned());
                        }
                    }
                    Some("Scopes") => {
                        for scope in value.split_whitespace() {
                            if scope.len() > MAX_SCOPE_BYTES {
                                return Err(OnvifError::ResponseTooLarge);
                            }
                            if current.scopes.len() >= MAX_SCOPES_PER_DEVICE {
                                break;
                            }
                            current.scopes.push(scope.to_owned());
                        }
                    }
                    Some("Types") => {
                        current.is_network_video_transmitter = value
                            .split_whitespace()
                            .any(|part| part.rsplit(':').next() == Some("NetworkVideoTransmitter"));
                    }
                    _ => {}
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                if name == "ProbeMatch"
                    && let Some(candidate) = current.take()
                    && !candidate.endpoint_reference.is_empty()
                    && !candidate.xaddrs.is_empty()
                {
                    matches.push(candidate);
                }
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(matches)
}

pub(crate) fn parse_device_information(xml: &[u8]) -> Result<DeviceInformation, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut info = DeviceInformation::default();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => push_start(&mut stack, local_name(start.name().as_ref()))?,
            Event::Text(text) => {
                let value = decode_text(text)?;
                match stack.last().map(String::as_str) {
                    Some("Manufacturer") => info.manufacturer = Some(value),
                    Some("Model") => info.model = Some(value),
                    Some("FirmwareVersion") => info.firmware_version = Some(value),
                    Some("SerialNumber") => info.serial_number = Some(value),
                    Some("HardwareId") => info.hardware_id = Some(value),
                    _ => {}
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if info.manufacturer.is_none()
        && info.model.is_none()
        && info.firmware_version.is_none()
        && info.serial_number.is_none()
        && info.hardware_id.is_none()
    {
        return Err(OnvifError::Protocol);
    }
    Ok(info)
}

pub(crate) fn parse_hostname(xml: &[u8]) -> Result<Option<String>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => push_start(&mut stack, local_name(start.name().as_ref()))?,
            Event::Text(text) if stack.last().is_some_and(|name| name == "Name") => {
                return Ok(Some(decode_text(text)?));
            }
            Event::End(_) => {
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

pub(crate) fn parse_services(xml: &[u8]) -> Result<Vec<ServiceEndpoint>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut namespace: Option<String> = None;
    let mut xaddr: Option<String> = None;
    let mut services = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if name == "Service" {
                    namespace = None;
                    xaddr = None;
                }
                push_start(&mut stack, name)?;
            }
            Event::Text(text) => {
                let value = decode_text(text)?;
                match stack.last().map(String::as_str) {
                    Some("Namespace") => namespace = Some(value),
                    Some("XAddr") => xaddr = Some(value),
                    _ => {}
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                if name == "Service"
                    && let (Some(namespace), Some(xaddr)) = (namespace.take(), xaddr.take())
                {
                    services.push(ServiceEndpoint { namespace, xaddr });
                }
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(services)
}

#[derive(Default)]
struct ProfileBuilder {
    token: Option<String>,
    name: Option<String>,
    video_codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    framerate: Option<u32>,
    bitrate_kbps: Option<u32>,
    audio_codec: Option<String>,
}

fn in_video(stack: &[String]) -> bool {
    stack.iter().any(|name| name.contains("VideoEncoder"))
}

fn in_audio(stack: &[String]) -> bool {
    stack.iter().any(|name| name.contains("AudioEncoder"))
}

pub(crate) fn parse_profiles(
    xml: &[u8],
    service_kind: MediaServiceKind,
) -> Result<Vec<MediaProfile>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut current: Option<ProfileBuilder> = None;
    let mut profiles = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if matches!(name.as_str(), "Profiles" | "Profile") {
                    if profiles.len() >= MAX_PROFILES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    let token = attribute(&start, "token")?;
                    if let Some(token) = token.as_ref()
                        && token.len() > MAX_PROFILE_TOKEN_BYTES
                    {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    current = Some(ProfileBuilder {
                        token,
                        ..ProfileBuilder::default()
                    });
                }
                push_start(&mut stack, name)?;
            }
            Event::Text(text) => {
                let Some(current) = current.as_mut() else {
                    continue;
                };
                let value = decode_text(text)?;
                let field = stack.last().map(String::as_str);
                match field {
                    Some("Name") if current.name.is_none() => {
                        if value.len() > MAX_PROFILE_NAME_BYTES {
                            return Err(OnvifError::ResponseTooLarge);
                        }
                        current.name = Some(value);
                    }
                    Some("Encoding") if in_video(&stack) => current.video_codec = Some(value),
                    Some("Encoding") if in_audio(&stack) => current.audio_codec = Some(value),
                    Some("Width") if in_video(&stack) => current.width = parse_u32(&value),
                    Some("Height") if in_video(&stack) => current.height = parse_u32(&value),
                    Some("FrameRateLimit") if in_video(&stack) => {
                        current.framerate = parse_u32(&value)
                    }
                    Some("BitrateLimit") if in_video(&stack) => {
                        current.bitrate_kbps = parse_u32(&value)
                    }
                    _ => {}
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                if matches!(name.as_str(), "Profiles" | "Profile")
                    && let Some(current) = current.take()
                {
                    let token = current.token.ok_or(OnvifError::Protocol)?;
                    profiles.push(MediaProfile {
                        token,
                        name: current.name,
                        video_codec: current.video_codec,
                        width: current.width,
                        height: current.height,
                        framerate: current.framerate,
                        bitrate_kbps: current.bitrate_kbps,
                        audio_codec: current.audio_codec,
                        service_kind,
                    });
                }
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(profiles)
}

pub(crate) fn parse_ptz_profile_associations(
    xml: &[u8],
) -> Result<Vec<PtzProfileAssociation>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut current: Option<(String, Option<String>)> = None;
    let mut associations = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if matches!(name.as_str(), "Profiles" | "Profile") && current.is_none() {
                    let token = attribute(&start, "token")?.ok_or(OnvifError::Protocol)?;
                    if token.len() > MAX_PROFILE_TOKEN_BYTES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    current = Some((token, None));
                } else if name == "PTZConfiguration"
                    || (name == "PTZ" && stack.iter().any(|item| item == "Configurations"))
                {
                    capture_ptz_configuration_token(&start, &mut current)?;
                }
                push_start(&mut stack, name)?;
            }
            Event::Empty(start) => {
                let name = local_name(start.name().as_ref());
                if name == "PTZConfiguration"
                    || (name == "PTZ" && stack.iter().any(|item| item == "Configurations"))
                {
                    capture_ptz_configuration_token(&start, &mut current)?;
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                if matches!(name.as_str(), "Profiles" | "Profile")
                    && let Some((profile_token, Some(configuration_token))) = current.take()
                {
                    if associations.len() >= MAX_PROFILES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    associations.push(PtzProfileAssociation {
                        profile_token,
                        configuration_token,
                    });
                }
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(associations)
}

fn capture_ptz_configuration_token(
    start: &BytesStart<'_>,
    current: &mut Option<(String, Option<String>)>,
) -> Result<(), OnvifError> {
    let Some((_, configuration)) = current.as_mut() else {
        return Ok(());
    };
    if configuration.is_some() {
        return Ok(());
    }
    if let Some(token) = attribute(start, "token")? {
        if token.len() > MAX_PTZ_CONFIGURATION_TOKEN_BYTES {
            return Err(OnvifError::ResponseTooLarge);
        }
        *configuration = Some(token);
    }
    Ok(())
}

pub(crate) fn parse_ptz_configuration_options(
    xml: &[u8],
) -> Result<PtzConfigurationOptions, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let (mut pan_min, mut pan_max) = (None, None);
    let (mut tilt_min, mut tilt_max) = (None, None);
    let (mut zoom_min, mut zoom_max) = (None, None);
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => push_start(&mut stack, local_name(start.name().as_ref()))?,
            Event::Text(text)
                if matches!(stack.last().map(String::as_str), Some("Min" | "Max")) =>
            {
                let value = decode_text(text)?
                    .parse::<f64>()
                    .map_err(|_| OnvifError::Protocol)?;
                if !value.is_finite() {
                    return Err(OnvifError::Protocol);
                }
                let is_min = stack.last().is_some_and(|name| name == "Min");
                let pan_tilt = stack
                    .iter()
                    .any(|name| name == "ContinuousPanTiltVelocitySpace");
                let zoom = stack
                    .iter()
                    .any(|name| name == "ContinuousZoomVelocitySpace");
                let x = stack.iter().any(|name| name == "XRange");
                let y = stack.iter().any(|name| name == "YRange");
                match (pan_tilt, zoom, x, y, is_min) {
                    (true, false, true, false, true) => pan_min = Some(value),
                    (true, false, true, false, false) => pan_max = Some(value),
                    (true, false, false, true, true) => tilt_min = Some(value),
                    (true, false, false, true, false) => tilt_max = Some(value),
                    (false, true, true, false, true) => zoom_min = Some(value),
                    (false, true, true, false, false) => zoom_max = Some(value),
                    _ => {}
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    fn range(min: Option<f64>, max: Option<f64>) -> Result<Option<PtzVelocityRange>, OnvifError> {
        match (min, max) {
            (Some(min), Some(max)) => Ok(Some(PtzVelocityRange { min, max }.validate()?)),
            (None, None) => Ok(None),
            _ => Err(OnvifError::Protocol),
        }
    }
    Ok(PtzConfigurationOptions {
        pan: range(pan_min, pan_max)?,
        tilt: range(tilt_min, tilt_max)?,
        zoom: range(zoom_min, zoom_max)?,
    })
}

pub(crate) fn parse_stream_uri(xml: &[u8]) -> Result<String, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut uri = String::new();
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        match event {
            Event::DocType(_) => return Err(OnvifError::Protocol),
            Event::Start(start) => push_start(&mut stack, local_name(start.name().as_ref()))?,
            Event::Text(text) if stack.last().is_some_and(|name| name == "Uri") => {
                uri.push_str(&decode_text(text)?);
                if uri.len() > MAX_XML_TEXT_BYTES {
                    return Err(OnvifError::ResponseTooLarge);
                }
            }
            Event::GeneralRef(reference) if stack.last().is_some_and(|name| name == "Uri") => {
                let encoded = format!("&{};", reference.xml10_content());
                let decoded =
                    quick_xml::escape::unescape(&encoded).map_err(|_| OnvifError::Protocol)?;
                // Reject named custom entities. `unescape` only resolves XML's
                // predefined names and numeric character references.
                if decoded.as_ref() == encoded {
                    return Err(OnvifError::Protocol);
                }
                uri.push_str(&decoded);
                if uri.len() > MAX_XML_TEXT_BYTES {
                    return Err(OnvifError::ResponseTooLarge);
                }
            }
            Event::GeneralRef(_) => return Err(OnvifError::Protocol),
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                stack.pop().ok_or(OnvifError::Protocol)?;
                if name == "Uri" {
                    return (!uri.is_empty()).then_some(uri).ok_or(OnvifError::Protocol);
                }
            }
            Event::Eof => return Err(OnvifError::Protocol),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_match_parses_network_video_transmitter_and_multiple_xaddrs() {
        let xml = br#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:cam-1</a:Address></a:EndpointReference><d:Types>dn:NetworkVideoTransmitter</d:Types><d:Scopes>onvif://www.onvif.org/name/front onvif://www.onvif.org/hardware/model</d:Scopes><d:XAddrs>http://192.168.1.8/onvif/device_service http://camera.local/onvif/device_service</d:XAddrs></d:ProbeMatch></d:ProbeMatches></e:Body></e:Envelope>"#;
        let matches = parse_probe_matches(xml).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].is_network_video_transmitter);
        assert_eq!(matches[0].xaddrs.len(), 2);
    }

    #[test]
    fn malicious_doctype_and_deep_xml_are_rejected() {
        let doctype =
            br#"<!DOCTYPE foo [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><foo>&xxe;</foo>"#;
        assert_eq!(parse_services(doctype), Err(OnvifError::Protocol));
        let mut deep = String::new();
        for _ in 0..=MAX_XML_DEPTH {
            deep.push_str("<x>");
        }
        for _ in 0..=MAX_XML_DEPTH {
            deep.push_str("</x>");
        }
        assert_eq!(parse_services(deep.as_bytes()), Err(OnvifError::Protocol));
    }

    #[test]
    fn recognized_fields_bound_to_unrelated_namespaces_are_rejected() {
        let spoofed_profile = br#"<root xmlns:evil="urn:evil"><evil:Profiles token="main"><evil:VideoEncoderConfiguration><evil:Encoding>H264</evil:Encoding></evil:VideoEncoderConfiguration></evil:Profiles></root>"#;
        assert_eq!(
            parse_profiles(spoofed_profile, MediaServiceKind::LegacyMedia),
            Err(OnvifError::Protocol)
        );

        let spoofed_discovery = br#"<root xmlns:evil="urn:evil"><evil:ProbeMatch><evil:EndpointReference><evil:Address>urn:uuid:fake</evil:Address></evil:EndpointReference><evil:Types>dn:NetworkVideoTransmitter</evil:Types><evil:XAddrs>http://192.168.1.8/onvif/device_service</evil:XAddrs></evil:ProbeMatch></root>"#;
        assert_eq!(
            parse_probe_matches(spoofed_discovery),
            Err(OnvifError::Protocol)
        );
    }

    #[test]
    fn namespace_binding_count_is_bounded() {
        let mut xml = String::from("<root");
        for index in 0..=MAX_NAMESPACE_BINDINGS {
            xml.push_str(&format!(" xmlns:p{index}=\"urn:fixture:{index}\""));
        }
        xml.push_str("><Profiles token=\"main\"/></root>");
        assert_eq!(
            parse_profiles(xml.as_bytes(), MediaServiceKind::LegacyMedia),
            Err(OnvifError::ResponseTooLarge)
        );
    }

    #[test]
    fn parses_h264_and_h265_profiles_without_silently_conflating_them() {
        let xml = br#"<GetProfilesResponse><Profiles token="main"><Name>Main</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1920</Width><Height>1080</Height></Resolution><RateControl><FrameRateLimit>25</FrameRateLimit><BitrateLimit>4096</BitrateLimit></RateControl></VideoEncoderConfiguration><AudioEncoderConfiguration><Encoding>AAC</Encoding></AudioEncoderConfiguration></Profiles><Profiles token="sub"><Name>Sub HEVC</Name><VideoEncoderConfiguration><Encoding>H265</Encoding><Resolution><Width>640</Width><Height>360</Height></Resolution></VideoEncoderConfiguration></Profiles></GetProfilesResponse>"#;
        let profiles = parse_profiles(xml, MediaServiceKind::LegacyMedia).unwrap();
        assert_eq!(profiles.len(), 2);
        assert!(profiles[0].is_h264_compatible());
        assert_eq!(profiles[0].width, Some(1920));
        assert_eq!(profiles[0].audio_codec.as_deref(), Some("AAC"));
        assert!(!profiles[1].is_h264_compatible());
    }

    #[test]
    fn parses_media_profile_ptz_association_and_bounded_velocity_spaces() {
        let profiles = br#"<trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><trt:Profiles token="main"><tt:PTZConfiguration token="ptz-config-1"/></trt:Profiles></trt:GetProfilesResponse>"#;
        let associations = parse_ptz_profile_associations(profiles).unwrap();
        assert_eq!(associations.len(), 1);
        assert_eq!(associations[0].profile_token, "main");
        assert_eq!(associations[0].configuration_token, "ptz-config-1");

        let options = br#"<tptz:GetConfigurationOptionsResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:PTZConfigurationOptions><tt:Spaces><tt:ContinuousPanTiltVelocitySpace><tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-0.5</tt:Min><tt:Max>0.75</tt:Max></tt:YRange></tt:ContinuousPanTiltVelocitySpace><tt:ContinuousZoomVelocitySpace><tt:XRange><tt:Min>-0.25</tt:Min><tt:Max>0.5</tt:Max></tt:XRange></tt:ContinuousZoomVelocitySpace></tt:Spaces></tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>"#;
        let options = parse_ptz_configuration_options(options).unwrap();
        assert_eq!(
            options.pan,
            Some(PtzVelocityRange {
                min: -1.0,
                max: 1.0
            })
        );
        assert_eq!(
            options.tilt,
            Some(PtzVelocityRange {
                min: -0.5,
                max: 0.75
            })
        );
        assert_eq!(
            options.zoom,
            Some(PtzVelocityRange {
                min: -0.25,
                max: 0.5
            })
        );
    }

    #[test]
    fn ptz_parser_rejects_namespace_spoofing_and_invalid_ranges() {
        let spoofed = br#"<root xmlns:evil="urn:evil"><evil:Profiles token="main"><evil:PTZConfiguration token="ptz"/></evil:Profiles></root>"#;
        assert_eq!(
            parse_ptz_profile_associations(spoofed),
            Err(OnvifError::Protocol)
        );

        let invalid = br#"<root><Spaces><ContinuousPanTiltVelocitySpace><XRange><Min>1</Min><Max>-1</Max></XRange><YRange><Min>-1</Min><Max>1</Max></YRange></ContinuousPanTiltVelocitySpace></Spaces></root>"#;
        assert_eq!(
            parse_ptz_configuration_options(invalid),
            Err(OnvifError::Protocol)
        );
    }

    #[test]
    fn oversized_response_is_rejected_before_parsing() {
        let xml = vec![b'x'; MAX_SOAP_RESPONSE_BYTES + 1];
        assert_eq!(parse_services(&xml), Err(OnvifError::ResponseTooLarge));
    }
}
