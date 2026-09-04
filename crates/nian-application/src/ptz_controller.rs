//! Optional ONVIF PTZ control-plane ownership.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nian_domain::{
    CameraConfig, CameraId, CameraSource, CredentialRef, Host, OnvifScheme, PtzBinding,
};
use nian_onvif::{OnvifClient, OnvifCredentials, OnvifError, PTZ_MOVE_TIMEOUT_MS, PtzControl};
use nian_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use crate::{CredentialStore, CredentialStoreError, PreparedPtzPairing, SettingsRepositoryError};

pub const MAX_ACTIVE_PTZ_SESSIONS: usize = 16;
pub const PTZ_COMMAND_QUEUE_CAPACITY: usize = 4;
pub const PTZ_MOVEMENT_LEASE_MS: u64 = PTZ_MOVE_TIMEOUT_MS;
pub const PTZ_RENEW_INTERVAL_MS: u64 = 400;
const PTZ_NORMALIZED_SPEED: f64 = 0.45;
const MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS: usize = 8;

pub trait PtzSettingsRepository: Send {
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError>;
    fn get_ptz_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<PtzBinding>, SettingsRepositoryError>;
    fn save_ptz_binding(&mut self, binding: &PtzBinding) -> Result<(), SettingsRepositoryError>;
    fn delete_ptz_binding(&mut self, camera_id: &CameraId)
    -> Result<bool, SettingsRepositoryError>;
}

impl PtzSettingsRepository for SettingsStore {
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError> {
        SettingsStore::get_camera(self, camera_id).map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn get_ptz_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<PtzBinding>, SettingsRepositoryError> {
        SettingsStore::get_ptz_binding(self, camera_id)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn save_ptz_binding(&mut self, binding: &PtzBinding) -> Result<(), SettingsRepositoryError> {
        SettingsStore::save_ptz_binding(self, binding)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn delete_ptz_binding(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::delete_ptz_binding(self, camera_id)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }
}

pub trait PtzBackend: Send + Sync {
    fn control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<PtzControl, OnvifError>;
    fn continuous_move(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: Option<(f64, f64)>,
        zoom: Option<f64>,
    ) -> Result<(), OnvifError>;
    fn stop(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: bool,
        zoom: bool,
    ) -> Result<(), OnvifError>;
}

impl PtzBackend for OnvifClient {
    fn control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<PtzControl, OnvifError> {
        OnvifClient::ptz_control(self, device_service, credentials)
    }

    fn continuous_move(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: Option<(f64, f64)>,
        zoom: Option<f64>,
    ) -> Result<(), OnvifError> {
        OnvifClient::continuous_move(self, control, credentials, pan_tilt, zoom)
    }

    fn stop(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: bool,
        zoom: bool,
    ) -> Result<(), OnvifError> {
        OnvifClient::stop(self, control, credentials, pan_tilt, zoom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PtzDirection {
    Up,
    Down,
    Left,
    Right,
    ZoomIn,
    ZoomOut,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PtzRuntimeState {
    Ready,
    Moving,
    Degraded,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PtzCapabilitiesDto {
    pub camera_id: String,
    pub configured: bool,
    pub ptz_supported: bool,
    pub pan_tilt_supported: bool,
    pub zoom_supported: bool,
    pub state: Option<PtzRuntimeState>,
    pub error: Option<String>,
}

impl PtzCapabilitiesDto {
    fn not_configured(camera_id: &CameraId) -> Self {
        Self {
            camera_id: camera_id.as_str().to_owned(),
            configured: false,
            ptz_supported: false,
            pan_tilt_supported: false,
            zoom_supported: false,
            state: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PtzMovementDto {
    pub camera_id: String,
    pub generation: u64,
    pub lease_ms: u64,
    pub renew_after_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PtzWarning {
    OrphanCredentialCleanupFailed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PtzMutation<T> {
    pub value: T,
    pub warning: Option<PtzWarning>,
}

#[derive(Debug, Error)]
pub enum PtzError {
    #[error("camera not found")]
    CameraNotFound,
    #[error("PTZ is not configured for this camera")]
    NotConfigured,
    #[error("camera does not advertise compatible PTZ control")]
    Unsupported,
    #[error("ONVIF authentication failed")]
    AuthFailed,
    #[error("ONVIF device is unreachable")]
    DeviceUnreachable,
    #[error("PTZ control request timed out")]
    ControlTimeout,
    #[error("PTZ control request failed")]
    ControlFailed,
    #[error("PTZ operation was cancelled by lifecycle")]
    LifecycleCancelled,
    #[error("selected ONVIF device authority does not match the configured RTSP camera")]
    AuthorityMismatch,
    #[error("PTZ session capacity reached")]
    Capacity,
    #[error("PTZ command queue is busy")]
    Busy,
    #[error("settings persistence failed")]
    Settings,
    #[error("credential store unavailable")]
    CredentialStore(#[from] CredentialStoreError),
    #[error("credential rollback cleanup failed")]
    CredentialRollbackCleanup,
    #[error("PTZ internal state unavailable")]
    Internal,
}

#[derive(Debug, Clone)]
struct RuntimeStatus {
    state: PtzRuntimeState,
    last_error: Option<String>,
}

#[derive(Clone)]
struct SessionRef {
    tx: SyncSender<PtzCommand>,
    status: Arc<Mutex<RuntimeStatus>>,
    stop_requested: Arc<AtomicBool>,
    pan_tilt_supported: bool,
    zoom_supported: bool,
}

struct SessionHandle {
    session: SessionRef,
    join: JoinHandle<()>,
}

enum PtzCommand {
    Move {
        generation: u64,
        direction: PtzDirection,
        reply: mpsc::Sender<Result<(), OnvifError>>,
    },
    Renew {
        generation: u64,
        reply: mpsc::Sender<Result<(), OnvifError>>,
    },
    Stop {
        generation: u64,
        reply: mpsc::Sender<Result<(), OnvifError>>,
    },
    StopAll,
    Shutdown,
}

#[derive(Default)]
pub struct PtzTeardownBatch {
    handles: Vec<SessionHandle>,
}

#[derive(Debug, Clone, Copy)]
struct Movement {
    generation: u64,
    direction: PtzDirection,
    deadline: Instant,
}

pub struct PtzController {
    repository: Mutex<Box<dyn PtzSettingsRepository>>,
    credentials: Arc<dyn CredentialStore>,
    backend: Arc<dyn PtzBackend>,
    accepting: AtomicBool,
    generation: AtomicU64,
    sessions: Mutex<HashMap<CameraId, SessionHandle>>,
}

impl std::fmt::Debug for PtzController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtzController").finish_non_exhaustive()
    }
}

impl PtzController {
    pub fn production(
        repository: Box<dyn PtzSettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, PtzError> {
        let backend = Arc::new(OnvifClient::new().map_err(map_protocol_error)?);
        Ok(Self::with_backend(repository, credentials, backend))
    }

    pub fn with_backend(
        repository: Box<dyn PtzSettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
        backend: Arc<dyn PtzBackend>,
    ) -> Self {
        Self {
            repository: Mutex::new(repository),
            credentials,
            backend,
            accepting: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn pair(
        &self,
        camera_id: &str,
        prepared: PreparedPtzPairing,
    ) -> Result<PtzMutation<PtzCapabilitiesDto>, PtzError> {
        self.require_accepting()?;
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        let camera = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_camera(&camera_id)
            .map_err(|_| PtzError::Settings)?
            .ok_or(PtzError::CameraNotFound)?;

        let authority = parse_device_service(&prepared.device_service)?;
        let CameraSource::Rtsp(endpoint) = camera.source();
        if !endpoint
            .host()
            .as_str()
            .eq_ignore_ascii_case(authority.host.as_str())
        {
            return Err(PtzError::AuthorityMismatch);
        }
        if !prepared.control.pan_tilt_supported() {
            return Err(PtzError::Unsupported);
        }

        self.retire_session(&camera_id);
        let old_binding = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_ptz_binding(&camera_id)
            .map_err(|_| PtzError::Settings)?;
        let camera_credentials = self.credentials.get(camera.credential_ref())?;
        let (credential_ref, owns_credential, wrote_new_credential) =
            if camera_credentials == prepared.credentials {
                (camera.credential_ref().clone(), false, false)
            } else {
                let reference = self.allocate_ptz_credential_ref(&camera_id)?;
                self.credentials.put(&reference, &prepared.credentials)?;
                (reference, true, true)
            };

        let binding = PtzBinding::new(
            camera_id.clone(),
            authority.scheme,
            authority.host,
            authority.port,
            authority.path,
            prepared.endpoint_reference,
            credential_ref.clone(),
            owns_credential,
        )
        .map_err(|_| PtzError::AuthorityMismatch)?;

        if self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .save_ptz_binding(&binding)
            .is_err()
        {
            if wrote_new_credential && self.credentials.delete(&credential_ref).is_err() {
                return Err(PtzError::CredentialRollbackCleanup);
            }
            return Err(PtzError::Settings);
        }

        let mut warning = None;
        if let Some(old) = old_binding
            && old.owns_credential()
            && old.credential_ref() != binding.credential_ref()
            && self.credentials.delete(old.credential_ref()).is_err()
        {
            warning = Some(PtzWarning::OrphanCredentialCleanupFailed);
        }

        let value = PtzCapabilitiesDto {
            camera_id: camera_id.as_str().to_owned(),
            configured: true,
            ptz_supported: true,
            pan_tilt_supported: prepared.control.pan_tilt_supported(),
            zoom_supported: prepared.control.zoom_supported(),
            state: Some(PtzRuntimeState::Ready),
            error: None,
        };
        Ok(PtzMutation { value, warning })
    }

    pub fn unpair(&self, camera_id: &str) -> Result<PtzMutation<PtzCapabilitiesDto>, PtzError> {
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        self.retire_session(&camera_id);
        let binding = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_ptz_binding(&camera_id)
            .map_err(|_| PtzError::Settings)?;
        let Some(binding) = binding else {
            return Ok(PtzMutation {
                value: PtzCapabilitiesDto::not_configured(&camera_id),
                warning: None,
            });
        };
        let deleted = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .delete_ptz_binding(&camera_id)
            .map_err(|_| PtzError::Settings)?;
        if !deleted {
            return Err(PtzError::Settings);
        }
        let warning = if binding.owns_credential()
            && self.credentials.delete(binding.credential_ref()).is_err()
        {
            Some(PtzWarning::OrphanCredentialCleanupFailed)
        } else {
            None
        };
        Ok(PtzMutation {
            value: PtzCapabilitiesDto::not_configured(&camera_id),
            warning,
        })
    }

    pub fn is_configured(&self, camera_id: &str) -> Result<bool, PtzError> {
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        Ok(self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_ptz_binding(&camera_id)
            .map_err(|_| PtzError::Settings)?
            .is_some())
    }

    pub fn capabilities(&self, camera_id: &str) -> Result<PtzCapabilitiesDto, PtzError> {
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        let configured = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_ptz_binding(&camera_id)
            .map_err(|_| PtzError::Settings)?
            .is_some();
        if !configured {
            return Ok(PtzCapabilitiesDto::not_configured(&camera_id));
        }
        let session = self.ensure_session(&camera_id)?;
        let status = session
            .status
            .lock()
            .map_err(|_| PtzError::Internal)?
            .clone();
        Ok(PtzCapabilitiesDto {
            camera_id: camera_id.as_str().to_owned(),
            configured: true,
            ptz_supported: session.pan_tilt_supported,
            pan_tilt_supported: session.pan_tilt_supported,
            zoom_supported: session.zoom_supported,
            state: Some(status.state),
            error: status.last_error,
        })
    }

    pub fn move_camera(
        &self,
        camera_id: &str,
        direction: PtzDirection,
    ) -> Result<PtzMovementDto, PtzError> {
        self.require_accepting()?;
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        let session = self.ensure_session(&camera_id)?;
        if matches!(direction, PtzDirection::ZoomIn | PtzDirection::ZoomOut)
            && !session.zoom_supported
        {
            return Err(PtzError::Unsupported);
        }
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let (tx, rx) = mpsc::channel();
        send_bounded(
            &session.tx,
            PtzCommand::Move {
                generation,
                direction,
                reply: tx,
            },
        )?;
        let result = rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| PtzError::ControlTimeout)?;
        if let Err(error) = result {
            let mapped = map_protocol_error(error);
            self.retire_session(&camera_id);
            return Err(mapped);
        }
        Ok(PtzMovementDto {
            camera_id: camera_id.as_str().to_owned(),
            generation,
            lease_ms: PTZ_MOVEMENT_LEASE_MS,
            renew_after_ms: PTZ_RENEW_INTERVAL_MS,
        })
    }

    pub fn renew(&self, camera_id: &str, generation: u64) -> Result<(), PtzError> {
        self.require_accepting()?;
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        let session = self.session(&camera_id)?.ok_or(PtzError::NotConfigured)?;
        let (tx, rx) = mpsc::channel();
        send_bounded(
            &session.tx,
            PtzCommand::Renew {
                generation,
                reply: tx,
            },
        )?;
        let result = rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| PtzError::ControlTimeout)?;
        result.map_err(map_protocol_error)
    }

    pub fn stop(&self, camera_id: &str, generation: u64) -> Result<(), PtzError> {
        let camera_id = CameraId::parse(camera_id).map_err(|_| PtzError::CameraNotFound)?;
        let Some(session) = self.session(&camera_id)? else {
            return Ok(());
        };
        let (tx, rx) = mpsc::channel();
        send_bounded(
            &session.tx,
            PtzCommand::Stop {
                generation,
                reply: tx,
            },
        )?;
        let result = rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| PtzError::ControlTimeout)?;
        result.map_err(map_protocol_error)
    }

    /// Fast lifecycle signal: closes admission and invalidates every movement.
    /// Network Stop runs on the existing per-camera worker threads.
    pub fn stop_accepting_and_stop_all(&self) -> Result<(), PtzError> {
        self.accepting.store(false, Ordering::Release);
        let sessions = self.sessions.lock().map_err(|_| PtzError::Internal)?;
        for handle in sessions.values() {
            handle.session.stop_requested.store(true, Ordering::Release);
            match handle.session.tx.try_send(PtzCommand::StopAll) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(command)) => {
                    let tx = handle.session.tx.clone();
                    let _ = thread::Builder::new()
                        .name("ptz-lifecycle-stop-signal".to_owned())
                        .spawn(move || {
                            let _ = tx.send(command);
                        });
                }
            }
        }
        Ok(())
    }

    /// Freezes only the currently-owned workers into a teardown batch. A later
    /// reactivation can create fresh sessions that this batch cannot observe.
    pub fn begin_shutdown_sessions(&self) -> PtzTeardownBatch {
        let handles = match self.sessions.lock() {
            Ok(mut sessions) => sessions
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        PtzTeardownBatch { handles }
    }

    /// Settles one frozen batch. Call off the Tauri/window event thread.
    pub fn finish_shutdown_sessions(&self, batch: PtzTeardownBatch) {
        for handle in &batch.handles {
            let _ = handle.session.tx.send(PtzCommand::Shutdown);
        }
        for handle in batch.handles {
            let _ = handle.join.join();
        }
    }

    /// Settles all worker ownership. Call off the Tauri/window event thread.
    pub fn shutdown_sessions(&self) {
        let batch = self.begin_shutdown_sessions();
        self.finish_shutdown_sessions(batch);
    }

    pub fn resume_accepting(&self) {
        if let Ok(sessions) = self.sessions.lock() {
            for handle in sessions.values() {
                handle
                    .session
                    .stop_requested
                    .store(false, Ordering::Release);
            }
        }
        self.accepting.store(true, Ordering::Release);
    }

    fn ensure_session(&self, camera_id: &CameraId) -> Result<SessionRef, PtzError> {
        self.require_accepting()?;
        if let Some(session) = self.session(camera_id)? {
            return Ok(session);
        }
        {
            let sessions = self.sessions.lock().map_err(|_| PtzError::Internal)?;
            if sessions.len() >= MAX_ACTIVE_PTZ_SESSIONS {
                return Err(PtzError::Capacity);
            }
        }
        let binding = self
            .repository
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get_ptz_binding(camera_id)
            .map_err(|_| PtzError::Settings)?
            .ok_or(PtzError::NotConfigured)?;
        let credentials = self.credentials.get(binding.credential_ref())?;
        let network_credentials = OnvifCredentials {
            username: credentials.username.clone(),
            password: credentials.password().to_owned(),
        };
        let service = binding_device_service(&binding);
        let control = self
            .backend
            .control(&service, &network_credentials)
            .map_err(map_protocol_error)?;
        if !control.pan_tilt_supported() {
            return Err(PtzError::Unsupported);
        }
        self.require_accepting()?;

        let status = Arc::new(Mutex::new(RuntimeStatus {
            state: PtzRuntimeState::Ready,
            last_error: None,
        }));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(PTZ_COMMAND_QUEUE_CAPACITY);
        let backend = self.backend.clone();
        let worker_control = control.clone();
        let worker_status = status.clone();
        let worker_stop_requested = stop_requested.clone();
        let join = thread::Builder::new()
            .name(format!("ptz-{}", camera_id.as_str()))
            .spawn(move || {
                ptz_worker(
                    backend,
                    worker_control,
                    network_credentials,
                    worker_status,
                    worker_stop_requested,
                    rx,
                )
            })
            .map_err(|_| PtzError::Internal)?;
        let new_session = SessionRef {
            tx,
            status,
            stop_requested,
            pan_tilt_supported: control.pan_tilt_supported(),
            zoom_supported: control.zoom_supported(),
        };
        let mut sessions = self.sessions.lock().map_err(|_| PtzError::Internal)?;
        if let Some(existing) = sessions.get(camera_id).map(|handle| handle.session.clone()) {
            let _ = new_session.tx.send(PtzCommand::Shutdown);
            drop(sessions);
            let _ = join.join();
            return Ok(existing);
        }
        sessions.insert(
            camera_id.clone(),
            SessionHandle {
                session: new_session.clone(),
                join,
            },
        );
        Ok(new_session)
    }

    fn session(&self, camera_id: &CameraId) -> Result<Option<SessionRef>, PtzError> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| PtzError::Internal)?
            .get(camera_id)
            .map(|handle| handle.session.clone()))
    }

    fn retire_session(&self, camera_id: &CameraId) {
        let handle = self
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.remove(camera_id));
        if let Some(handle) = handle {
            let _ = handle.session.tx.send(PtzCommand::Shutdown);
            let _ = handle.join.join();
        }
    }

    fn allocate_ptz_credential_ref(&self, camera_id: &CameraId) -> Result<CredentialRef, PtzError> {
        for _ in 0..MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS {
            let reference = CredentialRef::parse(format!(
                "nian-vision/{}/ptz/{}",
                camera_id.as_str(),
                Uuid::new_v4()
            ))
            .map_err(|_| PtzError::Internal)?;
            if !self.credentials.exists(&reference)? {
                return Ok(reference);
            }
        }
        Err(PtzError::Internal)
    }

    fn require_accepting(&self) -> Result<(), PtzError> {
        if self.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(PtzError::LifecycleCancelled)
        }
    }
}

impl Drop for PtzController {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        self.shutdown_sessions();
    }
}

fn send_bounded(tx: &SyncSender<PtzCommand>, command: PtzCommand) -> Result<(), PtzError> {
    match tx.try_send(command) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(PtzError::Busy),
        Err(TrySendError::Disconnected(_)) => Err(PtzError::ControlFailed),
    }
}

fn ptz_worker(
    backend: Arc<dyn PtzBackend>,
    control: PtzControl,
    credentials: OnvifCredentials,
    status: Arc<Mutex<RuntimeStatus>>,
    stop_requested: Arc<AtomicBool>,
    rx: Receiver<PtzCommand>,
) {
    let mut movement: Option<Movement> = None;
    loop {
        let command = if let Some(active) = movement {
            match active.deadline.checked_duration_since(Instant::now()) {
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        stop_owned(&*backend, &control, &credentials, active.direction, &status);
                        movement = None;
                        None
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => {
                    stop_owned(&*backend, &control, &credentials, active.direction, &status);
                    movement = None;
                    None
                }
            }
        } else {
            match rx.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
        };
        let Some(command) = command else {
            continue;
        };
        if stop_requested.load(Ordering::Acquire) {
            if let Some(active) = movement.take() {
                stop_owned(&*backend, &control, &credentials, active.direction, &status);
            }
            match command {
                PtzCommand::Move { reply, .. } | PtzCommand::Renew { reply, .. } => {
                    let _ = reply.send(Err(OnvifError::Cancelled));
                    continue;
                }
                _ => {}
            }
        }
        match command {
            PtzCommand::Move {
                generation,
                direction,
                reply,
            } => {
                if let Some(active) = movement.take() {
                    stop_owned(&*backend, &control, &credentials, active.direction, &status);
                }
                let (pan_tilt, zoom) = movement_vector(direction);
                let result = backend.continuous_move(&control, &credentials, pan_tilt, zoom);
                match &result {
                    Ok(()) => {
                        movement = Some(Movement {
                            generation,
                            direction,
                            deadline: Instant::now() + Duration::from_millis(PTZ_MOVEMENT_LEASE_MS),
                        });
                        set_status(&status, PtzRuntimeState::Moving, None);
                    }
                    Err(error) => set_status(
                        &status,
                        PtzRuntimeState::Degraded,
                        Some(protocol_code(error)),
                    ),
                }
                let _ = reply.send(result);
            }
            PtzCommand::Renew { generation, reply } => {
                let result = if let Some(active) = movement
                    .as_mut()
                    .filter(|active| active.generation == generation)
                {
                    let (pan_tilt, zoom) = movement_vector(active.direction);
                    let result = backend.continuous_move(&control, &credentials, pan_tilt, zoom);
                    if result.is_ok() {
                        active.deadline =
                            Instant::now() + Duration::from_millis(PTZ_MOVEMENT_LEASE_MS);
                    } else if let Err(error) = &result {
                        set_status(
                            &status,
                            PtzRuntimeState::Degraded,
                            Some(protocol_code(error)),
                        );
                    }
                    result
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtzCommand::Stop { generation, reply } => {
                let result = if let Some(active) = movement
                    .filter(|active| active.generation == generation)
                    .and_then(|_| movement.take())
                {
                    let (pan_tilt, zoom) = owned_axes(active.direction);
                    let result = backend.stop(&control, &credentials, pan_tilt, zoom);
                    match &result {
                        Ok(()) => set_status(&status, PtzRuntimeState::Ready, None),
                        Err(error) => set_status(
                            &status,
                            PtzRuntimeState::Degraded,
                            Some(protocol_code(error)),
                        ),
                    }
                    result
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtzCommand::StopAll => {
                if let Some(active) = movement.take() {
                    stop_owned(&*backend, &control, &credentials, active.direction, &status);
                }
            }
            PtzCommand::Shutdown => {
                if let Some(active) = movement.take() {
                    stop_owned(&*backend, &control, &credentials, active.direction, &status);
                }
                break;
            }
        }
    }
}

fn movement_vector(direction: PtzDirection) -> (Option<(f64, f64)>, Option<f64>) {
    match direction {
        PtzDirection::Up => (Some((0.0, PTZ_NORMALIZED_SPEED)), None),
        PtzDirection::Down => (Some((0.0, -PTZ_NORMALIZED_SPEED)), None),
        PtzDirection::Left => (Some((-PTZ_NORMALIZED_SPEED, 0.0)), None),
        PtzDirection::Right => (Some((PTZ_NORMALIZED_SPEED, 0.0)), None),
        PtzDirection::ZoomIn => (None, Some(PTZ_NORMALIZED_SPEED)),
        PtzDirection::ZoomOut => (None, Some(-PTZ_NORMALIZED_SPEED)),
    }
}

fn owned_axes(direction: PtzDirection) -> (bool, bool) {
    match direction {
        PtzDirection::ZoomIn | PtzDirection::ZoomOut => (false, true),
        _ => (true, false),
    }
}

fn stop_owned(
    backend: &dyn PtzBackend,
    control: &PtzControl,
    credentials: &OnvifCredentials,
    direction: PtzDirection,
    status: &Mutex<RuntimeStatus>,
) {
    let (pan_tilt, zoom) = owned_axes(direction);
    match backend.stop(control, credentials, pan_tilt, zoom) {
        Ok(()) => set_status(status, PtzRuntimeState::Ready, None),
        Err(error) => set_status(
            status,
            PtzRuntimeState::Degraded,
            Some(protocol_code(&error)),
        ),
    }
}

fn set_status(status: &Mutex<RuntimeStatus>, state: PtzRuntimeState, error: Option<String>) {
    if let Ok(mut status) = status.lock() {
        status.state = state;
        status.last_error = error;
    }
}

fn protocol_code(error: &OnvifError) -> String {
    match error {
        OnvifError::AuthFailed => "auth_failed",
        OnvifError::Timeout => "control_timeout",
        OnvifError::DeviceUnreachable => "device_unreachable",
        OnvifError::Unsupported => "unsupported",
        OnvifError::Cancelled => "lifecycle_cancelled",
        _ => "control_failed",
    }
    .to_owned()
}

fn map_protocol_error(error: OnvifError) -> PtzError {
    match error {
        OnvifError::AuthFailed => PtzError::AuthFailed,
        OnvifError::Timeout => PtzError::ControlTimeout,
        OnvifError::DeviceUnreachable => PtzError::DeviceUnreachable,
        OnvifError::Unsupported => PtzError::Unsupported,
        OnvifError::Cancelled => PtzError::LifecycleCancelled,
        _ => PtzError::ControlFailed,
    }
}

struct DeviceAuthority {
    scheme: OnvifScheme,
    host: Host,
    port: u16,
    path: String,
}

fn parse_device_service(raw: &str) -> Result<DeviceAuthority, PtzError> {
    let url = Url::parse(raw).map_err(|_| PtzError::AuthorityMismatch)?;
    let scheme = match url.scheme() {
        "http" => OnvifScheme::Http,
        "https" => OnvifScheme::Https,
        _ => return Err(PtzError::AuthorityMismatch),
    };
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PtzError::AuthorityMismatch);
    }
    let host = Host::parse(url.host_str().ok_or(PtzError::AuthorityMismatch)?)
        .map_err(|_| PtzError::AuthorityMismatch)?;
    let port = url
        .port_or_known_default()
        .ok_or(PtzError::AuthorityMismatch)?;
    let path = if url.path().is_empty() {
        "/".to_owned()
    } else {
        url.path().to_owned()
    };
    Ok(DeviceAuthority {
        scheme,
        host,
        port,
        path,
    })
}

fn binding_device_service(binding: &PtzBinding) -> String {
    let default_port = match binding.scheme() {
        OnvifScheme::Http => 80,
        OnvifScheme::Https => 443,
    };
    let authority = if binding.port() == default_port {
        binding.host().url_component()
    } else {
        format!("{}:{}", binding.host().url_component(), binding.port())
    };
    format!(
        "{}://{}{}",
        binding.scheme().as_str(),
        authority,
        binding.device_path()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use nian_domain::{AudioPolicy, CameraEndpoint, Credentials};
    use tempfile::TempDir;

    use crate::MemoryCredentialStore;

    #[derive(Default)]
    struct FakeBackend {
        events: Mutex<Vec<String>>,
        fail_moves: AtomicUsize,
        zoom: bool,
    }

    impl FakeBackend {
        fn with_zoom(zoom: bool) -> Self {
            Self {
                zoom,
                ..Self::default()
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn wait_for(&self, needle: &str, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let matches = self
                    .events()
                    .into_iter()
                    .filter(|event| event == needle)
                    .count();
                if matches >= count {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {needle}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl PtzBackend for FakeBackend {
        fn control(
            &self,
            _device_service: &str,
            _credentials: &OnvifCredentials,
        ) -> Result<PtzControl, OnvifError> {
            self.events.lock().unwrap().push("control".to_owned());
            Ok(PtzControl::test_fixture(self.zoom))
        }

        fn continuous_move(
            &self,
            _control: &PtzControl,
            _credentials: &OnvifCredentials,
            pan_tilt: Option<(f64, f64)>,
            zoom: Option<f64>,
        ) -> Result<(), OnvifError> {
            self.events.lock().unwrap().push(if zoom.is_some() {
                "move_zoom".to_owned()
            } else if pan_tilt.is_some() {
                "move_pan_tilt".to_owned()
            } else {
                "move_invalid".to_owned()
            });
            if self
                .fail_moves
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                Err(OnvifError::DeviceUnreachable)
            } else {
                Ok(())
            }
        }

        fn stop(
            &self,
            _control: &PtzControl,
            _credentials: &OnvifCredentials,
            pan_tilt: bool,
            zoom: bool,
        ) -> Result<(), OnvifError> {
            self.events.lock().unwrap().push(
                match (pan_tilt, zoom) {
                    (true, false) => "stop_pan_tilt",
                    (false, true) => "stop_zoom",
                    _ => "stop_other",
                }
                .to_owned(),
            );
            Ok(())
        }
    }

    struct Fixture {
        _temp: TempDir,
        db_path: std::path::PathBuf,
        credentials: Arc<MemoryCredentialStore>,
        backend: Arc<FakeBackend>,
        controller: PtzController,
    }

    fn camera(camera_id: &str, host: &str, credential_ref: CredentialRef) -> CameraConfig {
        CameraConfig::new(
            CameraId::parse(camera_id).unwrap(),
            camera_id,
            CameraSource::Rtsp(
                CameraEndpoint::new(Host::parse(host).unwrap(), 554, "/stream1").unwrap(),
            ),
            AudioPolicy::Exclude,
            credential_ref,
        )
        .unwrap()
    }

    fn fixture(with_binding: bool, zoom: bool) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("settings.sqlite3");
        let mut store = SettingsStore::open(db_path.clone()).unwrap();
        let credential_ref = CredentialRef::parse("nian-vision/front-door/camera").unwrap();
        let camera = camera("front-door", "192.168.1.50", credential_ref.clone());
        store.insert_camera(&camera).unwrap();
        if with_binding {
            store
                .save_ptz_binding(
                    &PtzBinding::new(
                        CameraId::parse("front-door").unwrap(),
                        OnvifScheme::Http,
                        Host::parse("192.168.1.50").unwrap(),
                        80,
                        "/onvif/device_service",
                        "urn:uuid:front-door",
                        credential_ref.clone(),
                        false,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let credentials = Arc::new(MemoryCredentialStore::default());
        credentials
            .put(
                &credential_ref,
                &Credentials::new("admin", "camera-password"),
            )
            .unwrap();
        let backend = Arc::new(FakeBackend::with_zoom(zoom));
        let controller =
            PtzController::with_backend(Box::new(store), credentials.clone(), backend.clone());
        Fixture {
            _temp: temp,
            db_path,
            credentials,
            backend,
            controller,
        }
    }

    #[test]
    fn configured_rtsp_camera_without_binding_remains_valid_and_reports_not_configured() {
        let fixture = fixture(false, false);
        let capabilities = fixture.controller.capabilities("front-door").unwrap();
        assert!(!capabilities.configured);
        assert!(!capabilities.ptz_supported);
        assert!(fixture.backend.events().is_empty());
    }

    #[test]
    fn stale_stop_cannot_stop_a_newer_movement_generation() {
        let fixture = fixture(true, false);
        let first = fixture
            .controller
            .move_camera("front-door", PtzDirection::Left)
            .unwrap();
        let second = fixture
            .controller
            .move_camera("front-door", PtzDirection::Right)
            .unwrap();
        fixture
            .controller
            .stop("front-door", first.generation)
            .unwrap();
        let stops_before_current = fixture
            .backend
            .events()
            .into_iter()
            .filter(|event| event == "stop_pan_tilt")
            .count();
        assert_eq!(
            stops_before_current, 1,
            "replacement should stop only the previous owner"
        );
        fixture
            .controller
            .stop("front-door", second.generation)
            .unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 2);
    }

    #[test]
    fn dead_man_expiry_automatically_stops_continuous_move() {
        let fixture = fixture(true, false);
        let _movement = fixture
            .controller
            .move_camera("front-door", PtzDirection::Up)
            .unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 1);
        let capabilities = fixture.controller.capabilities("front-door").unwrap();
        assert_eq!(capabilities.state, Some(PtzRuntimeState::Ready));
    }

    #[test]
    fn renewal_extends_dead_man_without_high_frequency_spam() {
        let fixture = fixture(true, false);
        let movement = fixture
            .controller
            .move_camera("front-door", PtzDirection::Down)
            .unwrap();
        thread::sleep(Duration::from_millis(600));
        fixture
            .controller
            .renew("front-door", movement.generation)
            .unwrap();
        thread::sleep(Duration::from_millis(600));
        assert!(
            !fixture
                .backend
                .events()
                .iter()
                .any(|event| event == "stop_pan_tilt")
        );
        fixture
            .controller
            .stop("front-door", movement.generation)
            .unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 1);
        let moves = fixture
            .backend
            .events()
            .into_iter()
            .filter(|event| event == "move_pan_tilt")
            .count();
        assert_eq!(moves, 2, "one initial move plus one lease renewal");
    }

    #[test]
    fn lifecycle_cancellation_stops_motion_and_resume_never_restores_it() {
        let fixture = fixture(true, false);
        let _movement = fixture
            .controller
            .move_camera("front-door", PtzDirection::Left)
            .unwrap();
        fixture.controller.stop_accepting_and_stop_all().unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 1);
        assert!(matches!(
            fixture
                .controller
                .move_camera("front-door", PtzDirection::Right),
            Err(PtzError::LifecycleCancelled)
        ));
        let moves_before_resume = fixture
            .backend
            .events()
            .iter()
            .filter(|event| event.as_str() == "move_pan_tilt")
            .count();
        fixture.controller.resume_accepting();
        thread::sleep(Duration::from_millis(50));
        let moves_after_resume = fixture
            .backend
            .events()
            .iter()
            .filter(|event| event.as_str() == "move_pan_tilt")
            .count();
        assert_eq!(moves_before_resume, moves_after_resume);
    }

    #[test]
    fn frozen_hide_teardown_cannot_retire_a_fresh_reactivated_session() {
        let fixture = fixture(true, false);
        let _old = fixture
            .controller
            .move_camera("front-door", PtzDirection::Left)
            .unwrap();
        fixture.controller.stop_accepting_and_stop_all().unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 1);
        let stale_batch = fixture.controller.begin_shutdown_sessions();

        fixture.controller.resume_accepting();
        let fresh = fixture
            .controller
            .move_camera("front-door", PtzDirection::Right)
            .unwrap();
        fixture.controller.finish_shutdown_sessions(stale_batch);

        fixture
            .controller
            .renew("front-door", fresh.generation)
            .unwrap();
        fixture
            .controller
            .stop("front-door", fresh.generation)
            .unwrap();
        fixture.backend.wait_for("stop_pan_tilt", 2);
    }

    #[test]
    fn zoom_is_rejected_when_configuration_does_not_advertise_it() {
        let fixture = fixture(true, false);
        assert!(matches!(
            fixture
                .controller
                .move_camera("front-door", PtzDirection::ZoomIn),
            Err(PtzError::Unsupported)
        ));
        assert!(
            !fixture
                .backend
                .events()
                .iter()
                .any(|event| event == "move_zoom")
        );
    }

    #[test]
    fn one_camera_failure_does_not_block_another_camera() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("settings.sqlite3");
        let mut store = SettingsStore::open(db_path).unwrap();
        let credentials = Arc::new(MemoryCredentialStore::default());
        for (camera_id, host) in [("camera-a", "192.168.1.50"), ("camera-b", "192.168.1.51")] {
            let reference =
                CredentialRef::parse(format!("nian-vision/{camera_id}/camera")).unwrap();
            store
                .insert_camera(&camera(camera_id, host, reference.clone()))
                .unwrap();
            store
                .save_ptz_binding(
                    &PtzBinding::new(
                        CameraId::parse(camera_id).unwrap(),
                        OnvifScheme::Http,
                        Host::parse(host).unwrap(),
                        80,
                        "/onvif/device_service",
                        format!("urn:uuid:{camera_id}"),
                        reference.clone(),
                        false,
                    )
                    .unwrap(),
                )
                .unwrap();
            credentials
                .put(&reference, &Credentials::new("admin", "pw"))
                .unwrap();
        }
        let backend = Arc::new(FakeBackend::with_zoom(false));
        backend.fail_moves.store(1, Ordering::Release);
        let controller = PtzController::with_backend(Box::new(store), credentials, backend.clone());
        assert!(matches!(
            controller.move_camera("camera-a", PtzDirection::Left),
            Err(PtzError::DeviceUnreachable)
        ));
        let movement = controller
            .move_camera("camera-b", PtzDirection::Right)
            .unwrap();
        controller.stop("camera-b", movement.generation).unwrap();
        assert!(
            backend
                .events()
                .iter()
                .filter(|event| event.as_str() == "control")
                .count()
                >= 2
        );
    }

    #[test]
    fn pairing_requires_exact_configured_camera_authority() {
        let fixture = fixture(false, true);
        let prepared = PreparedPtzPairing {
            device_service: "http://192.168.1.99/onvif/device_service".to_owned(),
            endpoint_reference: "urn:uuid:other-camera".to_owned(),
            credentials: Credentials::new("admin", "camera-password"),
            control: PtzControl::test_fixture(true),
        };
        assert!(matches!(
            fixture.controller.pair("front-door", prepared),
            Err(PtzError::AuthorityMismatch)
        ));
        assert!(!fixture.controller.is_configured("front-door").unwrap());
    }

    #[test]
    fn pair_and_unpair_separate_credentials_leave_rtsp_camera_and_recording_intent_unchanged() {
        let fixture = fixture(false, true);
        {
            let mut store = SettingsStore::open(fixture.db_path.clone()).unwrap();
            store
                .set_recording_enabled(&CameraId::parse("front-door").unwrap(), true)
                .unwrap();
        }
        let prepared = PreparedPtzPairing {
            device_service: "http://192.168.1.50/onvif/device_service".to_owned(),
            endpoint_reference: "urn:uuid:front-door".to_owned(),
            credentials: Credentials::new("onvif-admin", "different-password"),
            control: PtzControl::test_fixture(true),
        };
        let paired = fixture.controller.pair("front-door", prepared).unwrap();
        assert!(paired.value.configured);
        let binding = SettingsStore::open(fixture.db_path.clone())
            .unwrap()
            .get_ptz_binding(&CameraId::parse("front-door").unwrap())
            .unwrap()
            .unwrap();
        assert!(binding.owns_credential());
        assert!(
            fixture
                .credentials
                .exists(binding.credential_ref())
                .unwrap()
        );
        let owned_ref = binding.credential_ref().clone();

        fixture.controller.unpair("front-door").unwrap();
        let store = SettingsStore::open(fixture.db_path.clone()).unwrap();
        assert!(
            store
                .get_camera(&CameraId::parse("front-door").unwrap())
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_ptz_binding(&CameraId::parse("front-door").unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.recording_enabled_cameras().unwrap(),
            vec![CameraId::parse("front-door").unwrap()]
        );
        assert!(!fixture.credentials.exists(&owned_ref).unwrap());
    }
}
