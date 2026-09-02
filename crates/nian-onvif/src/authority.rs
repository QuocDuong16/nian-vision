use std::net::IpAddr;

use url::{Host, Url};

use crate::{MAX_URL_BYTES, OnvifError, StreamEndpoint};

fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.is_unicast_link_local() || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

fn local_hostname(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok_and(is_local_ip)
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".local")
        || host.to_ascii_lowercase().ends_with(".lan")
}

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
    let responder_matches = host.parse::<IpAddr>().is_ok_and(|ip| ip == responder);
    if !responder_matches && !local_hostname(&host) {
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
    if !candidate_host.eq_ignore_ascii_case(&device_host) && !local_hostname(&candidate_host) {
        return Err(OnvifError::AuthorityRejected);
    }
    Ok(candidate.to_string())
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
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
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
    if host_mismatch && !local_hostname(&host) {
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
    fn stream_uri_strips_userinfo_and_preserves_port_path() {
        let endpoint = parse_stream_uri(
            "rtsp://admin:secret@192.168.1.9:8554/live/main?transport=tcp",
            "http://192.168.1.5/onvif/device_service",
        )
        .unwrap();
        assert_eq!(endpoint.host, "192.168.1.9");
        assert_eq!(endpoint.port, 8554);
        assert_eq!(endpoint.path, "/live/main?transport=tcp");
        assert!(endpoint.host_mismatch);
        assert!(!format!("{endpoint:?}").contains("secret"));
    }

    #[test]
    fn stream_uri_rejects_public_host_mismatch() {
        assert_eq!(
            parse_stream_uri(
                "rtsp://203.0.113.9/live",
                "http://192.168.1.5/onvif/device_service"
            ),
            Err(OnvifError::AuthorityRejected)
        );
    }
}
