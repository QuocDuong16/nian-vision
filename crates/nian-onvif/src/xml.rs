use chrono::{DateTime, Utc};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::{NsReader, Reader, XmlVersion};
use sha2::{Digest as _, Sha256};

use crate::types::{
    DeviceInformation, EventProperties, MediaProfile, MediaServiceKind, MotionNotification,
    ProbeMatch, PtzConfigurationOptions, PtzProfileAssociation, PtzVelocityRange,
    PullPointSubscription, ServiceEndpoint,
};
use crate::{
    EVENT_PULL_MESSAGE_LIMIT, MAX_ENDPOINT_REFERENCE_BYTES, MAX_EVENT_SIMPLE_ITEM_NAME_BYTES,
    MAX_EVENT_SIMPLE_ITEM_VALUE_BYTES, MAX_EVENT_SIMPLE_ITEMS, MAX_EVENT_TIMESTAMP_BYTES,
    MAX_EVENT_TOPIC_BYTES, MAX_EVENT_TOPIC_SET_NODES, MAX_PROFILE_NAME_BYTES,
    MAX_PROFILE_TOKEN_BYTES, MAX_PROFILES, MAX_PTZ_CONFIGURATION_TOKEN_BYTES, MAX_SCOPE_BYTES,
    MAX_SCOPES_PER_DEVICE, MAX_SOAP_RESPONSE_BYTES, MAX_XADDRS_PER_DEVICE, MAX_XML_DEPTH,
    MAX_XML_TEXT_BYTES, OnvifError,
};

const ONVIF_TOPICS_NAMESPACE: &str = "http://www.onvif.org/ver10/topics";

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
    "http://www.onvif.org/ver10/events/wsdl",
    "http://www.onvif.org/ver10/schema",
    "http://docs.oasis-open.org/wsn/b-2",
    "http://docs.oasis-open.org/wsn/t-1",
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
            | "Capabilities"
            | "Events"
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
            | "GetEventPropertiesResponse"
            | "TopicSet"
            | "Topic"
            | "NotificationMessage"
            | "Message"
            | "Source"
            | "Data"
            | "SimpleItem"
            | "SubscriptionReference"
            | "CurrentTime"
            | "TerminationTime"
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

pub(crate) fn parse_event_capability_xaddr(xml: &[u8]) -> Result<Option<String>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut depth = 0usize;
    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                if depth >= MAX_XML_DEPTH {
                    return Err(OnvifError::Protocol);
                }
                depth += 1;
                if local_name(start.name().as_ref()) == "Events"
                    && let Some(xaddr) = attribute(&start, "XAddr")?
                {
                    return Ok(Some(xaddr));
                }
            }
            Event::Empty(start) => {
                if local_name(start.name().as_ref()) == "Events"
                    && let Some(xaddr) = attribute(&start, "XAddr")?
                {
                    return Ok(Some(xaddr));
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(OnvifError::Protocol)?,
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
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

#[derive(Debug, Default)]
struct RawEventNotification {
    topic: Option<String>,
    topic_is_standard_motion: bool,
    utc_time: Option<String>,
    property_operation: Option<String>,
    source_items: Vec<(String, String)>,
    data_items: Vec<(String, String)>,
}

pub(crate) fn parse_event_properties(xml: &[u8]) -> Result<EventProperties, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut stack: Vec<(String, bool)> = Vec::new();
    let mut topic_nodes = 0usize;
    let mut motion_supported = false;

    loop {
        let (resolved, event) = reader
            .read_resolved_event()
            .map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(OnvifError::ResponseTooLarge);
                }
                let name = local_name(start.name().as_ref());
                let standard_topic = matches!(
                    resolved,
                    ResolveResult::Bound(namespace)
                        if namespace.as_ref() == ONVIF_TOPICS_NAMESPACE
                );
                stack.push((name, standard_topic));
                if let Some(topic_set) = stack.iter().position(|(name, _)| name == "TopicSet") {
                    topic_nodes = topic_nodes.saturating_add(1);
                    if topic_nodes > MAX_EVENT_TOPIC_SET_NODES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    let relative = &stack[topic_set + 1..];
                    if relative.len() >= 3
                        && relative[relative.len() - 3..]
                            .iter()
                            .map(|(name, standard)| (name.as_str(), *standard))
                            .eq([
                                ("RuleEngine", true),
                                ("CellMotionDetector", true),
                                ("Motion", true),
                            ])
                    {
                        motion_supported = true;
                    }
                }
            }
            Event::Empty(start) => {
                if let Some(topic_set) = stack.iter().position(|(name, _)| name == "TopicSet") {
                    topic_nodes = topic_nodes.saturating_add(1);
                    if topic_nodes > MAX_EVENT_TOPIC_SET_NODES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    let name = local_name(start.name().as_ref());
                    let standard_topic = matches!(
                        resolved,
                        ResolveResult::Bound(namespace)
                            if namespace.as_ref() == ONVIF_TOPICS_NAMESPACE
                    );
                    let mut relative = stack[topic_set + 1..].to_vec();
                    relative.push((name, standard_topic));
                    if relative.len() >= 3
                        && relative[relative.len() - 3..]
                            .iter()
                            .map(|(name, standard)| (name.as_str(), *standard))
                            .eq([
                                ("RuleEngine", true),
                                ("CellMotionDetector", true),
                                ("Motion", true),
                            ])
                    {
                        motion_supported = true;
                    }
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(OnvifError::Protocol)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(EventProperties { motion_supported })
}

type SubscriptionTimes = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
type SubscriptionMetadata = (Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

pub(crate) fn parse_pullpoint_subscription(
    xml: &[u8],
) -> Result<PullPointSubscription, OnvifError> {
    let (endpoint, current_time_utc, termination_time_utc) =
        parse_subscription_metadata(xml, true)?;
    Ok(PullPointSubscription {
        endpoint: endpoint.ok_or(OnvifError::Protocol)?,
        current_time_utc,
        termination_time_utc,
    })
}

pub(crate) fn parse_renew_times(xml: &[u8]) -> Result<SubscriptionTimes, OnvifError> {
    let (_, current, termination) = parse_subscription_metadata(xml, false)?;
    Ok((current, termination))
}

fn parse_subscription_metadata(
    xml: &[u8],
    require_reference: bool,
) -> Result<SubscriptionMetadata, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = reader(xml)?;
    let mut stack = Vec::new();
    let mut endpoint = None;
    let mut current = None;
    let mut termination = None;

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
                    Some("Address") if stack.iter().any(|name| name == "SubscriptionReference") => {
                        if value.len() > crate::MAX_URL_BYTES {
                            return Err(OnvifError::ResponseTooLarge);
                        }
                        endpoint = Some(value);
                    }
                    Some("CurrentTime") => {
                        current = Some(parse_strict_event_timestamp(&value)?);
                    }
                    Some("TerminationTime") => {
                        termination = Some(parse_strict_event_timestamp(&value)?);
                    }
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

    if require_reference && endpoint.is_none() {
        return Err(OnvifError::Protocol);
    }
    Ok((endpoint, current, termination))
}

pub(crate) fn parse_motion_notifications(
    xml: &[u8],
) -> Result<Vec<MotionNotification>, OnvifError> {
    validate_recognized_namespaces(xml)?;
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut stack = Vec::new();
    let mut current: Option<RawEventNotification> = None;
    let mut notifications = Vec::new();
    let mut notification_count = 0usize;
    let mut simple_item_count = 0usize;

    loop {
        let event = reader.read_event().map_err(|_| OnvifError::Protocol)?;
        if prohibited(&event) {
            return Err(OnvifError::Protocol);
        }
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if name == "NotificationMessage" {
                    if current.is_some() {
                        return Err(OnvifError::Protocol);
                    }
                    notification_count = notification_count.saturating_add(1);
                    if notification_count > EVENT_PULL_MESSAGE_LIMIT {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    current = Some(RawEventNotification::default());
                    simple_item_count = 0;
                }
                if name == "Message"
                    && stack.iter().any(|part| part == "NotificationMessage")
                    && let Some(notification) = current.as_mut()
                {
                    if let Some(value) = attribute(&start, "UtcTime")? {
                        if value.len() > MAX_EVENT_TIMESTAMP_BYTES {
                            return Err(OnvifError::ResponseTooLarge);
                        }
                        notification.utc_time = Some(value);
                    }
                    notification.property_operation = attribute(&start, "PropertyOperation")?;
                }
                push_start(&mut stack, name)?;
            }
            Event::Empty(start) => {
                let name = local_name(start.name().as_ref());
                if name == "SimpleItem" {
                    let Some(notification) = current.as_mut() else {
                        continue;
                    };
                    simple_item_count = simple_item_count.saturating_add(1);
                    if simple_item_count > MAX_EVENT_SIMPLE_ITEMS {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    let Some(item_name) = attribute(&start, "Name")? else {
                        continue;
                    };
                    let Some(item_value) = attribute(&start, "Value")? else {
                        continue;
                    };
                    if item_name.len() > MAX_EVENT_SIMPLE_ITEM_NAME_BYTES
                        || item_value.len() > MAX_EVENT_SIMPLE_ITEM_VALUE_BYTES
                    {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    if stack.iter().any(|part| part == "Source") {
                        notification.source_items.push((item_name, item_value));
                    } else if stack.iter().any(|part| part == "Data") {
                        notification.data_items.push((item_name, item_value));
                    }
                }
            }
            Event::Text(text) => {
                if stack.last().is_some_and(|name| name == "Topic")
                    && let Some(notification) = current.as_mut()
                {
                    let value = decode_text(text)?;
                    if value.len() > MAX_EVENT_TOPIC_BYTES {
                        return Err(OnvifError::ResponseTooLarge);
                    }
                    notification.topic_is_standard_motion =
                        is_standard_cell_motion_topic(reader.resolver(), &value);
                    notification.topic = Some(value);
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref());
                stack.pop().ok_or(OnvifError::Protocol)?;
                if name == "NotificationMessage"
                    && let Some(raw) = current.take()
                    && let Some(normalized) = normalize_motion_notification(raw)?
                {
                    notifications.push(normalized);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if current.is_some() {
        return Err(OnvifError::Protocol);
    }
    Ok(notifications)
}

fn normalize_motion_notification(
    mut raw: RawEventNotification,
) -> Result<Option<MotionNotification>, OnvifError> {
    let Some(_topic) = raw.topic.take() else {
        return Ok(None);
    };
    if !raw.topic_is_standard_motion {
        return Ok(None);
    }

    let mut motion = None;
    for (name, value) in &raw.data_items {
        if name != "IsMotion" {
            continue;
        }
        let parsed = match value.trim() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err(OnvifError::Protocol),
        };
        if motion.replace(parsed).is_some() {
            return Err(OnvifError::Protocol);
        }
    }
    let Some(active) = motion else {
        return Ok(None);
    };

    let source_key = if raw.source_items.is_empty() {
        None
    } else {
        raw.source_items.sort();
        let mut digest = Sha256::new();
        for (name, value) in raw.source_items {
            let name_len = u32::try_from(name.len()).map_err(|_| OnvifError::ResponseTooLarge)?;
            let value_len = u32::try_from(value.len()).map_err(|_| OnvifError::ResponseTooLarge)?;
            digest.update(name_len.to_be_bytes());
            digest.update(name.as_bytes());
            digest.update(value_len.to_be_bytes());
            digest.update(value.as_bytes());
        }
        Some(hex_lower(&digest.finalize()))
    };

    Ok(Some(MotionNotification {
        active,
        device_time_utc: raw
            .utc_time
            .as_deref()
            .and_then(parse_lenient_event_timestamp),
        source_key,
        synchronization_baseline: raw
            .property_operation
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("Initialized")),
    }))
}

fn is_standard_cell_motion_topic(
    resolver: &quick_xml::name::NamespaceResolver,
    topic: &str,
) -> bool {
    let parts = topic.trim().split('/').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 3 {
        return false;
    }
    let first = QName(parts[0]);
    let Some(prefix) = first.prefix() else {
        return false;
    };
    if first.local_name().as_ref() != "RuleEngine"
        || parts[1] != "CellMotionDetector"
        || parts[2] != "Motion"
    {
        return false;
    }
    matches!(
        resolver.resolve_prefix(Some(prefix), false),
        ResolveResult::Bound(namespace) if namespace.as_ref() == ONVIF_TOPICS_NAMESPACE
    )
}

fn parse_strict_event_timestamp(value: &str) -> Result<DateTime<Utc>, OnvifError> {
    if value.len() > MAX_EVENT_TIMESTAMP_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| OnvifError::Protocol)
}

fn parse_lenient_event_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if value.len() > MAX_EVENT_TIMESTAMP_BYTES {
        return None;
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
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
    fn event_properties_recognize_only_standard_cell_motion_topic_path() {
        let supported = br#"<tev:GetEventPropertiesResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:tns1="http://www.onvif.org/ver10/topics"><tev:TopicSet><tns1:RuleEngine><tns1:CellMotionDetector><tns1:Motion/></tns1:CellMotionDetector></tns1:RuleEngine></tev:TopicSet></tev:GetEventPropertiesResponse>"#;
        assert!(parse_event_properties(supported).unwrap().motion_supported);

        let spoofed = br#"<tev:GetEventPropertiesResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:tns1="urn:evil"><tev:TopicSet><tns1:RuleEngine><tns1:CellMotionDetector><tns1:Motion/></tns1:CellMotionDetector></tns1:RuleEngine></tev:TopicSet></tev:GetEventPropertiesResponse>"#;
        assert!(matches!(
            parse_event_properties(spoofed),
            Ok(EventProperties {
                motion_supported: false
            }) | Err(OnvifError::Protocol)
        ));

        let lookalike = br#"<tev:GetEventPropertiesResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl"><tev:TopicSet><RuleEngine><VendorMotion><Motion/></VendorMotion></RuleEngine></tev:TopicSet></tev:GetEventPropertiesResponse>"#;
        assert!(!parse_event_properties(lookalike).unwrap().motion_supported);
    }

    #[test]
    fn pullpoint_subscription_metadata_is_bounded_and_typed() {
        let xml = br#"<tev:CreatePullPointSubscriptionResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing"><tev:SubscriptionReference><wsa:Address>http://192.168.1.8:8080/onvif/pullpoint/42</wsa:Address></tev:SubscriptionReference><tev:CurrentTime>2026-09-05T00:00:00Z</tev:CurrentTime><tev:TerminationTime>2026-09-05T00:01:00Z</tev:TerminationTime></tev:CreatePullPointSubscriptionResponse>"#;
        let subscription = parse_pullpoint_subscription(xml).unwrap();
        assert_eq!(
            subscription.current_time_utc().unwrap().to_rfc3339(),
            "2026-09-05T00:00:00+00:00"
        );
        assert_eq!(
            subscription.termination_time_utc().unwrap().to_rfc3339(),
            "2026-09-05T00:01:00+00:00"
        );
        assert!(!format!("{subscription:?}").contains("pullpoint/42"));
    }

    #[test]
    fn motion_notification_normalizes_boolean_source_and_synchronization_without_raw_tokens() {
        let xml = br#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="http://www.onvif.org/ver10/topics"><s:Body><wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message UtcTime="2026-09-05T00:00:01Z" PropertyOperation="Initialized"><tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="SENTINEL-raw-camera-token"/></tt:Source><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage></s:Body></s:Envelope>"#;
        let notifications = parse_motion_notifications(xml).unwrap();
        assert_eq!(notifications.len(), 1);
        let notification = &notifications[0];
        assert!(notification.active);
        assert!(notification.synchronization_baseline);
        assert_eq!(notification.source_key.as_deref().map(str::len), Some(64));
        assert!(!format!("{notification:?}").contains("SENTINEL-raw-camera-token"));
    }

    #[test]
    fn incompatible_motion_topic_is_ignored_and_malformed_ismotion_is_rejected() {
        let lookalike = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema"><wsnt:Topic>RuleEngine/VendorMotion/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        assert!(parse_motion_notifications(lookalike).unwrap().is_empty());

        let spoofed = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="urn:evil"><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        let spoofed_result = parse_motion_notifications(spoofed);
        assert!(
            matches!(&spoofed_result, Ok(notifications) if notifications.is_empty())
                || matches!(spoofed_result, Err(OnvifError::Protocol))
        );

        let malformed = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="http://www.onvif.org/ver10/topics"><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="maybe"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        assert_eq!(
            parse_motion_notifications(malformed),
            Err(OnvifError::Protocol)
        );
    }

    #[test]
    fn event_topic_namespace_spoofing_cannot_enable_standard_motion() {
        let spoofed = br#"<tev:GetEventPropertiesResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:evil="urn:evil"><tev:TopicSet><evil:RuleEngine><evil:CellMotionDetector><evil:Motion/></evil:CellMotionDetector></evil:RuleEngine></tev:TopicSet></tev:GetEventPropertiesResponse>"#;
        assert!(matches!(
            parse_event_properties(spoofed),
            Ok(EventProperties {
                motion_supported: false
            }) | Err(OnvifError::Protocol)
        ));
    }

    #[test]
    fn notification_topic_qname_must_resolve_to_onvif_topics_namespace() {
        let standard = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="http://www.onvif.org/ver10/topics"><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        assert_eq!(parse_motion_notifications(standard).unwrap().len(), 1);

        let spoofed = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="urn:evil"><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        let spoofed = parse_motion_notifications(spoofed);
        assert!(
            matches!(&spoofed, Ok(notifications) if notifications.is_empty())
                || matches!(spoofed, Err(OnvifError::Protocol))
        );

        let vendor = br#"<wsnt:NotificationMessage xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="http://www.onvif.org/ver10/topics"><wsnt:Topic>tns1:VendorMotion/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#;
        assert!(parse_motion_notifications(vendor).unwrap().is_empty());
    }

    #[test]
    fn oversized_response_is_rejected_before_parsing() {
        let xml = vec![b'x'; MAX_SOAP_RESPONSE_BYTES + 1];
        assert_eq!(parse_services(&xml), Err(OnvifError::ResponseTooLarge));
    }
}
