use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use md5::Md5;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::redirect::Policy;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use url::Url;
use uuid::Uuid;

use crate::authority::{parse_stream_uri as normalize_stream_uri, validate_service_xaddr};
use crate::types::{
    MediaProfile, MediaServiceKind, OnvifCredentials, OnvifInterrogation, ServiceEndpoint,
};
use crate::xml::{
    parse_device_information, parse_hostname, parse_profiles, parse_services, parse_stream_uri,
};
use crate::{HTTP_TIMEOUT_MS, MAX_SOAP_RESPONSE_BYTES, OnvifError, StreamEndpoint};

const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA1_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2_NS: &str = "http://www.onvif.org/ver20/media/wsdl";

#[derive(Clone)]
pub struct OnvifClient {
    http: Client,
    username_token_authorities: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for OnvifClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnvifClient").finish_non_exhaustive()
    }
}

impl OnvifClient {
    pub fn new() -> Result<Self, OnvifError> {
        let http = Client::builder()
            .timeout(Duration::from_millis(HTTP_TIMEOUT_MS))
            .redirect(Policy::none())
            .build()
            .map_err(|_| OnvifError::Internal)?;
        Ok(Self {
            http,
            username_token_authorities: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn interrogate(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<OnvifInterrogation, OnvifError> {
        let info_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetDeviceInformation"),
            "<tds:GetDeviceInformation/>",
        )?;
        let mut device = parse_device_information(&info_xml)?;

        if let Ok(hostname_xml) = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetHostname"),
            "<tds:GetHostname/>",
        ) {
            device.hostname = parse_hostname(&hostname_xml).ok().flatten();
        }

        let services_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetServices"),
            "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        )?;
        let services = parse_services(&services_xml)?;
        let media2 = preferred_service_xaddr(&services, "/ver20/media/wsdl", device_service);
        let media1 = preferred_service_xaddr(&services, "/ver10/media/wsdl", device_service);

        let (media_service, media_service_kind, mut profiles) = if let Some(service) = media2 {
            match self.get_profiles(&service, credentials, MediaServiceKind::Media2) {
                Ok(profiles) if !profiles.is_empty() => {
                    (service, MediaServiceKind::Media2, profiles)
                }
                Ok(_) | Err(OnvifError::Protocol | OnvifError::Unsupported) => {
                    let service = media1.ok_or(OnvifError::Unsupported)?;
                    let profiles =
                        self.get_profiles(&service, credentials, MediaServiceKind::LegacyMedia)?;
                    (service, MediaServiceKind::LegacyMedia, profiles)
                }
                Err(error) => return Err(error),
            }
        } else {
            let service = media1.ok_or(OnvifError::Unsupported)?;
            let profiles =
                self.get_profiles(&service, credentials, MediaServiceKind::LegacyMedia)?;
            (service, MediaServiceKind::LegacyMedia, profiles)
        };

        profiles.sort_by(|left, right| {
            right
                .is_h264_compatible()
                .cmp(&left.is_h264_compatible())
                .then_with(|| profile_pixels(right).cmp(&profile_pixels(left)))
                .then_with(|| left.token.cmp(&right.token))
        });
        if !profiles.iter().any(MediaProfile::is_h264_compatible) {
            return Err(OnvifError::NoCompatibleProfile);
        }

        Ok(OnvifInterrogation {
            device,
            media_service,
            media_service_kind,
            profiles,
        })
    }

    pub fn stream_endpoint(
        &self,
        device_service: &str,
        media_service: &str,
        credentials: &OnvifCredentials,
        profile: &MediaProfile,
    ) -> Result<StreamEndpoint, OnvifError> {
        if !profile.is_h264_compatible() {
            return Err(OnvifError::NoCompatibleProfile);
        }
        let token = xml_escape(&profile.token);
        let (action, body) = match profile.service_kind {
            MediaServiceKind::Media2 => (
                format!("{MEDIA2_NS}/GetStreamUri"),
                format!(
                    "<tr2:GetStreamUri><tr2:Protocol>RTSP</tr2:Protocol><tr2:ProfileToken>{token}</tr2:ProfileToken></tr2:GetStreamUri>"
                ),
            ),
            MediaServiceKind::LegacyMedia => (
                format!("{MEDIA1_NS}/GetStreamUri"),
                format!(
                    "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{token}</trt:ProfileToken></trt:GetStreamUri>"
                ),
            ),
        };
        let xml = self.soap(media_service, credentials, &action, &body)?;
        let raw = parse_stream_uri(&xml)?;
        normalize_stream_uri(&raw, device_service)
    }

    fn get_profiles(
        &self,
        service: &str,
        credentials: &OnvifCredentials,
        kind: MediaServiceKind,
    ) -> Result<Vec<MediaProfile>, OnvifError> {
        let (action, body) = match kind {
            MediaServiceKind::Media2 => (
                format!("{MEDIA2_NS}/GetProfiles"),
                "<tr2:GetProfiles><tr2:Type>All</tr2:Type></tr2:GetProfiles>",
            ),
            MediaServiceKind::LegacyMedia => {
                (format!("{MEDIA1_NS}/GetProfiles"), "<trt:GetProfiles/>")
            }
        };
        let xml = self.soap(service, credentials, &action, body)?;
        parse_profiles(&xml, kind)
    }

    fn send_soap_request(
        &self,
        url: &str,
        content_type: &str,
        envelope: String,
        authorization: Option<String>,
    ) -> Result<Response, OnvifError> {
        let mut request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, content_type)
            .body(envelope);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        request.send().map_err(map_http_error)
    }

    fn soap(
        &self,
        url: &str,
        credentials: &OnvifCredentials,
        action: &str,
        body: &str,
    ) -> Result<Vec<u8>, OnvifError> {
        let parsed = Url::parse(url).map_err(|_| OnvifError::Protocol)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.username() != ""
            || parsed.password().is_some()
        {
            return Err(OnvifError::AuthorityRejected);
        }

        let content_type = format!("application/soap+xml; charset=utf-8; action=\"{action}\"");
        let authority = auth_authority_key(&parsed)?;
        let prefer_username_token = self
            .username_token_authorities
            .lock()
            .map_err(|_| OnvifError::Internal)?
            .contains(&authority);

        if prefer_username_token {
            let response =
                self.send_soap_request(url, &content_type, soap_envelope(credentials, body), None)?;
            if response.status().as_u16() == 401
                && let Some(challenge) = digest_challenge_from_response(&response)
                && let Ok(authorization) = digest_authorization(credentials, &parsed, &challenge)
            {
                self.username_token_authorities
                    .lock()
                    .map_err(|_| OnvifError::Internal)?
                    .remove(&authority);
                let retry = self.send_soap_request(
                    url,
                    &content_type,
                    anonymous_soap_envelope(body),
                    Some(authorization),
                )?;
                return read_response(retry);
            }
            return read_response(response);
        }

        let anonymous = anonymous_soap_envelope(body);
        let response = self.send_soap_request(url, &content_type, anonymous.clone(), None)?;
        let status = response.status();
        if status.is_success() || status.is_redirection() || matches!(status.as_u16(), 404 | 405) {
            return read_response(response);
        }

        if status.as_u16() == 401
            && let Some(challenge) = digest_challenge_from_response(&response)
            && let Ok(authorization) = digest_authorization(credentials, &parsed, &challenge)
        {
            let retry =
                self.send_soap_request(url, &content_type, anonymous, Some(authorization))?;
            return read_response(retry);
        }

        if !matches!(status.as_u16(), 400 | 401 | 403 | 500) {
            return read_response(response);
        }

        let response =
            self.send_soap_request(url, &content_type, soap_envelope(credentials, body), None)?;
        let result = read_response(response);
        if result.is_ok() {
            self.username_token_authorities
                .lock()
                .map_err(|_| OnvifError::Internal)?
                .insert(authority);
        }
        result
    }
}

fn preferred_service_xaddr(
    services: &[ServiceEndpoint],
    namespace_fragment: &str,
    device_service: &str,
) -> Option<String> {
    services
        .iter()
        .filter(|service| service.namespace.contains(namespace_fragment))
        .filter_map(|service| validate_service_xaddr(&service.xaddr, device_service).ok())
        .min_by(|left, right| {
            right
                .starts_with("https://")
                .cmp(&left.starts_with("https://"))
                .then_with(|| left.cmp(right))
        })
}

fn auth_authority_key(url: &Url) -> Result<String, OnvifError> {
    let host = url.host_str().ok_or(OnvifError::AuthorityRejected)?;
    let port = url
        .port_or_known_default()
        .ok_or(OnvifError::AuthorityRejected)?;
    Ok(format!(
        "{}://{}:{port}",
        url.scheme(),
        host.to_ascii_lowercase()
    ))
}

fn digest_challenge_from_response(response: &Response) -> Option<DigestChallenge> {
    response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(parse_digest_challenge_value)
}

fn parse_digest_challenge_value(raw: &str) -> Option<DigestChallenge> {
    let start = raw.to_ascii_lowercase().find("digest ")?;
    parse_digest_challenge(&raw[start..])
}

fn profile_pixels(profile: &MediaProfile) -> u64 {
    u64::from(profile.width.unwrap_or(0)) * u64::from(profile.height.unwrap_or(0))
}

fn map_http_error(error: reqwest::Error) -> OnvifError {
    if error.is_timeout() {
        OnvifError::Timeout
    } else {
        OnvifError::DeviceUnreachable
    }
}

fn read_response(mut response: Response) -> Result<Vec<u8>, OnvifError> {
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(OnvifError::AuthFailed);
    }
    if status.is_redirection() {
        return Err(OnvifError::AuthorityRejected);
    }
    if !status.is_success() {
        return Err(if status.as_u16() == 404 || status.as_u16() == 405 {
            OnvifError::Unsupported
        } else {
            OnvifError::Protocol
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SOAP_RESPONSE_BYTES as u64)
    {
        return Err(OnvifError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((MAX_SOAP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| OnvifError::DeviceUnreachable)?;
    if bytes.len() > MAX_SOAP_RESPONSE_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    Ok(bytes)
}

fn anonymous_soap_envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn soap_envelope(credentials: &OnvifCredentials, body: &str) -> String {
    let nonce = Uuid::new_v4().as_bytes().to_vec();
    let created = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut digest_input = nonce.clone();
    digest_input.extend_from_slice(created.as_bytes());
    digest_input.extend_from_slice(credentials.password.as_bytes());
    let digest = Sha1::digest(&digest_input);
    let digest = base64::engine::general_purpose::STANDARD.encode(digest);
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce);
    let username = xml_escape(&credentials.username);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
<s:Header><wsse:Security s:mustUnderstand="1"><wsse:UsernameToken><wsse:Username>{username}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>
<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn xml_escape(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

#[derive(Debug)]
struct DigestChallenge {
    realm: String,
    nonce: String,
    qop: Option<String>,
    algorithm: String,
    opaque: Option<String>,
}

fn parse_digest_challenge(raw: &str) -> Option<DigestChallenge> {
    let raw = raw.trim();
    let rest = raw
        .strip_prefix("Digest ")
        .or_else(|| raw.strip_prefix("digest "))?;
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut algorithm = None;
    let mut opaque = None;
    for part in split_digest_parts(rest) {
        let (key, value) = part.split_once('=')?;
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(value),
            "nonce" => nonce = Some(value),
            "qop" => qop = Some(value),
            "algorithm" => algorithm = Some(value),
            "opaque" => opaque = Some(value),
            _ => {}
        }
    }
    Some(DigestChallenge {
        realm: realm?,
        nonce: nonce?,
        qop,
        algorithm: algorithm.unwrap_or_else(|| "MD5".to_owned()),
        opaque,
    })
}

fn split_digest_parts(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (index, ch) in raw.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch == ',' && !quoted {
            parts.push(raw[start..index].trim());
            start = index + 1;
        }
    }
    if start < raw.len() {
        parts.push(raw[start..].trim());
    }
    parts
}

fn digest_authorization(
    credentials: &OnvifCredentials,
    url: &Url,
    challenge: &DigestChallenge,
) -> Result<String, OnvifError> {
    let mut uri = url.path().to_owned();
    if uri.is_empty() {
        uri.push('/');
    }
    if let Some(query) = url.query() {
        uri.push('?');
        uri.push_str(query);
    }
    let qop = challenge.qop.as_deref().and_then(|qops| {
        qops.split(',')
            .map(str::trim)
            .find(|candidate| candidate.eq_ignore_ascii_case("auth"))
    });
    if challenge.qop.is_some() && qop.is_none() {
        return Err(OnvifError::Unsupported);
    }
    let cnonce = Uuid::new_v4().simple().to_string();
    let nc = "00000001";
    let ha1 = digest_hex(
        &challenge.algorithm,
        &format!(
            "{}:{}:{}",
            credentials.username, challenge.realm, credentials.password
        ),
    )?;
    let ha2 = digest_hex(&challenge.algorithm, &format!("POST:{uri}"))?;
    let response = if let Some(qop) = qop {
        digest_hex(
            &challenge.algorithm,
            &format!("{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}", challenge.nonce),
        )?
    } else {
        digest_hex(
            &challenge.algorithm,
            &format!("{ha1}:{}:{ha2}", challenge.nonce),
        )?
    };
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm={}",
        credentials.username, challenge.realm, challenge.nonce, uri, response, challenge.algorithm
    );
    if let Some(qop) = qop {
        header.push_str(&format!(", qop={qop}, nc={nc}, cnonce=\"{cnonce}\""));
    }
    if let Some(opaque) = &challenge.opaque {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    Ok(header)
}

fn digest_hex(algorithm: &str, input: &str) -> Result<String, OnvifError> {
    let bytes = match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => Md5::digest(input.as_bytes()).to_vec(),
        "SHA-256" => Sha256::digest(input.as_bytes()).to_vec(),
        _ => return Err(OnvifError::Unsupported),
    };
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").map_err(|_| OnvifError::Internal)?;
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_endpoint_selection_prefers_https_and_skips_invalid_candidates() {
        let services = vec![
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "ftp://192.168.1.8/media2".into(),
            },
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "http://192.168.1.8/media2".into(),
            },
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "https://192.168.1.8/media2-secure".into(),
            },
        ];
        let selected =
            preferred_service_xaddr(&services, "/ver20/media/wsdl", "http://192.168.1.8/device")
                .unwrap();
        assert_eq!(selected, "https://192.168.1.8/media2-secure");
    }

    #[test]
    fn ranking_prefers_h264_then_resolution_then_token() {
        let mut profiles = [
            MediaProfile {
                token: "hevc".into(),
                name: None,
                video_codec: Some("H265".into()),
                width: Some(3840),
                height: Some(2160),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
            MediaProfile {
                token: "b".into(),
                name: None,
                video_codec: Some("H264".into()),
                width: Some(1280),
                height: Some(720),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
            MediaProfile {
                token: "a".into(),
                name: None,
                video_codec: Some("H264".into()),
                width: Some(1920),
                height: Some(1080),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
        ];
        profiles.sort_by(|left, right| {
            right
                .is_h264_compatible()
                .cmp(&left.is_h264_compatible())
                .then_with(|| profile_pixels(right).cmp(&profile_pixels(left)))
                .then_with(|| left.token.cmp(&right.token))
        });
        assert_eq!(profiles[0].token, "a");
        assert_eq!(profiles[1].token, "b");
    }

    #[test]
    fn wsse_envelope_never_contains_plaintext_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        let body = soap_envelope(&credentials, "<tds:GetDeviceInformation/>");
        assert!(!body.contains("SENTINEL-password"));
        assert!(body.contains("PasswordDigest"));
    }

    #[test]
    fn digest_authorization_does_not_echo_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        let challenge = parse_digest_challenge(
            "Digest realm=\"cam\", nonce=\"abc\", qop=\"auth\", algorithm=MD5",
        )
        .unwrap();
        let header = digest_authorization(
            &credentials,
            &Url::parse("http://127.0.0.1/onvif").unwrap(),
            &challenge,
        )
        .unwrap();
        assert!(header.starts_with("Digest "));
        assert!(!header.contains("SENTINEL-password"));
    }

    #[test]
    fn digest_parser_finds_digest_when_basic_is_advertised_first() {
        let challenge = parse_digest_challenge_value(
            "Basic realm=\"camera\", Digest realm=\"camera\", nonce=\"abc\", qop=\"auth\", algorithm=SHA-256",
        )
        .unwrap();
        assert_eq!(challenge.realm, "camera");
        assert_eq!(challenge.nonce, "abc");
        assert_eq!(challenge.algorithm, "SHA-256");
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        assert!(!format!("{credentials:?}").contains("SENTINEL-password"));
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 2048];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if header_end.is_none()
                && let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let end = position + 4;
                let headers = String::from_utf8_lossy(&bytes[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                header_end = Some(end);
            }
            if let Some(end) = header_end
                && bytes.len() >= end + content_length
            {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn write_http_response(
        stream: &mut std::net::TcpStream,
        status: &str,
        headers: &[(&str, String)],
        body: &str,
    ) {
        use std::io::Write as _;
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        response.push_str(body);
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn local_fixture_interrogates_media2_and_sanitizes_stream_userinfo() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let body = if request.contains("GetDeviceInformation") {
                    "<Envelope><Body><GetDeviceInformationResponse><Manufacturer>Fixture Corp</Manufacturer><Model>Fixture Cam</Model><FirmwareVersion>1.0</FirmwareVersion><SerialNumber>abc</SerialNumber><HardwareId>hw</HardwareId></GetDeviceInformationResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetHostname") {
                    "<Envelope><Body><GetHostnameResponse><HostnameInformation><Name>front-fixture</Name></HostnameInformation></GetHostnameResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetServices") {
                    format!(
                        "<Envelope><Body><GetServicesResponse><Service><Namespace>{MEDIA2_NS}</Namespace><XAddr>http://{address}/media2</XAddr></Service></GetServicesResponse></Body></Envelope>"
                    )
                } else if request.contains("GetProfiles") {
                    "<Envelope><Body><GetProfilesResponse><Profiles token=\"main\"><Name>Main</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1920</Width><Height>1080</Height></Resolution><RateControl><FrameRateLimit>25</FrameRateLimit><BitrateLimit>4096</BitrateLimit></RateControl></VideoEncoderConfiguration><AudioEncoderConfiguration><Encoding>AAC</Encoding></AudioEncoderConfiguration></Profiles><Profiles token=\"hevc\"><Name>HEVC</Name><VideoEncoderConfiguration><Encoding>H265</Encoding><Resolution><Width>3840</Width><Height>2160</Height></Resolution></VideoEncoderConfiguration></Profiles></GetProfilesResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetStreamUri") {
                    "<Envelope><Body><GetStreamUriResponse><Uri>rtsp://admin:SENTINEL-fixture-password@127.0.0.1:8554/live/main?x=1&amp;y=2</Uri></GetStreamUriResponse></Body></Envelope>".to_owned()
                } else {
                    panic!("unexpected fixture request")
                };
                write_http_response(&mut stream, "200 OK", &[], &body);
                requests.push(request);
            }
            requests
        });

        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-fixture-password".into(),
        };
        let client = OnvifClient::new().unwrap();
        let device_service = format!("http://{address}/onvif/device_service");
        let interrogation = client.interrogate(&device_service, &credentials).unwrap();
        assert_eq!(interrogation.device.model.as_deref(), Some("Fixture Cam"));
        assert_eq!(
            interrogation.device.hostname.as_deref(),
            Some("front-fixture")
        );
        assert_eq!(interrogation.media_service_kind, MediaServiceKind::Media2);
        assert_eq!(interrogation.profiles.len(), 2);
        assert!(interrogation.profiles[0].is_h264_compatible());
        let endpoint = client
            .stream_endpoint(
                &device_service,
                &interrogation.media_service,
                &credentials,
                &interrogation.profiles[0],
            )
            .unwrap();
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8554);
        assert_eq!(endpoint.path, "/live/main?x=1&y=2");
        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("SENTINEL-fixture-password"))
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("PasswordDigest"))
        );
    }

    #[test]
    fn media2_protocol_failure_falls_back_to_legacy_media_profiles() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                if request.contains("GetDeviceInformation") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Manufacturer>Fixture</Manufacturer><Model>Cam</Model></Envelope>",
                    );
                } else if request.contains("GetHostname") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Name>fixture</Name></Envelope>",
                    );
                } else if request.contains("GetServices") {
                    let body = format!(
                        "<Envelope><Service><Namespace>{MEDIA2_NS}</Namespace><XAddr>http://{address}/media2</XAddr></Service><Service><Namespace>{MEDIA1_NS}</Namespace><XAddr>http://{address}/media1</XAddr></Service></Envelope>"
                    );
                    write_http_response(&mut stream, "200 OK", &[], &body);
                } else if request.contains("POST /media2") {
                    write_http_response(&mut stream, "500 Internal Server Error", &[], "");
                } else if request.contains("POST /media1") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Profiles token=\"legacy-main\"><Name>Legacy Main</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1280</Width><Height>720</Height></Resolution></VideoEncoderConfiguration></Profiles></Envelope>",
                    );
                } else {
                    panic!("unexpected fallback fixture request: {request}");
                }
            }
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let interrogation = client
            .interrogate(&format!("http://{address}/device"), &credentials)
            .unwrap();
        assert_eq!(
            interrogation.media_service_kind,
            MediaServiceKind::LegacyMedia
        );
        assert_eq!(interrogation.profiles[0].token, "legacy-main");
        server.join().unwrap();
    }

    #[test]
    fn digest_challenge_retries_once_without_plaintext_password() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_http_request(&mut first_stream);
            write_http_response(
                &mut first_stream,
                "401 Unauthorized",
                &[(
                    WWW_AUTHENTICATE.as_str(),
                    "Digest realm=\"fixture\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5"
                        .to_owned(),
                )],
                "",
            );
            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_http_request(&mut second_stream);
            write_http_response(
                &mut second_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );
            (first, second)
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-digest-password".into(),
        };
        let response = client
            .soap(
                &format!("http://{address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();
        assert!(!response.is_empty());
        let (first, second) = server.join().unwrap();
        assert!(!first.contains("SENTINEL-digest-password"));
        assert!(!second.contains("SENTINEL-digest-password"));
        assert!(!first.contains("PasswordDigest"));
        assert!(!second.contains("PasswordDigest"));
        assert!(!first.to_ascii_lowercase().contains("authorization: digest"));
        assert!(
            second
                .to_ascii_lowercase()
                .contains("authorization: digest")
        );
    }

    #[test]
    fn username_token_is_legacy_fallback_and_cached_without_storing_the_secret() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_http_request(&mut first_stream);
            write_http_response(&mut first_stream, "401 Unauthorized", &[], "");

            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_http_request(&mut second_stream);
            write_http_response(
                &mut second_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );

            let (mut third_stream, _) = listener.accept().unwrap();
            let third = read_http_request(&mut third_stream);
            write_http_response(
                &mut third_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );
            (first, second, third)
        });

        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-username-token-password".into(),
        };
        let url = format!("http://{address}/device");
        client
            .soap(
                &url,
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();
        client
            .soap(
                &url,
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();

        let (first, second, third) = server.join().unwrap();
        assert!(!first.contains("PasswordDigest"));
        assert!(second.contains("PasswordDigest"));
        assert!(third.contains("PasswordDigest"));
        for request in [&first, &second, &third] {
            assert!(!request.contains("SENTINEL-username-token-password"));
        }
    }

    #[test]
    fn bare_unauthorized_response_maps_to_auth_failed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_http_request(&mut stream);
                write_http_response(&mut stream, "401 Unauthorized", &[], "");
            }
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        assert_eq!(
            client.soap(
                &format!("http://{address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            ),
            Err(OnvifError::AuthFailed)
        );
        server.join().unwrap();
    }

    #[test]
    fn redirects_are_not_followed_with_authenticated_requests() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_address = target.local_addr().unwrap();
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "302 Found",
                &[("Location", format!("http://{target_address}/capture"))],
                "",
            );
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        assert_eq!(
            client.soap(
                &format!("http://{source_address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            ),
            Err(OnvifError::AuthorityRejected)
        );
        server.join().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(target.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
