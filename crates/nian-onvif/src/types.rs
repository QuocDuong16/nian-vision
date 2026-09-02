use serde::Serialize;

#[derive(Clone, PartialEq, Eq)]
pub struct OnvifCredentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for OnvifCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnvifCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub endpoint_reference: String,
    pub xaddrs: Vec<String>,
    pub scopes: Vec<String>,
    pub network_address: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DeviceInformation {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub hardware_id: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaServiceKind {
    Media2,
    LegacyMedia,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MediaProfile {
    pub token: String,
    pub name: Option<String>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub framerate: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub audio_codec: Option<String>,
    pub service_kind: MediaServiceKind,
}

impl MediaProfile {
    pub fn is_h264_compatible(&self) -> bool {
        self.video_codec.as_deref().is_some_and(|codec| {
            codec.eq_ignore_ascii_case("H264") || codec.eq_ignore_ascii_case("H.264")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnvifInterrogation {
    pub device: DeviceInformation,
    pub media_service: String,
    pub media_service_kind: MediaServiceKind,
    pub profiles: Vec<MediaProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEndpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub host_mismatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceEndpoint {
    pub namespace: String,
    pub xaddr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeMatch {
    pub endpoint_reference: String,
    pub xaddrs: Vec<String>,
    pub scopes: Vec<String>,
    pub is_network_video_transmitter: bool,
}
