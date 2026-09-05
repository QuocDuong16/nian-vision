use chrono::{DateTime, Utc};
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtzVelocityRange {
    pub min: f64,
    pub max: f64,
}

impl PtzVelocityRange {
    pub(crate) fn validate(self) -> Result<Self, crate::OnvifError> {
        if !self.min.is_finite() || !self.max.is_finite() || self.min > 0.0 || self.max < 0.0 {
            return Err(crate::OnvifError::Protocol);
        }
        Ok(self)
    }

    pub(crate) fn map_normalized(self, value: f64) -> Result<f64, crate::OnvifError> {
        if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
            return Err(crate::OnvifError::Protocol);
        }
        let mapped = if value >= 0.0 {
            value * self.max.max(0.0)
        } else {
            (-value) * self.min.min(0.0)
        };
        Ok(mapped.clamp(self.min, self.max))
    }
}

#[derive(Clone, PartialEq)]
pub struct PtzControl {
    pub(crate) service: String,
    pub(crate) profile_token: String,
    pub(crate) pan: Option<PtzVelocityRange>,
    pub(crate) tilt: Option<PtzVelocityRange>,
    pub(crate) zoom: Option<PtzVelocityRange>,
}

impl std::fmt::Debug for PtzControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtzControl")
            .field("pan_tilt_supported", &self.pan_tilt_supported())
            .field("zoom_supported", &self.zoom_supported())
            .finish_non_exhaustive()
    }
}

impl PtzControl {
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_fixture(zoom_supported: bool) -> Self {
        let unit = PtzVelocityRange {
            min: -1.0,
            max: 1.0,
        };
        Self {
            service: "http://127.0.0.1/ptz-fixture".to_owned(),
            profile_token: "ptz-fixture-profile".to_owned(),
            pan: Some(unit),
            tilt: Some(unit),
            zoom: zoom_supported.then_some(unit),
        }
    }

    pub fn pan_tilt_supported(&self) -> bool {
        self.pan.is_some() && self.tilt.is_some()
    }

    pub fn zoom_supported(&self) -> bool {
        self.zoom.is_some()
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct EventProperties {
    pub motion_supported: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EventControl {
    pub(crate) device_service: String,
    pub(crate) event_service: String,
    pub(crate) properties: EventProperties,
}

impl std::fmt::Debug for EventControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventControl")
            .field("motion_supported", &self.properties.motion_supported)
            .finish_non_exhaustive()
    }
}

impl EventControl {
    pub fn properties(&self) -> EventProperties {
        self.properties
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_fixture() -> Self {
        Self {
            device_service: "http://127.0.0.1/onvif/device_service".to_owned(),
            event_service: "http://127.0.0.1/onvif/events".to_owned(),
            properties: EventProperties {
                motion_supported: true,
            },
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PullPointSubscription {
    pub(crate) endpoint: String,
    pub(crate) current_time_utc: Option<DateTime<Utc>>,
    pub(crate) termination_time_utc: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for PullPointSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullPointSubscription")
            .field("current_time_utc", &self.current_time_utc)
            .field("termination_time_utc", &self.termination_time_utc)
            .finish_non_exhaustive()
    }
}

impl PullPointSubscription {
    pub fn current_time_utc(&self) -> Option<DateTime<Utc>> {
        self.current_time_utc
    }

    pub fn termination_time_utc(&self) -> Option<DateTime<Utc>> {
        self.termination_time_utc
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn test_fixture(lifetime_secs: i64) -> Self {
        let current = Utc::now();
        Self {
            endpoint: "http://127.0.0.1/onvif/pullpoint-fixture".to_owned(),
            current_time_utc: Some(current),
            termination_time_utc: Some(current + chrono::Duration::seconds(lifetime_secs)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionNotification {
    pub active: bool,
    pub device_time_utc: Option<DateTime<Utc>>,
    /// SHA-256 of bounded canonical Source SimpleItems. Raw source tokens never
    /// escape the protocol crate.
    pub source_key: Option<String>,
    /// `Initialized` synchronization messages establish baseline state and are
    /// never interpreted as historical transitions.
    pub synchronization_baseline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PtzProfileAssociation {
    pub profile_token: String,
    pub configuration_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct PtzConfigurationOptions {
    pub pan: Option<PtzVelocityRange>,
    pub tilt: Option<PtzVelocityRange>,
    pub zoom: Option<PtzVelocityRange>,
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
