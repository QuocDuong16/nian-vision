use std::net::IpAddr;

use url::{Host, Url};

use crate::{MAX_URL_BYTES, OnvifError, StreamEndpoint};

fn host_string(url: &Url) -> Result<String, OnvifError> {
    match url.host().ok_or(OnvifError::AuthorityRejected)? {
        Host::Domain(host) => Ok(host.to_owned()),
        Host::Ipv4(host) => Ok(host.to_string()),
        Host::Ipv6(host) => Ok(host.to_string()),
    }
}

pub(crate) fn validate_discovery_xaddr(
    candidate: &str,
    responder: IpAddr,
) -> Result<String, OnvifError> {
    if candidate.len() > MAX_URL_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    let url = Url::parse(candidate).map_err(|_| OnvifError::Protocol)?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return Err(OnvifError::AuthorityRejected);
    }
    if url.fragment().is_some() || url.port_or_known_default().is_none() {
        return Err(OnvifError::AuthorityRejected);
    }
    let host = host_string(&url)?;
    // M10 accepts only an IP literal that is exactly the UDP responder.
    // Hostname aliases require an explicit bounded equivalence proof and are
    // rejected rather than trusted merely because they look local.
    if !host.parse::<IpAddr>().is_ok_and(|ip| ip == responder) {
        return Err(OnvifError::AuthorityRejected);
    }
    Ok(url.to_string())
}

pub(crate) fn validate_service_xaddr(
    candidate: &str,
    device_service: &str,
) -> Result<String, OnvifError> {
    if candidate.len() > MAX_URL_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    let candidate = Url::parse(candidate).map_err(|_| OnvifError::Protocol)?;
    let device = Url::parse(device_service).map_err(|_| OnvifError::Protocol)?;
    if !matches!(candidate.scheme(), "http" | "https")
        || candidate.username() != ""
        || candidate.password().is_some()
        || candidate.fragment().is_some()
    {
        return Err(OnvifError::AuthorityRejected);
    }
    let candidate_host = host_string(&candidate)?;
    let device_host = host_string(&device)?;
    if !candidate_host.eq_ignore_ascii_case(&device_host) {
        return Err(OnvifError::AuthorityRejected);
    }
    Ok(candidate.to_string())
}

/// Event-service and PullPoint endpoints are stricter than the older generic
/// service policy: no query/fragment/userinfo, same physical device host, and
/// only HTTP(S). Different ports/paths remain interoperable.
pub(crate) fn validate_event_xaddr(
    candidate: &str,
    device_service: &str,
) -> Result<String, OnvifError> {
    let normalized = validate_service_xaddr(candidate, device_service)?;
    let url = Url::parse(&normalized).map_err(|_| OnvifError::Protocol)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(OnvifError::AuthorityRejected);
    }
    Ok(normalized)
}

pub(crate) fn parse_stream_uri(
    raw: &str,
    device_service: &str,
) -> Result<StreamEndpoint, OnvifError> {
    if raw.len() > MAX_URL_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    let mut url = Url::parse(raw).map_err(|_| OnvifError::InvalidStreamUri)?;
    if !url.scheme().eq_ignore_ascii_case("rtsp") || url.fragment().is_some() {
        return Err(OnvifError::InvalidStreamUri);
    }
    // Strip credentials before constructing any returned value or error detail.
    url.set_username("")
        .map_err(|_| OnvifError::InvalidStreamUri)?;
    url.set_password(None)
        .map_err(|_| OnvifError::InvalidStreamUri)?;
    let host = host_string(&url).map_err(|_| OnvifError::InvalidStreamUri)?;
    let port = url.port_or_known_default().unwrap_or(554);
    if url.query().is_some() {
        return Err(OnvifError::InvalidStreamUri);
    }
    let path = url.path().to_owned();
    if !path.starts_with('/')
        || path.len() > 4096
        || path
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '@')
    {
        return Err(OnvifError::InvalidStreamUri);
    }

    let device = Url::parse(device_service).map_err(|_| OnvifError::InvalidStreamUri)?;
    let device_host = host_string(&device).map_err(|_| OnvifError::InvalidStreamUri)?;
    let host_mismatch = !host.eq_ignore_ascii_case(&device_host);
    if host_mismatch {
        return Err(OnvifError::AuthorityRejected);
    }

    Ok(StreamEndpoint {
        host,
        port,
        path,
        host_mismatch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_rejects_non_http_and_userinfo() {
        let responder: IpAddr = "192.168.1.5".parse().unwrap();
        assert_eq!(
            validate_discovery_xaddr("ftp://192.168.1.5/device", responder),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_discovery_xaddr("http://admin:secret@192.168.1.5/device", responder),
            Err(OnvifError::AuthorityRejected)
        );
    }

    #[test]
    fn discovery_requires_exact_responder_ip_and_rejects_hostname_aliases() {
        let responder: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(
            validate_discovery_xaddr("http://192.168.1.20/onvif/device_service", responder).is_ok()
        );
        assert_eq!(
            validate_discovery_xaddr("http://192.168.1.90/onvif/device_service", responder),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_discovery_xaddr("http://camera.local/onvif/device_service", responder),
            Err(OnvifError::AuthorityRejected)
        );
    }

    #[test]
    fn service_authority_requires_same_host_but_allows_different_port_and_path() {
        assert!(
            validate_service_xaddr(
                "https://192.168.1.20:8443/onvif/media2",
                "http://192.168.1.20/onvif/device_service"
            )
            .is_ok()
        );
        assert_eq!(
            validate_service_xaddr(
                "http://192.168.1.90/onvif/media",
                "http://192.168.1.20/onvif/device_service"
            ),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_service_xaddr(
                "http://camera.local/onvif/media",
                "http://192.168.1.20/onvif/device_service"
            ),
            Err(OnvifError::AuthorityRejected)
        );
    }

    #[test]
    fn event_and_pullpoint_authority_require_same_host_and_reject_query_or_userinfo() {
        let device = "http://192.168.1.20/onvif/device_service";
        assert_eq!(
            validate_event_xaddr("https://192.168.1.20:8443/onvif/events", device),
            Ok("https://192.168.1.20:8443/onvif/events".to_owned())
        );
        assert_eq!(
            validate_event_xaddr("http://192.168.1.90/onvif/events", device),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_event_xaddr("http://admin:secret@192.168.1.20/onvif/events", device),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_event_xaddr("http://192.168.1.20/onvif/pull?token=opaque", device),
            Err(OnvifError::AuthorityRejected)
        );
        assert_eq!(
            validate_event_xaddr("ftp://192.168.1.20/onvif/pull", device),
            Err(OnvifError::AuthorityRejected)
        );
    }

    #[test]
    fn stream_uri_strips_userinfo_and_preserves_safe_path() {
        let endpoint = parse_stream_uri(
            "rtsp://admin:secret@192.168.1.20:8554/live/main",
            "http://192.168.1.20/onvif/device_service",
        )
        .unwrap();
        assert_eq!(endpoint.host, "192.168.1.20");
        assert_eq!(endpoint.port, 8554);
        assert_eq!(endpoint.path, "/live/main");
        assert!(!endpoint.host_mismatch);
        assert!(!format!("{endpoint:?}").contains("secret"));
    }

    #[test]
    fn stream_uri_rejects_query_and_any_host_mismatch_without_leaking_secret() {
        let query_result = parse_stream_uri(
            "rtsp://192.168.1.20/live?opaque=SENTINEL-stream-token",
            "http://192.168.1.20/onvif/device_service",
        );
        assert_eq!(query_result, Err(OnvifError::InvalidStreamUri));
        assert!(!format!("{query_result:?}").contains("SENTINEL-stream-token"));
        assert_eq!(
            parse_stream_uri(
                "rtsp://192.168.1.90/live",
                "http://192.168.1.20/onvif/device_service"
            ),
            Err(OnvifError::AuthorityRejected)
        );
    }
}
