use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use if_addrs::get_if_addrs;
use uuid::Uuid;

use crate::authority::validate_discovery_xaddr;
use crate::xml::parse_probe_matches;
use crate::{
    DEFAULT_DISCOVERY_TIMEOUT_MS, DiscoveredDevice, MAX_DISCOVERED_DEVICES,
    MAX_DISCOVERY_DATAGRAM_BYTES, MAX_SCOPES_PER_DEVICE, MAX_XADDRS_PER_DEVICE, OnvifError,
};

const WS_DISCOVERY_TARGET: &str = "239.255.255.250:3702";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryConfig {
    pub timeout: Duration,
    pub max_devices: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(DEFAULT_DISCOVERY_TIMEOUT_MS),
            max_devices: MAX_DISCOVERED_DEVICES,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DiscoveryScanner;

impl DiscoveryScanner {
    pub fn scan(
        &self,
        config: DiscoveryConfig,
        cancel: &AtomicBool,
    ) -> Result<Vec<DiscoveredDevice>, OnvifError> {
        if cancel.load(Ordering::Acquire) {
            return Err(OnvifError::Cancelled);
        }
        let timeout = config
            .timeout
            .clamp(Duration::from_millis(100), Duration::from_secs(10));
        let max_devices = config.max_devices.clamp(1, MAX_DISCOVERED_DEVICES);
        let message = probe_message();
        let sockets = discovery_sockets()?;
        for socket in &sockets {
            let _ = socket.send_to(message.as_bytes(), WS_DISCOVERY_TARGET);
        }

        collect_responses(&sockets, timeout, max_devices, cancel)
    }
}

fn collect_responses(
    sockets: &[UdpSocket],
    timeout: Duration,
    max_devices: usize,
    cancel: &AtomicBool,
) -> Result<Vec<DiscoveredDevice>, OnvifError> {
    let deadline = Instant::now() + timeout;
    let mut devices: BTreeMap<String, DiscoveredDevice> = BTreeMap::new();
    let mut buffer = vec![0u8; MAX_DISCOVERY_DATAGRAM_BYTES];

    while Instant::now() < deadline && devices.len() < max_devices {
        if cancel.load(Ordering::Acquire) {
            return Err(OnvifError::Cancelled);
        }
        let mut received_any = false;
        for socket in sockets {
            if cancel.load(Ordering::Acquire) {
                return Err(OnvifError::Cancelled);
            }
            match socket.recv_from(&mut buffer) {
                Ok((size, source)) => {
                    received_any = true;
                    if size == 0 || size > MAX_DISCOVERY_DATAGRAM_BYTES {
                        continue;
                    }
                    ingest_datagram(&mut devices, &buffer[..size], source.ip(), max_devices);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {}
            }
        }
        if !received_any {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    Ok(devices.into_values().take(max_devices).collect())
}

fn ingest_datagram(
    devices: &mut BTreeMap<String, DiscoveredDevice>,
    datagram: &[u8],
    source: IpAddr,
    max_devices: usize,
) {
    let Ok(matches) = parse_probe_matches(datagram) else {
        return;
    };
    for probe_match in matches {
        if devices.len() >= max_devices {
            break;
        }
        if !probe_match.is_network_video_transmitter {
            continue;
        }
        let mut safe_xaddrs = Vec::new();
        for xaddr in probe_match.xaddrs {
            if safe_xaddrs.len() >= MAX_XADDRS_PER_DEVICE {
                break;
            }
            if let Ok(xaddr) = validate_discovery_xaddr(&xaddr, source)
                && !safe_xaddrs.contains(&xaddr)
            {
                safe_xaddrs.push(xaddr);
            }
        }
        if safe_xaddrs.is_empty() {
            continue;
        }
        let key = probe_match.endpoint_reference.clone();
        let entry = devices
            .entry(key.clone())
            .or_insert_with(|| DiscoveredDevice {
                endpoint_reference: key,
                xaddrs: Vec::new(),
                scopes: Vec::new(),
                network_address: source.to_string(),
            });
        for xaddr in safe_xaddrs {
            if entry.xaddrs.len() < MAX_XADDRS_PER_DEVICE && !entry.xaddrs.contains(&xaddr) {
                entry.xaddrs.push(xaddr);
            }
        }
        for scope in probe_match.scopes {
            if entry.scopes.len() < MAX_SCOPES_PER_DEVICE && !entry.scopes.contains(&scope) {
                entry.scopes.push(scope);
            }
        }
    }
}

fn discovery_sockets() -> Result<Vec<UdpSocket>, OnvifError> {
    let mut ips = Vec::new();
    if let Ok(interfaces) = get_if_addrs() {
        for interface in interfaces {
            if let IpAddr::V4(ip) = interface.ip()
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !ips.contains(&ip)
            {
                ips.push(ip);
            }
        }
    }
    if ips.is_empty() {
        ips.push(Ipv4Addr::UNSPECIFIED);
    }

    let mut sockets = Vec::new();
    for ip in ips {
        let Ok(socket) = UdpSocket::bind(SocketAddr::new(IpAddr::V4(ip), 0)) else {
            continue;
        };
        let _ = socket.set_multicast_ttl_v4(1);
        let _ = socket.set_multicast_loop_v4(false);
        if socket
            .set_read_timeout(Some(Duration::from_millis(60)))
            .is_ok()
        {
            sockets.push(socket);
        }
    }
    if sockets.is_empty() {
        return Err(OnvifError::DeviceUnreachable);
    }
    Ok(sockets)
}

fn probe_message() -> String {
    let message_id = Uuid::new_v4();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header>
    <w:MessageID>urn:uuid:{message_id}</w:MessageID>
    <w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
    <w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
  </e:Header>
  <e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProbeMatch;

    fn merge_fixture(
        devices: &mut BTreeMap<String, DiscoveredDevice>,
        source: IpAddr,
        fixture: ProbeMatch,
    ) {
        let safe: Vec<_> = fixture
            .xaddrs
            .iter()
            .filter_map(|xaddr| validate_discovery_xaddr(xaddr, source).ok())
            .collect();
        if safe.is_empty() {
            return;
        }
        let entry = devices
            .entry(fixture.endpoint_reference.clone())
            .or_insert(DiscoveredDevice {
                endpoint_reference: fixture.endpoint_reference,
                xaddrs: Vec::new(),
                scopes: Vec::new(),
                network_address: source.to_string(),
            });
        for xaddr in safe {
            if !entry.xaddrs.contains(&xaddr) {
                entry.xaddrs.push(xaddr);
            }
        }
        for scope in fixture.scopes {
            if !entry.scopes.contains(&scope) {
                entry.scopes.push(scope);
            }
        }
    }

    #[test]
    fn duplicate_identity_merges_addresses_and_scopes() {
        let source: IpAddr = "192.168.1.8".parse().unwrap();
        let mut devices = BTreeMap::new();
        merge_fixture(
            &mut devices,
            source,
            ProbeMatch {
                endpoint_reference: "urn:uuid:one".to_owned(),
                xaddrs: vec!["http://192.168.1.8/onvif/device_service".to_owned()],
                scopes: vec!["scope:a".to_owned()],
                is_network_video_transmitter: true,
            },
        );
        merge_fixture(
            &mut devices,
            source,
            ProbeMatch {
                endpoint_reference: "urn:uuid:one".to_owned(),
                xaddrs: vec!["http://camera.local/onvif/device_service".to_owned()],
                scopes: vec!["scope:b".to_owned()],
                is_network_video_transmitter: true,
            },
        );
        assert_eq!(devices.len(), 1);
        let device = devices.values().next().unwrap();
        assert_eq!(device.xaddrs.len(), 2);
        assert_eq!(device.scopes.len(), 2);
    }

    #[test]
    fn malformed_datagram_is_ignored_without_mutating_results() {
        let mut devices = BTreeMap::new();
        ingest_datagram(
            &mut devices,
            b"<not-valid-xml",
            "192.168.1.8".parse().unwrap(),
            MAX_DISCOVERED_DEVICES,
        );
        assert!(devices.is_empty());
    }

    #[test]
    fn one_datagram_can_add_multiple_onvif_devices() {
        let xml = br#"<Envelope><ProbeMatches><ProbeMatch><EndpointReference><Address>urn:uuid:one</Address></EndpointReference><Types>dn:NetworkVideoTransmitter</Types><XAddrs>http://192.168.1.8/onvif/device_service</XAddrs></ProbeMatch><ProbeMatch><EndpointReference><Address>urn:uuid:two</Address></EndpointReference><Types>dn:NetworkVideoTransmitter</Types><XAddrs>http://192.168.1.8/onvif/device2</XAddrs></ProbeMatch></ProbeMatches></Envelope>"#;
        let mut devices = BTreeMap::new();
        ingest_datagram(
            &mut devices,
            xml,
            "192.168.1.8".parse().unwrap(),
            MAX_DISCOVERED_DEVICES,
        );
        assert_eq!(devices.len(), 2);
        assert!(devices.contains_key("urn:uuid:one"));
        assert!(devices.contains_key("urn:uuid:two"));
    }

    #[test]
    fn response_collection_times_out_to_an_empty_result_without_sockets() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let devices = collect_responses(&[], Duration::from_millis(15), 8, &cancel).unwrap();
        assert!(devices.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(10));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn pre_cancelled_scan_exits_without_waiting_for_timeout() {
        let cancel = AtomicBool::new(true);
        assert_eq!(
            DiscoveryScanner.scan(DiscoveryConfig::default(), &cancel),
            Err(OnvifError::Cancelled)
        );
    }
}
