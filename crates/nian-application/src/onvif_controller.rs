//! Application-owned ONVIF onboarding sessions.
//!
//! The frontend receives only random session/device handles and safe metadata.
//! Discovery XAddrs, authenticated service endpoints and credentials remain in
//! Rust until a validated RTSP `CameraDraft` is handed to `CameraService`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nian_domain::{AudioPolicy, Credentials};
use nian_onvif::{
    DiscoveredDevice, DiscoveryConfig, DiscoveryScanner, MediaProfile, OnvifClient,
    OnvifCredentials, OnvifError, OnvifInterrogation, PtzControl, StreamEndpoint,
};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::CameraDraft;

trait DiscoveryBackend: Send + Sync {
    fn discover(&self, cancel: &AtomicBool) -> Result<Vec<DiscoveredDevice>, OnvifError>;
}

#[derive(Debug, Default)]
struct ProductionDiscovery;

impl DiscoveryBackend for ProductionDiscovery {
    fn discover(&self, cancel: &AtomicBool) -> Result<Vec<DiscoveredDevice>, OnvifError> {
        DiscoveryScanner.scan(DiscoveryConfig::default(), cancel)
    }
}

trait DeviceBackend: Send + Sync {
    fn interrogate(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<OnvifInterrogation, OnvifError>;

    fn stream_endpoint(
        &self,
        device_service: &str,
        media_service: &str,
        credentials: &OnvifCredentials,
        profile: &MediaProfile,
    ) -> Result<StreamEndpoint, OnvifError>;

    fn ptz_control(
        &self,
        _device_service: &str,
        _credentials: &OnvifCredentials,
    ) -> Result<PtzControl, OnvifError> {
        Err(OnvifError::Unsupported)
    }
}

impl DeviceBackend for OnvifClient {
    fn interrogate(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<OnvifInterrogation, OnvifError> {
        OnvifClient::interrogate(self, device_service, credentials)
    }

    fn stream_endpoint(
        &self,
        device_service: &str,
        media_service: &str,
        credentials: &OnvifCredentials,
        profile: &MediaProfile,
    ) -> Result<StreamEndpoint, OnvifError> {
        OnvifClient::stream_endpoint(self, device_service, media_service, credentials, profile)
    }

    fn ptz_control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<PtzControl, OnvifError> {
        OnvifClient::ptz_control(self, device_service, credentials)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnvifDiscoveredDeviceDto {
    pub device_id: String,
    pub endpoint_reference: String,
    pub label: String,
    pub network_address: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnvifDiscoveryDto {
    pub session_id: String,
    pub devices: Vec<OnvifDiscoveredDeviceDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnvifMediaProfileDto {
    pub token: String,
    pub name: Option<String>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub framerate: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub audio_codec: Option<String>,
    pub supported: bool,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnvifConnectionDto {
    pub session_id: String,
    pub device_id: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub hostname: Option<String>,
    pub profiles: Vec<OnvifMediaProfileDto>,
    pub proposed_camera_id: String,
    pub proposed_display_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OnvifPreparedProfileDto {
    pub session_id: String,
    pub device_id: String,
    pub profile_token: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub host_mismatch: bool,
}

#[derive(Debug, Error)]
pub enum OnvifControllerError {
    #[error("ONVIF onboarding is not accepting new work")]
    NotAccepting,
    #[error("ONVIF discovery session expired")]
    SessionExpired,
    #[error("ONVIF discovered device handle expired")]
    DeviceExpired,
    #[error("ONVIF media profile was not found")]
    ProfileNotFound,
    #[error("ONVIF validation failed")]
    Validation,
    #[error(transparent)]
    Protocol(#[from] OnvifError),
    #[error("ONVIF internal state is unavailable")]
    Internal,
}

pub struct PreparedPtzPairing {
    pub(crate) device_service: String,
    pub(crate) endpoint_reference: String,
    pub(crate) credentials: Credentials,
    pub(crate) control: PtzControl,
}

impl std::fmt::Debug for PreparedPtzPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPtzPairing")
            .field("pan_tilt_supported", &self.control.pan_tilt_supported())
            .field("zoom_supported", &self.control.zoom_supported())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ConnectedDevice {
    connection_id: Uuid,
    credentials: Credentials,
    device_service: String,
    interrogation: OnvifInterrogation,
    prepared: Option<PreparedProfile>,
}

#[derive(Clone)]
struct PreparedProfile {
    token: String,
    endpoint: StreamEndpoint,
}

struct DiscoverySession {
    devices: HashMap<String, DiscoveredDevice>,
    connections: HashMap<String, ConnectedDevice>,
}

pub struct OnvifController {
    discovery: Arc<dyn DiscoveryBackend>,
    device: Arc<dyn DeviceBackend>,
    discovery_gate: Mutex<()>,
    active_discovery_cancel: Mutex<Option<Arc<AtomicBool>>>,
    accepting: AtomicBool,
    sessions: Mutex<HashMap<String, DiscoverySession>>,
}

impl std::fmt::Debug for OnvifController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnvifController").finish_non_exhaustive()
    }
}

impl OnvifController {
    pub fn production() -> Result<Self, OnvifControllerError> {
        Ok(Self {
            discovery: Arc::new(ProductionDiscovery),
            device: Arc::new(OnvifClient::new()?),
            discovery_gate: Mutex::new(()),
            active_discovery_cancel: Mutex::new(None),
            accepting: AtomicBool::new(true),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    fn with_backends(discovery: Arc<dyn DiscoveryBackend>, device: Arc<dyn DeviceBackend>) -> Self {
        Self {
            discovery,
            device,
            discovery_gate: Mutex::new(()),
            active_discovery_cancel: Mutex::new(None),
            accepting: AtomicBool::new(true),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn discover(&self) -> Result<OnvifDiscoveryDto, OnvifControllerError> {
        self.require_accepting()?;
        self.cancel_active_discovery()?;
        let _gate = self
            .discovery_gate
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        self.require_accepting()?;
        let cancel = Arc::new(AtomicBool::new(false));
        *self
            .active_discovery_cancel
            .lock()
            .map_err(|_| OnvifControllerError::Internal)? = Some(cancel.clone());

        let discovered = self.discovery.discover(&cancel);
        let mut active_discovery = self
            .active_discovery_cancel
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        if cancel.load(Ordering::Acquire) {
            *active_discovery = None;
            return Err(OnvifError::Cancelled.into());
        }
        let discovered = match discovered {
            Ok(discovered) => discovered,
            Err(error) => {
                *active_discovery = None;
                return Err(error.into());
            }
        };
        if let Err(error) = self.require_accepting() {
            *active_discovery = None;
            return Err(error);
        }

        let session_id = Uuid::new_v4().to_string();
        let mut internal = HashMap::new();
        let mut devices = Vec::new();
        for device in discovered {
            let device_id = Uuid::new_v4().to_string();
            devices.push(OnvifDiscoveredDeviceDto {
                device_id: device_id.clone(),
                endpoint_reference: device.endpoint_reference.clone(),
                label: discovery_label(&device),
                network_address: device.network_address.clone(),
            });
            internal.insert(device_id, device);
        }
        devices.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.endpoint_reference.cmp(&right.endpoint_reference))
        });

        let mut sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(_) => {
                *active_discovery = None;
                return Err(OnvifControllerError::Internal);
            }
        };
        // Refresh intentionally invalidates all previously returned handles.
        sessions.clear();
        sessions.insert(
            session_id.clone(),
            DiscoverySession {
                devices: internal,
                connections: HashMap::new(),
            },
        );
        *active_discovery = None;
        Ok(OnvifDiscoveryDto {
            session_id,
            devices,
        })
    }

    pub fn connect(
        &self,
        session_id: &str,
        device_id: &str,
        credentials: Credentials,
    ) -> Result<OnvifConnectionDto, OnvifControllerError> {
        self.require_accepting()?;
        credentials
            .validate()
            .map_err(|_| OnvifControllerError::Validation)?;
        let device = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| OnvifControllerError::Internal)?;
            let session = sessions
                .get(session_id)
                .ok_or(OnvifControllerError::SessionExpired)?;
            session
                .devices
                .get(device_id)
                .cloned()
                .ok_or(OnvifControllerError::DeviceExpired)?
        };
        let network_credentials = OnvifCredentials {
            username: credentials.username.clone(),
            password: credentials.password().to_owned(),
        };

        let mut last_error = OnvifError::DeviceUnreachable;
        let mut connected = None;
        let mut xaddrs = device.xaddrs.clone();
        xaddrs.sort_by(|left, right| {
            right
                .starts_with("https://")
                .cmp(&left.starts_with("https://"))
                .then_with(|| left.cmp(right))
        });
        for xaddr in &xaddrs {
            match self.device.interrogate(xaddr, &network_credentials) {
                Ok(interrogation) => {
                    connected = Some((xaddr.clone(), interrogation));
                    break;
                }
                Err(OnvifError::AuthFailed) => return Err(OnvifError::AuthFailed.into()),
                Err(error) => last_error = error,
            }
        }
        let (device_service, interrogation) = connected.ok_or(last_error)?;
        self.require_accepting()?;

        let proposed_display_name = interrogation
            .device
            .hostname
            .clone()
            .or_else(|| interrogation.device.model.clone())
            .unwrap_or_else(|| discovery_label(&device));
        let proposed_camera_id = format!("onvif-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let profiles = profile_dtos(&interrogation.profiles);

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(OnvifControllerError::SessionExpired)?;
        if !session.devices.contains_key(device_id) {
            return Err(OnvifControllerError::DeviceExpired);
        }
        session.connections.insert(
            device_id.to_owned(),
            ConnectedDevice {
                connection_id: Uuid::new_v4(),
                credentials,
                device_service,
                interrogation: interrogation.clone(),
                prepared: None,
            },
        );
        Ok(OnvifConnectionDto {
            session_id: session_id.to_owned(),
            device_id: device_id.to_owned(),
            manufacturer: interrogation.device.manufacturer,
            model: interrogation.device.model,
            firmware_version: interrogation.device.firmware_version,
            serial_number: interrogation.device.serial_number,
            hostname: interrogation.device.hostname,
            profiles,
            proposed_camera_id,
            proposed_display_name,
        })
    }

    pub fn prepare_profile(
        &self,
        session_id: &str,
        device_id: &str,
        profile_token: &str,
    ) -> Result<OnvifPreparedProfileDto, OnvifControllerError> {
        self.require_accepting()?;
        let connection = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| OnvifControllerError::Internal)?;
            sessions
                .get(session_id)
                .ok_or(OnvifControllerError::SessionExpired)?
                .connections
                .get(device_id)
                .cloned()
                .ok_or(OnvifControllerError::DeviceExpired)?
        };
        let profile = connection
            .interrogation
            .profiles
            .iter()
            .find(|profile| profile.token == profile_token)
            .cloned()
            .ok_or(OnvifControllerError::ProfileNotFound)?;
        if !profile.is_h264_compatible() {
            return Err(OnvifError::NoCompatibleProfile.into());
        }
        let network_credentials = OnvifCredentials {
            username: connection.credentials.username.clone(),
            password: connection.credentials.password().to_owned(),
        };
        let endpoint = self.device.stream_endpoint(
            &connection.device_service,
            &connection.interrogation.media_service,
            &network_credentials,
            &profile,
        )?;
        // Defense in depth: production `nian-onvif` rejects RTSP queries before
        // constructing a StreamEndpoint. Keep the application/UI persistence
        // boundary query-free even if a future backend regresses.
        if endpoint.path.contains('?') {
            return Err(OnvifError::InvalidStreamUri.into());
        }
        self.require_accepting()?;

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        let connection = sessions
            .get_mut(session_id)
            .ok_or(OnvifControllerError::SessionExpired)?
            .connections
            .get_mut(device_id)
            .ok_or(OnvifControllerError::DeviceExpired)?;
        connection.prepared = Some(PreparedProfile {
            token: profile_token.to_owned(),
            endpoint: endpoint.clone(),
        });
        Ok(OnvifPreparedProfileDto {
            session_id: session_id.to_owned(),
            device_id: device_id.to_owned(),
            profile_token: profile_token.to_owned(),
            host: endpoint.host,
            port: endpoint.port,
            path: endpoint.path,
            host_mismatch: endpoint.host_mismatch,
        })
    }

    pub fn prepare_ptz_pairing(
        &self,
        session_id: &str,
        device_id: &str,
    ) -> Result<PreparedPtzPairing, OnvifControllerError> {
        self.require_accepting()?;
        let (device, connection) = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| OnvifControllerError::Internal)?;
            let session = sessions
                .get(session_id)
                .ok_or(OnvifControllerError::SessionExpired)?;
            let device = session
                .devices
                .get(device_id)
                .cloned()
                .ok_or(OnvifControllerError::DeviceExpired)?;
            let connection = session
                .connections
                .get(device_id)
                .cloned()
                .ok_or(OnvifControllerError::DeviceExpired)?;
            (device, connection)
        };
        let credentials = OnvifCredentials {
            username: connection.credentials.username.clone(),
            password: connection.credentials.password().to_owned(),
        };
        let control = self
            .device
            .ptz_control(&connection.device_service, &credentials)?;
        self.require_accepting()?;
        {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| OnvifControllerError::Internal)?;
            let session = sessions
                .get(session_id)
                .ok_or(OnvifControllerError::SessionExpired)?;
            let current_device = session
                .devices
                .get(device_id)
                .ok_or(OnvifControllerError::DeviceExpired)?;
            let current_connection = session
                .connections
                .get(device_id)
                .ok_or(OnvifControllerError::DeviceExpired)?;
            if current_device.endpoint_reference != device.endpoint_reference
                || current_connection.connection_id != connection.connection_id
            {
                return Err(OnvifControllerError::DeviceExpired);
            }
        }
        Ok(PreparedPtzPairing {
            device_service: connection.device_service,
            endpoint_reference: device.endpoint_reference,
            credentials: connection.credentials,
            control,
        })
    }

    pub fn camera_draft(
        &self,
        session_id: &str,
        device_id: &str,
        profile_token: &str,
        camera_id: String,
        display_name: String,
        audio_policy: AudioPolicy,
    ) -> Result<CameraDraft, OnvifControllerError> {
        self.require_accepting()?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        let connection = sessions
            .get(session_id)
            .ok_or(OnvifControllerError::SessionExpired)?
            .connections
            .get(device_id)
            .ok_or(OnvifControllerError::DeviceExpired)?;
        let profile = connection
            .interrogation
            .profiles
            .iter()
            .find(|profile| profile.token == profile_token)
            .ok_or(OnvifControllerError::ProfileNotFound)?;
        if !profile.is_h264_compatible() {
            return Err(OnvifError::NoCompatibleProfile.into());
        }
        let prepared = connection
            .prepared
            .as_ref()
            .filter(|prepared| prepared.token == profile_token)
            .ok_or(OnvifControllerError::ProfileNotFound)?;
        Ok(CameraDraft {
            camera_id,
            display_name,
            host: prepared.endpoint.host.clone(),
            port: prepared.endpoint.port,
            path: prepared.endpoint.path.clone(),
            audio_policy,
            replacement_credentials: Some(connection.credentials.clone()),
        })
    }

    pub fn cancel(&self, session_id: Option<&str>) -> Result<(), OnvifControllerError> {
        self.cancel_active_discovery()?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?;
        if let Some(session_id) = session_id {
            sessions.remove(session_id);
        } else {
            sessions.clear();
        }
        Ok(())
    }

    pub fn cancel_session(&self, session_id: &str) -> Result<(), OnvifControllerError> {
        self.cancel(Some(session_id))
    }

    pub fn stop_accepting_and_cancel(&self) -> Result<(), OnvifControllerError> {
        self.accepting.store(false, Ordering::Release);
        self.cancel_active_discovery()?;
        self.sessions
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?
            .clear();
        Ok(())
    }

    pub fn resume_accepting(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    fn cancel_active_discovery(&self) -> Result<(), OnvifControllerError> {
        if let Some(cancel) = self
            .active_discovery_cancel
            .lock()
            .map_err(|_| OnvifControllerError::Internal)?
            .as_ref()
        {
            cancel.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn require_accepting(&self) -> Result<(), OnvifControllerError> {
        if self.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(OnvifControllerError::NotAccepting)
        }
    }
}

fn discovery_label(device: &DiscoveredDevice) -> String {
    device
        .scopes
        .iter()
        .find_map(|scope| scope.split("/name/").nth(1))
        .map(|name| name.replace("%20", " "))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| device.network_address.clone())
}

fn profile_dtos(profiles: &[MediaProfile]) -> Vec<OnvifMediaProfileDto> {
    let recommended = profiles.iter().position(MediaProfile::is_h264_compatible);
    profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| OnvifMediaProfileDto {
            token: profile.token.clone(),
            name: profile.name.clone(),
            video_codec: profile.video_codec.clone(),
            width: profile.width,
            height: profile.height,
            framerate: profile.framerate,
            bitrate_kbps: profile.bitrate_kbps,
            audio_codec: profile.audio_codec.clone(),
            supported: profile.is_h264_compatible(),
            recommended: recommended == Some(index),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nian_onvif::{DeviceInformation, MediaServiceKind};

    #[derive(Debug)]
    struct FakeDiscovery {
        devices: Vec<DiscoveredDevice>,
    }

    impl DiscoveryBackend for FakeDiscovery {
        fn discover(&self, cancel: &AtomicBool) -> Result<Vec<DiscoveredDevice>, OnvifError> {
            if cancel.load(Ordering::Acquire) {
                Err(OnvifError::Cancelled)
            } else {
                Ok(self.devices.clone())
            }
        }
    }

    #[derive(Debug)]
    struct FakeDevice {
        auth_failure: bool,
    }

    impl DeviceBackend for FakeDevice {
        fn interrogate(
            &self,
            _device_service: &str,
            credentials: &OnvifCredentials,
        ) -> Result<OnvifInterrogation, OnvifError> {
            if self.auth_failure || credentials.password == "wrong" {
                return Err(OnvifError::AuthFailed);
            }
            Ok(OnvifInterrogation {
                device: DeviceInformation {
                    manufacturer: Some("Fixture Corp".into()),
                    model: Some("Fixture Cam".into()),
                    hostname: Some("front-camera".into()),
                    ..DeviceInformation::default()
                },
                media_service: "http://192.168.1.8/onvif/media".into(),
                media_service_kind: MediaServiceKind::Media2,
                profiles: vec![
                    MediaProfile {
                        token: "main".into(),
                        name: Some("Main".into()),
                        video_codec: Some("H264".into()),
                        width: Some(1920),
                        height: Some(1080),
                        framerate: Some(25),
                        bitrate_kbps: Some(4096),
                        audio_codec: Some("AAC".into()),
                        service_kind: MediaServiceKind::Media2,
                    },
                    MediaProfile {
                        token: "hevc".into(),
                        name: Some("HEVC".into()),
                        video_codec: Some("H265".into()),
                        width: Some(3840),
                        height: Some(2160),
                        framerate: Some(25),
                        bitrate_kbps: None,
                        audio_codec: None,
                        service_kind: MediaServiceKind::Media2,
                    },
                ],
            })
        }

        fn stream_endpoint(
            &self,
            _device_service: &str,
            _media_service: &str,
            _credentials: &OnvifCredentials,
            profile: &MediaProfile,
        ) -> Result<StreamEndpoint, OnvifError> {
            if !profile.is_h264_compatible() {
                return Err(OnvifError::NoCompatibleProfile);
            }
            Ok(StreamEndpoint {
                host: "192.168.1.8".into(),
                port: 8554,
                path: "/live/main".into(),
                host_mismatch: false,
            })
        }
    }

    struct BlockingPtzDevice {
        inner: FakeDevice,
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl DeviceBackend for BlockingPtzDevice {
        fn interrogate(
            &self,
            device_service: &str,
            credentials: &OnvifCredentials,
        ) -> Result<OnvifInterrogation, OnvifError> {
            self.inner.interrogate(device_service, credentials)
        }

        fn stream_endpoint(
            &self,
            device_service: &str,
            media_service: &str,
            credentials: &OnvifCredentials,
            profile: &MediaProfile,
        ) -> Result<StreamEndpoint, OnvifError> {
            self.inner
                .stream_endpoint(device_service, media_service, credentials, profile)
        }

        fn ptz_control(
            &self,
            _device_service: &str,
            _credentials: &OnvifCredentials,
        ) -> Result<PtzControl, OnvifError> {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(PtzControl::test_fixture(false))
        }
    }

    fn blocking_ptz_controller(
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    ) -> OnvifController {
        OnvifController::with_backends(
            Arc::new(FakeDiscovery {
                devices: vec![DiscoveredDevice {
                    endpoint_reference: "urn:uuid:fixture".into(),
                    xaddrs: vec!["http://192.168.1.8/onvif/device_service".into()],
                    scopes: vec![],
                    network_address: "192.168.1.8".into(),
                }],
            }),
            Arc::new(BlockingPtzDevice {
                inner: FakeDevice {
                    auth_failure: false,
                },
                entered,
                release,
            }),
        )
    }

    fn wait_for_atomic_true(flag: &AtomicBool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !flag.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    fn controller(auth_failure: bool) -> OnvifController {
        OnvifController::with_backends(
            Arc::new(FakeDiscovery {
                devices: vec![DiscoveredDevice {
                    endpoint_reference: "urn:uuid:fixture".into(),
                    xaddrs: vec!["http://192.168.1.8/onvif/device_service".into()],
                    scopes: vec!["onvif://www.onvif.org/name/Front%20Door".into()],
                    network_address: "192.168.1.8".into(),
                }],
            }),
            Arc::new(FakeDevice { auth_failure }),
        )
    }

    #[test]
    fn cancellation_wins_after_discovery_network_work_but_before_session_commit() {
        #[derive(Debug)]
        struct CancelRaceDiscovery {
            entered: Arc<AtomicBool>,
        }

        impl DiscoveryBackend for CancelRaceDiscovery {
            fn discover(&self, cancel: &AtomicBool) -> Result<Vec<DiscoveredDevice>, OnvifError> {
                self.entered.store(true, Ordering::Release);
                while !cancel.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                // Simulate a socket read that completed successfully at the same
                // instant cancellation arrived. The controller must still refuse
                // to publish a fresh discovery session after Cancel won.
                Ok(vec![DiscoveredDevice {
                    endpoint_reference: "urn:uuid:cancel-race".into(),
                    xaddrs: vec!["http://192.168.1.8/onvif/device_service".into()],
                    scopes: vec![],
                    network_address: "192.168.1.8".into(),
                }])
            }
        }

        let entered = Arc::new(AtomicBool::new(false));
        let controller = Arc::new(OnvifController::with_backends(
            Arc::new(CancelRaceDiscovery {
                entered: entered.clone(),
            }),
            Arc::new(FakeDevice {
                auth_failure: false,
            }),
        ));
        let worker_controller = controller.clone();
        let worker = std::thread::spawn(move || worker_controller.discover());
        while !entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        controller.cancel(None).unwrap();

        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(
            error,
            OnvifControllerError::Protocol(OnvifError::Cancelled)
        ));
        assert!(controller.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn refresh_invalidates_old_session_and_device_handles() {
        let controller = controller(false);
        let first = controller.discover().unwrap();
        let first_device = first.devices[0].device_id.clone();
        let second = controller.discover().unwrap();
        assert_ne!(first.session_id, second.session_id);
        let error = controller
            .connect(
                &first.session_id,
                &first_device,
                Credentials::new("admin", "secret"),
            )
            .unwrap_err();
        assert!(matches!(error, OnvifControllerError::SessionExpired));
    }

    #[test]
    fn cancelled_session_invalidates_blocked_ptz_pairing_prepare() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let controller = Arc::new(blocking_ptz_controller(entered.clone(), release.clone()));
        let discovery = controller.discover().unwrap();
        let device_id = discovery.devices[0].device_id.clone();
        controller
            .connect(
                &discovery.session_id,
                &device_id,
                Credentials::new("admin", "secret"),
            )
            .unwrap();

        let worker_controller = controller.clone();
        let session_id = discovery.session_id.clone();
        let worker_device_id = device_id.clone();
        let worker = std::thread::spawn(move || {
            worker_controller.prepare_ptz_pairing(&session_id, &worker_device_id)
        });
        wait_for_atomic_true(&entered);
        controller.cancel_session(&discovery.session_id).unwrap();
        release.store(true, Ordering::Release);

        assert!(matches!(
            worker.join().unwrap(),
            Err(OnvifControllerError::SessionExpired)
        ));
    }

    #[test]
    fn reconnect_invalidates_blocked_ptz_pairing_prepare_connection_generation() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let controller = Arc::new(blocking_ptz_controller(entered.clone(), release.clone()));
        let discovery = controller.discover().unwrap();
        let device_id = discovery.devices[0].device_id.clone();
        controller
            .connect(
                &discovery.session_id,
                &device_id,
                Credentials::new("admin", "first-secret"),
            )
            .unwrap();

        let worker_controller = controller.clone();
        let session_id = discovery.session_id.clone();
        let worker_device_id = device_id.clone();
        let worker = std::thread::spawn(move || {
            worker_controller.prepare_ptz_pairing(&session_id, &worker_device_id)
        });
        wait_for_atomic_true(&entered);
        controller
            .connect(
                &discovery.session_id,
                &device_id,
                Credentials::new("admin", "second-secret"),
            )
            .unwrap();
        release.store(true, Ordering::Release);

        assert!(matches!(
            worker.join().unwrap(),
            Err(OnvifControllerError::DeviceExpired)
        ));
    }

    #[test]
    fn safe_dtos_omit_credentials_and_prepared_draft_owns_transient_secret() {
        let controller = controller(false);
        let discovery = controller.discover().unwrap();
        let device = &discovery.devices[0];
        let connection = controller
            .connect(
                &discovery.session_id,
                &device.device_id,
                Credentials::new("admin", "SENTINEL-secret"),
            )
            .unwrap();
        let serialized = serde_json::to_string(&connection).unwrap();
        assert!(!serialized.contains("SENTINEL-secret"));
        assert!(connection.profiles[0].recommended);
        assert!(!connection.profiles[1].supported);
        controller
            .prepare_profile(&discovery.session_id, &device.device_id, "main")
            .unwrap();
        let draft = controller
            .camera_draft(
                &discovery.session_id,
                &device.device_id,
                "main",
                "front-door".into(),
                "Front door".into(),
                AudioPolicy::CopyAll,
            )
            .unwrap();
        assert_eq!(draft.host, "192.168.1.8");
        assert_eq!(draft.port, 8554);
        assert_eq!(draft.path, "/live/main");
        assert_eq!(
            draft.replacement_credentials.unwrap().password(),
            "SENTINEL-secret"
        );
    }

    #[test]
    fn rtsp_query_cannot_cross_into_ui_or_camera_draft_even_if_backend_regresses() {
        #[derive(Debug)]
        struct QueryEndpointDevice;

        impl DeviceBackend for QueryEndpointDevice {
            fn interrogate(
                &self,
                device_service: &str,
                credentials: &OnvifCredentials,
            ) -> Result<OnvifInterrogation, OnvifError> {
                FakeDevice {
                    auth_failure: false,
                }
                .interrogate(device_service, credentials)
            }

            fn stream_endpoint(
                &self,
                _device_service: &str,
                _media_service: &str,
                _credentials: &OnvifCredentials,
                _profile: &MediaProfile,
            ) -> Result<StreamEndpoint, OnvifError> {
                Ok(StreamEndpoint {
                    host: "192.168.1.8".into(),
                    port: 554,
                    path: "/live?opaque=SENTINEL-query-secret".into(),
                    host_mismatch: false,
                })
            }
        }

        let controller = OnvifController::with_backends(
            Arc::new(FakeDiscovery {
                devices: vec![DiscoveredDevice {
                    endpoint_reference: "urn:uuid:fixture".into(),
                    xaddrs: vec!["http://192.168.1.8/onvif/device_service".into()],
                    scopes: Vec::new(),
                    network_address: "192.168.1.8".into(),
                }],
            }),
            Arc::new(QueryEndpointDevice),
        );
        let discovery = controller.discover().unwrap();
        let device = &discovery.devices[0];
        controller
            .connect(
                &discovery.session_id,
                &device.device_id,
                Credentials::new("admin", "secret"),
            )
            .unwrap();
        let error = controller
            .prepare_profile(&discovery.session_id, &device.device_id, "main")
            .unwrap_err();
        assert!(matches!(
            error,
            OnvifControllerError::Protocol(OnvifError::InvalidStreamUri)
        ));
        assert!(!format!("{error:?}").contains("SENTINEL-query-secret"));
        assert!(matches!(
            controller.camera_draft(
                &discovery.session_id,
                &device.device_id,
                "main",
                "camera".into(),
                "Camera".into(),
                AudioPolicy::Exclude,
            ),
            Err(OnvifControllerError::ProfileNotFound)
        ));
    }

    #[test]
    fn authentication_failure_is_typed_and_secret_safe() {
        let controller = controller(true);
        let discovery = controller.discover().unwrap();
        let error = controller
            .connect(
                &discovery.session_id,
                &discovery.devices[0].device_id,
                Credentials::new("admin", "SENTINEL-secret"),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            OnvifControllerError::Protocol(OnvifError::AuthFailed)
        ));
        assert!(!error.to_string().contains("SENTINEL-secret"));
    }

    #[test]
    fn connect_prefers_https_discovery_authority_before_http() {
        #[derive(Debug)]
        struct PreferenceDevice {
            attempts: Arc<Mutex<Vec<String>>>,
        }

        impl DeviceBackend for PreferenceDevice {
            fn interrogate(
                &self,
                device_service: &str,
                credentials: &OnvifCredentials,
            ) -> Result<OnvifInterrogation, OnvifError> {
                self.attempts
                    .lock()
                    .unwrap()
                    .push(device_service.to_owned());
                if device_service.starts_with("https://") {
                    FakeDevice {
                        auth_failure: false,
                    }
                    .interrogate(device_service, credentials)
                } else {
                    Err(OnvifError::DeviceUnreachable)
                }
            }

            fn stream_endpoint(
                &self,
                _device_service: &str,
                _media_service: &str,
                _credentials: &OnvifCredentials,
                _profile: &MediaProfile,
            ) -> Result<StreamEndpoint, OnvifError> {
                Err(OnvifError::Unsupported)
            }
        }

        let attempts = Arc::new(Mutex::new(Vec::new()));
        let controller = OnvifController::with_backends(
            Arc::new(FakeDiscovery {
                devices: vec![DiscoveredDevice {
                    endpoint_reference: "urn:uuid:https-preference".into(),
                    xaddrs: vec![
                        "http://192.168.1.8/onvif/device_service".into(),
                        "https://192.168.1.8/onvif/device_service".into(),
                    ],
                    scopes: vec![],
                    network_address: "192.168.1.8".into(),
                }],
            }),
            Arc::new(PreferenceDevice {
                attempts: attempts.clone(),
            }),
        );
        let discovery = controller.discover().unwrap();
        controller
            .connect(
                &discovery.session_id,
                &discovery.devices[0].device_id,
                Credentials::new("admin", "secret"),
            )
            .unwrap();

        let attempts = attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(attempts[0].starts_with("https://"));
    }

    #[test]
    fn one_failed_device_does_not_poison_another_device_in_same_discovery_session() {
        #[derive(Debug)]
        struct SelectiveDevice;

        impl DeviceBackend for SelectiveDevice {
            fn interrogate(
                &self,
                device_service: &str,
                _credentials: &OnvifCredentials,
            ) -> Result<OnvifInterrogation, OnvifError> {
                if device_service.contains("192.168.1.8") {
                    return Err(OnvifError::DeviceUnreachable);
                }
                Ok(OnvifInterrogation {
                    device: DeviceInformation {
                        manufacturer: Some("Fixture Corp".into()),
                        model: Some("Working Cam".into()),
                        hostname: Some("working-camera".into()),
                        ..DeviceInformation::default()
                    },
                    media_service: "http://192.168.1.9/onvif/media".into(),
                    media_service_kind: MediaServiceKind::Media2,
                    profiles: vec![MediaProfile {
                        token: "main".into(),
                        name: Some("Main".into()),
                        video_codec: Some("H264".into()),
                        width: Some(1920),
                        height: Some(1080),
                        framerate: Some(25),
                        bitrate_kbps: Some(4096),
                        audio_codec: None,
                        service_kind: MediaServiceKind::Media2,
                    }],
                })
            }

            fn stream_endpoint(
                &self,
                _device_service: &str,
                _media_service: &str,
                _credentials: &OnvifCredentials,
                _profile: &MediaProfile,
            ) -> Result<StreamEndpoint, OnvifError> {
                Ok(StreamEndpoint {
                    host: "192.168.1.9".into(),
                    port: 554,
                    path: "/stream1".into(),
                    host_mismatch: false,
                })
            }
        }

        let controller = OnvifController::with_backends(
            Arc::new(FakeDiscovery {
                devices: vec![
                    DiscoveredDevice {
                        endpoint_reference: "urn:uuid:broken".into(),
                        xaddrs: vec!["http://192.168.1.8/onvif/device_service".into()],
                        scopes: vec![],
                        network_address: "192.168.1.8".into(),
                    },
                    DiscoveredDevice {
                        endpoint_reference: "urn:uuid:working".into(),
                        xaddrs: vec!["http://192.168.1.9/onvif/device_service".into()],
                        scopes: vec![],
                        network_address: "192.168.1.9".into(),
                    },
                ],
            }),
            Arc::new(SelectiveDevice),
        );
        let discovery = controller.discover().unwrap();
        let broken = discovery
            .devices
            .iter()
            .find(|device| device.endpoint_reference == "urn:uuid:broken")
            .unwrap();
        let working = discovery
            .devices
            .iter()
            .find(|device| device.endpoint_reference == "urn:uuid:working")
            .unwrap();

        assert!(matches!(
            controller.connect(
                &discovery.session_id,
                &broken.device_id,
                Credentials::new("admin", "secret"),
            ),
            Err(OnvifControllerError::Protocol(
                OnvifError::DeviceUnreachable
            ))
        ));
        let connected = controller
            .connect(
                &discovery.session_id,
                &working.device_id,
                Credentials::new("admin", "secret"),
            )
            .unwrap();
        assert_eq!(connected.model.as_deref(), Some("Working Cam"));
        assert_eq!(connected.profiles.len(), 1);
        assert!(connected.profiles[0].supported);
    }

    #[derive(Debug)]
    struct FixedCredentialRefGenerator {
        reference: nian_domain::CredentialRef,
    }

    impl crate::CredentialRefGenerator for FixedCredentialRefGenerator {
        fn generate(
            &self,
            _camera_id: &nian_domain::CameraId,
        ) -> Result<nian_domain::CredentialRef, crate::CredentialRefGeneratorError> {
            Ok(self.reference.clone())
        }
    }

    struct FailingInsertRepository {
        inner: nian_settings::SettingsStore,
    }

    impl crate::SettingsRepository for FailingInsertRepository {
        fn list_cameras(
            &self,
        ) -> Result<Vec<nian_domain::CameraConfig>, crate::SettingsRepositoryError> {
            crate::SettingsRepository::list_cameras(&self.inner)
        }

        fn get_camera(
            &self,
            camera_id: &nian_domain::CameraId,
        ) -> Result<Option<nian_domain::CameraConfig>, crate::SettingsRepositoryError> {
            crate::SettingsRepository::get_camera(&self.inner, camera_id)
        }

        fn insert_camera(
            &mut self,
            _camera: &nian_domain::CameraConfig,
        ) -> Result<(), crate::SettingsRepositoryError> {
            Err(crate::SettingsRepositoryError::Persistence)
        }

        fn update_camera(
            &mut self,
            camera: &nian_domain::CameraConfig,
        ) -> Result<bool, crate::SettingsRepositoryError> {
            crate::SettingsRepository::update_camera(&mut self.inner, camera)
        }

        fn delete_camera(
            &mut self,
            camera_id: &nian_domain::CameraId,
        ) -> Result<bool, crate::SettingsRepositoryError> {
            crate::SettingsRepository::delete_camera(&mut self.inner, camera_id)
        }

        fn get_ptz_binding(
            &self,
            camera_id: &nian_domain::CameraId,
        ) -> Result<Option<nian_domain::PtzBinding>, crate::SettingsRepositoryError> {
            crate::SettingsRepository::get_ptz_binding(&self.inner, camera_id)
        }

        fn application_settings(
            &self,
        ) -> Result<nian_settings::ApplicationSettings, crate::SettingsRepositoryError> {
            crate::SettingsRepository::application_settings(&self.inner)
        }

        fn save_application_settings(
            &mut self,
            settings: &nian_settings::ApplicationSettings,
        ) -> Result<(), crate::SettingsRepositoryError> {
            crate::SettingsRepository::save_application_settings(&mut self.inner, settings)
        }

        fn recording_enabled_cameras(
            &self,
        ) -> Result<Vec<nian_domain::CameraId>, crate::SettingsRepositoryError> {
            crate::SettingsRepository::recording_enabled_cameras(&self.inner)
        }

        fn set_recording_enabled(
            &mut self,
            camera_id: &nian_domain::CameraId,
            enabled: bool,
        ) -> Result<bool, crate::SettingsRepositoryError> {
            crate::SettingsRepository::set_recording_enabled(&mut self.inner, camera_id, enabled)
        }

        fn set_all_recording_enabled(
            &mut self,
            enabled: bool,
        ) -> Result<(), crate::SettingsRepositoryError> {
            crate::SettingsRepository::set_all_recording_enabled(&mut self.inner, enabled)
        }
    }

    fn prepared_fixture_draft() -> CameraDraft {
        let controller = controller(false);
        let discovery = controller.discover().unwrap();
        let device = &discovery.devices[0];
        controller
            .connect(
                &discovery.session_id,
                &device.device_id,
                Credentials::new("admin", "SENTINEL-persist-secret"),
            )
            .unwrap();
        controller
            .prepare_profile(&discovery.session_id, &device.device_id, "main")
            .unwrap();
        controller
            .camera_draft(
                &discovery.session_id,
                &device.device_id,
                "main",
                "front-door".into(),
                "Front door".into(),
                AudioPolicy::Exclude,
            )
            .unwrap()
    }

    #[test]
    fn onvif_draft_provisions_through_the_existing_camera_model_and_credential_store() {
        let temp = tempfile::tempdir().unwrap();
        let repository =
            nian_settings::SettingsStore::open(temp.path().join("settings.sqlite3")).unwrap();
        let credentials = Arc::new(crate::MemoryCredentialStore::default());
        let reference = nian_domain::CredentialRef::parse(
            "nian-vision/front-door/11111111-1111-4111-8111-111111111111",
        )
        .unwrap();
        let mut service = crate::CameraService::with_credential_ref_generator(
            Box::new(repository),
            credentials.clone(),
            Arc::new(FixedCredentialRefGenerator {
                reference: reference.clone(),
            }),
        );

        let mutation = service.create_camera(prepared_fixture_draft()).unwrap();
        assert_eq!(mutation.value.camera_id, "front-door");
        assert_eq!(mutation.value.host, "192.168.1.8");
        assert_eq!(mutation.value.port, 8554);
        assert_eq!(mutation.value.path, "/live/main");
        assert!(crate::CredentialStore::exists(credentials.as_ref(), &reference).unwrap());
        assert_eq!(service.list_cameras().unwrap(), vec![mutation.value]);
    }

    #[test]
    fn onvif_provisioning_rolls_back_new_credential_when_camera_insert_fails() {
        let temp = tempfile::tempdir().unwrap();
        let inner =
            nian_settings::SettingsStore::open(temp.path().join("settings.sqlite3")).unwrap();
        let credentials = Arc::new(crate::MemoryCredentialStore::default());
        let reference = nian_domain::CredentialRef::parse(
            "nian-vision/front-door/22222222-2222-4222-8222-222222222222",
        )
        .unwrap();
        let mut service = crate::CameraService::with_credential_ref_generator(
            Box::new(FailingInsertRepository { inner }),
            credentials.clone(),
            Arc::new(FixedCredentialRefGenerator {
                reference: reference.clone(),
            }),
        );

        let error = service.create_camera(prepared_fixture_draft()).unwrap_err();
        assert!(matches!(error, crate::CameraServiceError::Settings));
        assert!(!crate::CredentialStore::exists(credentials.as_ref(), &reference).unwrap());
        assert!(service.list_cameras().unwrap().is_empty());
    }

    #[test]
    fn suspend_clears_sessions_and_resume_does_not_restore_them() {
        let controller = controller(false);
        let discovery = controller.discover().unwrap();
        controller.stop_accepting_and_cancel().unwrap();
        controller.resume_accepting();
        let error = controller
            .connect(
                &discovery.session_id,
                &discovery.devices[0].device_id,
                Credentials::new("admin", "secret"),
            )
            .unwrap_err();
        assert!(matches!(error, OnvifControllerError::SessionExpired));
    }
}
