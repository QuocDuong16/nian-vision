//! Optional ONVIF PullPoint motion-event monitoring ownership.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nian_domain::{
    CameraConfig, CameraId, CameraSource, CredentialRef, EventBinding, Host, OnvifScheme,
    ReconnectBackoff,
};
use nian_index::{
    EventCursor, EventIndex, EventInsert, EventKind, EventQuery, IndexError,
    MAX_EVENT_QUERY_CAMERAS,
};
use nian_onvif::{
    EventControl, MotionNotification, OnvifClient, OnvifCredentials, OnvifError,
    PullPointSubscription,
};
use nian_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use crate::notification::{NoopPersistedEventSink, PersistedEventSignal, PersistedEventSink};
use crate::{CredentialStore, CredentialStoreError, PreparedEventPairing, SettingsRepositoryError};

pub const MAX_ACTIVE_EVENT_SESSIONS: usize = 16;
pub const MAX_EVENT_SOURCES_PER_SESSION: usize = 64;
const MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS: usize = 8;
const WORKER_IDLE_SLEEP_MS: u64 = 50;
const RENEW_FALLBACK_SECS: u64 = 40;

pub trait EventSettingsRepository: Send {
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError>;
    fn get_event_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<EventBinding>, SettingsRepositoryError>;
    fn save_event_binding(&mut self, binding: &EventBinding)
    -> Result<(), SettingsRepositoryError>;
    fn event_monitoring_enabled(
        &self,
        camera_id: &CameraId,
    ) -> Result<bool, SettingsRepositoryError>;
    fn event_monitoring_enabled_cameras(&self) -> Result<Vec<CameraId>, SettingsRepositoryError>;
    fn event_status_camera_ids(&self) -> Result<Vec<CameraId>, SettingsRepositoryError>;
    fn set_event_monitoring_enabled(
        &mut self,
        camera_id: &CameraId,
        enabled: bool,
    ) -> Result<bool, SettingsRepositoryError>;
    fn disable_and_delete_event_binding(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<Option<EventBinding>, SettingsRepositoryError>;
}

impl EventSettingsRepository for SettingsStore {
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError> {
        SettingsStore::get_camera(self, camera_id).map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn get_event_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<EventBinding>, SettingsRepositoryError> {
        SettingsStore::get_event_binding(self, camera_id)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn save_event_binding(
        &mut self,
        binding: &EventBinding,
    ) -> Result<(), SettingsRepositoryError> {
        SettingsStore::save_event_binding(self, binding)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn event_monitoring_enabled(
        &self,
        camera_id: &CameraId,
    ) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::event_monitoring_enabled(self, camera_id)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn event_monitoring_enabled_cameras(&self) -> Result<Vec<CameraId>, SettingsRepositoryError> {
        SettingsStore::event_monitoring_enabled_cameras(self)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn event_status_camera_ids(&self) -> Result<Vec<CameraId>, SettingsRepositoryError> {
        SettingsStore::list_cameras(self)
            .map(|cameras| {
                cameras
                    .into_iter()
                    .map(|camera| camera.camera_id().clone())
                    .collect()
            })
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn set_event_monitoring_enabled(
        &mut self,
        camera_id: &CameraId,
        enabled: bool,
    ) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::set_event_monitoring_enabled(self, camera_id, enabled)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }

    fn disable_and_delete_event_binding(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<Option<EventBinding>, SettingsRepositoryError> {
        SettingsStore::disable_and_delete_event_binding(self, camera_id)
            .map_err(|_| SettingsRepositoryError::Persistence)
    }
}

pub trait EventBackend: Send + Sync {
    fn control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<EventControl, OnvifError>;
    fn create_subscription(
        &self,
        control: &EventControl,
        credentials: &OnvifCredentials,
    ) -> Result<PullPointSubscription, OnvifError>;
    fn synchronize(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError>;
    fn pull(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<Vec<MotionNotification>, OnvifError>;
    fn renew(
        &self,
        subscription: &mut PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError>;
    fn unsubscribe(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError>;

    fn wait_reconnect(&self, cancel: &AtomicBool, duration: Duration) {
        sleep_cancellable(cancel, duration);
    }
}

impl EventBackend for OnvifClient {
    fn control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<EventControl, OnvifError> {
        OnvifClient::event_control(self, device_service, credentials)
    }

    fn create_subscription(
        &self,
        control: &EventControl,
        credentials: &OnvifCredentials,
    ) -> Result<PullPointSubscription, OnvifError> {
        OnvifClient::create_pullpoint_subscription(self, control, credentials)
    }

    fn synchronize(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        OnvifClient::set_synchronization_point(self, subscription, credentials)
    }

    fn pull(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<Vec<MotionNotification>, OnvifError> {
        OnvifClient::pull_messages(self, subscription, credentials)
    }

    fn renew(
        &self,
        subscription: &mut PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        OnvifClient::renew_subscription(self, subscription, credentials)
    }

    fn unsubscribe(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        OnvifClient::unsubscribe(self, subscription, credentials)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventRuntimeState {
    Disabled,
    Starting,
    Subscribing,
    Polling,
    Backoff,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventStatusDto {
    pub camera_id: String,
    pub configured: bool,
    pub desired: bool,
    pub state: EventRuntimeState,
    pub motion_active: Option<bool>,
    pub last_event_at: Option<String>,
    pub last_error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventHistoryKind {
    MotionStarted,
    MotionEnded,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventHistoryDto {
    pub event_id: u64,
    pub camera_id: String,
    pub kind: EventHistoryKind,
    pub source_key: Option<String>,
    pub device_time_utc: Option<String>,
    pub received_time_utc: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventReviewRowDto {
    pub event_id: u64,
    pub camera_id: String,
    pub camera_display_name: String,
    pub kind: EventHistoryKind,
    pub device_time_utc: Option<String>,
    pub received_time_utc: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventReviewPageDto {
    pub rows: Vec<EventReviewRowDto>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventWarning {
    OrphanCredentialCleanupFailed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventMutation<T> {
    pub value: T,
    pub warning: Option<EventWarning>,
}

#[derive(Debug, Error)]
pub enum EventError {
    #[error("camera not found")]
    CameraNotFound,
    #[error("event monitoring is not configured")]
    NotConfigured,
    #[error("camera does not advertise a compatible motion event")]
    Unsupported,
    #[error("ONVIF authentication failed")]
    AuthFailed,
    #[error("ONVIF device is unreachable")]
    DeviceUnreachable,
    #[error("event subscription failed")]
    SubscriptionFailed,
    #[error("event PullMessages timed out")]
    PullTimeout,
    #[error("ONVIF event protocol failed")]
    ProtocolError,
    #[error("event persistence failed")]
    PersistenceFailed,
    #[error("invalid event review query: {0}")]
    InvalidQuery(String),
    #[error("event session capacity reached")]
    Capacity,
    #[error("event operation is busy")]
    Busy,
    #[error("event operation was cancelled by lifecycle")]
    LifecycleCancelled,
    #[error("selected ONVIF authority does not match the configured RTSP camera")]
    AuthorityMismatch,
    #[error("settings persistence failed")]
    Settings,
    #[error("credential store unavailable")]
    CredentialStore(#[from] CredentialStoreError),
    #[error("credential rollback cleanup failed")]
    CredentialRollbackCleanup,
    #[error("event controller internal state unavailable")]
    Internal,
}

#[derive(Debug, Clone)]
struct RuntimeStatus {
    state: EventRuntimeState,
    motion_active: Option<bool>,
    last_event_at: Option<DateTime<Utc>>,
    last_error_code: Option<String>,
}

impl RuntimeStatus {
    fn starting() -> Self {
        Self {
            state: EventRuntimeState::Starting,
            motion_active: None,
            last_event_at: None,
            last_error_code: None,
        }
    }
}

#[derive(Clone)]
struct EventSessionRef {
    session_id: u64,
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<RuntimeStatus>>,
}

struct EventSessionHandle {
    camera_id: CameraId,
    session: EventSessionRef,
    join: JoinHandle<()>,
}

struct DrainState {
    camera_id: CameraId,
    session: EventSessionRef,
    join: Mutex<Option<JoinHandle<()>>>,
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl DrainState {
    fn from_handle(handle: EventSessionHandle) -> Arc<Self> {
        Arc::new(Self {
            camera_id: handle.camera_id,
            session: handle.session,
            join: Mutex::new(Some(handle.join)),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        })
    }

    fn wait_done(&self) {
        if let Ok(mut done) = self.done.lock() {
            while !*done {
                match self.done_cv.wait(done) {
                    Ok(next) => done = next,
                    Err(_) => break,
                }
            }
        }
    }
}

struct OpeningState {
    reservation_id: u64,
    lifecycle_generation: u64,
    cancelled: AtomicBool,
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl OpeningState {
    fn new(reservation_id: u64, lifecycle_generation: u64) -> Self {
        Self {
            reservation_id,
            lifecycle_generation,
            cancelled: AtomicBool::new(false),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        }
    }
}

struct MutationState {
    camera_id: CameraId,
    mutation_id: u64,
    lifecycle_generation: u64,
    cancelled: AtomicBool,
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl MutationState {
    fn new(camera_id: CameraId, mutation_id: u64, lifecycle_generation: u64) -> Self {
        Self {
            camera_id,
            mutation_id,
            lifecycle_generation,
            cancelled: AtomicBool::new(false),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct EventRegistry {
    opening: HashMap<CameraId, Arc<OpeningState>>,
    active: HashMap<CameraId, EventSessionHandle>,
    draining: HashMap<u64, Arc<DrainState>>,
    statuses: HashMap<CameraId, Arc<Mutex<RuntimeStatus>>>,
    mutating: HashMap<CameraId, Arc<MutationState>>,
}

impl EventRegistry {
    fn owned_worker_count(&self) -> usize {
        self.opening.len() + self.active.len() + self.draining.len()
    }
}

pub struct EventTeardownBatch {
    opening: Vec<Arc<OpeningState>>,
    draining_ids: Vec<u64>,
    mutations: Vec<Arc<MutationState>>,
}

pub struct EventController {
    repository: Mutex<Box<dyn EventSettingsRepository>>,
    credentials: Arc<dyn CredentialStore>,
    backend: Arc<dyn EventBackend>,
    event_index: Arc<Mutex<Option<EventIndex>>>,
    persisted_event_sink: Mutex<Arc<dyn PersistedEventSink>>,
    retention_days: Mutex<Option<u32>>,
    accepting: AtomicBool,
    lifecycle_generation: AtomicU64,
    reservation_ids: AtomicU64,
    session_ids: AtomicU64,
    mutation_ids: AtomicU64,
    registry: Mutex<EventRegistry>,
    commit_gate: Mutex<()>,
}

struct EventMutationLease<'a> {
    controller: &'a EventController,
    state: Arc<MutationState>,
}

impl Drop for EventMutationLease<'_> {
    fn drop(&mut self) {
        self.controller.finish_mutation(&self.state);
    }
}

impl std::fmt::Debug for EventController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventController").finish_non_exhaustive()
    }
}

impl EventController {
    pub fn production(
        repository: Box<dyn EventSettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
        event_index: Option<EventIndex>,
        retention_days: Option<u32>,
    ) -> Result<Self, EventError> {
        let backend = Arc::new(OnvifClient::new().map_err(map_protocol_error)?);
        Ok(Self::with_backend(
            repository,
            credentials,
            backend,
            event_index,
            retention_days,
        ))
    }

    pub fn with_backend(
        repository: Box<dyn EventSettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
        backend: Arc<dyn EventBackend>,
        event_index: Option<EventIndex>,
        retention_days: Option<u32>,
    ) -> Self {
        Self {
            repository: Mutex::new(repository),
            credentials,
            backend,
            event_index: Arc::new(Mutex::new(event_index)),
            persisted_event_sink: Mutex::new(Arc::new(NoopPersistedEventSink)),
            retention_days: Mutex::new(retention_days),
            accepting: AtomicBool::new(true),
            lifecycle_generation: AtomicU64::new(0),
            reservation_ids: AtomicU64::new(0),
            session_ids: AtomicU64::new(0),
            mutation_ids: AtomicU64::new(0),
            registry: Mutex::new(EventRegistry::default()),
            commit_gate: Mutex::new(()),
        }
    }

    /// Replaces the dedicated runtime Event index. The caller must first
    /// settle Event workers so no transition can race a storage-root switch.
    pub fn configure_event_storage(
        &self,
        index: Option<EventIndex>,
        retention_days: Option<u32>,
    ) -> Result<(), EventError> {
        if self.owned_worker_count() != 0 {
            return Err(EventError::Busy);
        }
        *self.event_index.lock().map_err(|_| EventError::Internal)? = index;
        *self
            .retention_days
            .lock()
            .map_err(|_| EventError::Internal)? = retention_days;
        Ok(())
    }

    pub fn configure_retention_days(&self, retention_days: Option<u32>) -> Result<(), EventError> {
        if self.owned_worker_count() != 0 {
            return Err(EventError::Busy);
        }
        *self
            .retention_days
            .lock()
            .map_err(|_| EventError::Internal)? = retention_days;
        Ok(())
    }

    pub fn event_index_configured(&self) -> bool {
        self.event_index
            .lock()
            .map(|index| index.is_some())
            .unwrap_or(false)
    }

    pub fn set_persisted_event_sink(
        &self,
        sink: Arc<dyn PersistedEventSink>,
    ) -> Result<(), EventError> {
        *self
            .persisted_event_sink
            .lock()
            .map_err(|_| EventError::Internal)? = sink;
        Ok(())
    }

    pub fn configured(&self, camera_id: &str) -> Result<bool, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        Ok(self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .get_event_binding(&camera_id)
            .map_err(|_| EventError::Settings)?
            .is_some())
    }

    pub fn pair(
        &self,
        camera_id: &str,
        prepared: PreparedEventPairing,
    ) -> Result<EventMutation<EventStatusDto>, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        let mutation = self.acquire_camera_mutation(&camera_id, true)?;
        let camera = self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .get_camera(&camera_id)
            .map_err(|_| EventError::Settings)?
            .ok_or(EventError::CameraNotFound)?;
        let authority = parse_device_service(&prepared.device_service)?;
        let CameraSource::Rtsp(endpoint) = camera.source();
        if !endpoint
            .host()
            .as_str()
            .eq_ignore_ascii_case(authority.host.as_str())
        {
            return Err(EventError::AuthorityMismatch);
        }
        if !prepared.control.properties().motion_supported {
            return Err(EventError::Unsupported);
        }

        let old_binding = self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .get_event_binding(&camera_id)
            .map_err(|_| EventError::Settings)?;
        let camera_credentials = self.credentials.get(camera.credential_ref())?;
        let (credential_ref, owns_credential, wrote_new) =
            if camera_credentials == prepared.credentials {
                (camera.credential_ref().clone(), false, false)
            } else {
                let reference = self.allocate_event_credential_ref(&camera_id)?;
                self.credentials.put(&reference, &prepared.credentials)?;
                (reference, true, true)
            };
        let binding = EventBinding::new(
            camera_id.clone(),
            authority.scheme,
            authority.host,
            authority.port,
            authority.path,
            prepared.endpoint_reference,
            credential_ref.clone(),
            owns_credential,
        )
        .map_err(|_| EventError::AuthorityMismatch)?;

        let commit_result = (|| {
            let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
            self.require_generation(mutation.state.lifecycle_generation)?;
            self.require_mutation_registered(&mutation.state)?;
            self.repository
                .lock()
                .map_err(|_| EventError::Internal)?
                .save_event_binding(&binding)
                .map_err(|_| EventError::Settings)
        })();
        if let Err(error) = commit_result {
            if wrote_new && self.credentials.delete(&credential_ref).is_err() {
                return Err(EventError::CredentialRollbackCleanup);
            }
            return Err(error);
        }
        let mut warning = None;
        if let Some(old) = old_binding
            && old.owns_credential()
            && old.credential_ref() != binding.credential_ref()
            && self.credentials.delete(old.credential_ref()).is_err()
        {
            warning = Some(EventWarning::OrphanCredentialCleanupFailed);
        }
        let desired = self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .event_monitoring_enabled(&camera_id)
            .map_err(|_| EventError::Settings)?;
        drop(mutation);
        if desired
            && let Err(error) = self.ensure_session(&camera_id)
            && !matches!(error, EventError::Busy)
        {
            self.set_failed_status(&camera_id, error_code(&error));
        }
        Ok(EventMutation {
            value: self.status_for(&camera_id)?,
            warning,
        })
    }

    pub fn enable(&self, camera_id: &str) -> Result<EventStatusDto, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        let mutation = self.acquire_camera_mutation(&camera_id, false)?;
        let commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
        self.require_generation(mutation.state.lifecycle_generation)?;
        self.require_mutation_registered(&mutation.state)?;
        let mut repository = self.repository.lock().map_err(|_| EventError::Internal)?;
        if repository
            .get_event_binding(&camera_id)
            .map_err(|_| EventError::Settings)?
            .is_none()
        {
            return Err(EventError::NotConfigured);
        }
        if !repository
            .set_event_monitoring_enabled(&camera_id, true)
            .map_err(|_| EventError::Settings)?
        {
            return Err(EventError::CameraNotFound);
        }
        drop(repository);
        drop(commit);
        drop(mutation);
        if let Err(error) = self.ensure_session(&camera_id)
            && !matches!(error, EventError::Busy)
        {
            self.set_failed_status(&camera_id, error_code(&error));
        }
        self.status_for(&camera_id)
    }

    pub fn disable(&self, camera_id: &str) -> Result<EventStatusDto, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        let mutation = self.acquire_camera_mutation(&camera_id, false)?;
        {
            let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
            self.require_generation(mutation.state.lifecycle_generation)?;
            self.require_mutation_registered(&mutation.state)?;
            let mut repository = self.repository.lock().map_err(|_| EventError::Internal)?;
            if !repository
                .set_event_monitoring_enabled(&camera_id, false)
                .map_err(|_| EventError::Settings)?
            {
                return Err(EventError::CameraNotFound);
            }
        }
        self.retire_camera(&camera_id);
        self.reap_camera(&camera_id);
        self.status_for(&camera_id)
    }

    pub fn unpair(&self, camera_id: &str) -> Result<EventMutation<EventStatusDto>, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        let mutation = self.acquire_camera_mutation(&camera_id, true)?;
        let binding = {
            let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
            self.require_generation(mutation.state.lifecycle_generation)?;
            self.require_mutation_registered(&mutation.state)?;
            self.repository
                .lock()
                .map_err(|_| EventError::Internal)?
                .disable_and_delete_event_binding(&camera_id)
                .map_err(|_| EventError::Settings)?
        };
        let warning = if let Some(binding) = binding
            && binding.owns_credential()
            && self.credentials.delete(binding.credential_ref()).is_err()
        {
            Some(EventWarning::OrphanCredentialCleanupFailed)
        } else {
            None
        };
        Ok(EventMutation {
            value: self.status_for(&camera_id)?,
            warning,
        })
    }

    pub fn status(&self, camera_id: &str) -> Result<EventStatusDto, EventError> {
        self.status_for(&parse_camera_id(camera_id)?)
    }

    pub fn statuses(&self) -> Result<Vec<EventStatusDto>, EventError> {
        let bases = {
            let repository = self.repository.lock().map_err(|_| EventError::Internal)?;
            let camera_ids = repository
                .event_status_camera_ids()
                .map_err(|_| EventError::Settings)?;
            let mut bases = Vec::with_capacity(camera_ids.len());
            for camera_id in camera_ids {
                let configured = repository
                    .get_event_binding(&camera_id)
                    .map_err(|_| EventError::Settings)?
                    .is_some();
                let desired = repository
                    .event_monitoring_enabled(&camera_id)
                    .map_err(|_| EventError::Settings)?;
                bases.push((camera_id, configured, desired));
            }
            bases
        };
        let runtime = {
            let registry = self.registry.lock().map_err(|_| EventError::Internal)?;
            registry
                .statuses
                .iter()
                .map(|(camera_id, status)| (camera_id.clone(), status.clone()))
                .collect::<HashMap<_, _>>()
        };
        Ok(bases
            .into_iter()
            .map(|(camera_id, configured, desired)| {
                let runtime = runtime
                    .get(&camera_id)
                    .and_then(|status| status.lock().ok().map(|value| value.clone()))
                    .unwrap_or(RuntimeStatus {
                        state: EventRuntimeState::Disabled,
                        motion_active: None,
                        last_event_at: None,
                        last_error_code: None,
                    });
                EventStatusDto {
                    camera_id: camera_id.as_str().to_owned(),
                    configured,
                    desired,
                    state: if desired {
                        runtime.state
                    } else {
                        EventRuntimeState::Disabled
                    },
                    motion_active: desired.then_some(runtime.motion_active).flatten(),
                    last_event_at: runtime.last_event_at.map(|value| value.to_rfc3339()),
                    last_error_code: runtime.last_error_code,
                }
            })
            .collect())
    }

    pub fn recent(&self, camera_id: &str, limit: u32) -> Result<Vec<EventHistoryDto>, EventError> {
        let camera_id = parse_camera_id(camera_id)?;
        let index = self.event_index.lock().map_err(|_| EventError::Internal)?;
        let index = index.as_ref().ok_or(EventError::PersistenceFailed)?;
        let rows = index
            .recent(&camera_id, limit)
            .map_err(|_| EventError::PersistenceFailed)?;
        Ok(rows
            .into_iter()
            .map(|row| EventHistoryDto {
                event_id: row.event_id,
                camera_id: row.camera_id.as_str().to_owned(),
                kind: match row.kind {
                    EventKind::MotionStarted => EventHistoryKind::MotionStarted,
                    EventKind::MotionEnded => EventHistoryKind::MotionEnded,
                },
                source_key: row.source_key,
                device_time_utc: row.device_time_utc.map(|value| value.to_rfc3339()),
                received_time_utc: row.received_time_utc.to_rfc3339(),
            })
            .collect())
    }

    pub fn review_query(
        &self,
        camera_ids: &[String],
        kind: Option<EventHistoryKind>,
        from_utc: DateTime<Utc>,
        to_utc: DateTime<Utc>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<EventReviewPageDto, EventError> {
        if camera_ids.len() > MAX_EVENT_QUERY_CAMERAS {
            return Err(EventError::InvalidQuery(format!(
                "camera count exceeds {MAX_EVENT_QUERY_CAMERAS}"
            )));
        }
        let camera_ids = camera_ids
            .iter()
            .map(|camera_id| {
                CameraId::parse(camera_id)
                    .map_err(|_| EventError::InvalidQuery("malformed camera id".to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cursor = cursor
            .map(EventCursor::decode)
            .transpose()
            .map_err(map_event_query_index_error)?;
        let page = {
            let index = self.event_index.lock().map_err(|_| EventError::Internal)?;
            let index = index.as_ref().ok_or(EventError::PersistenceFailed)?;
            index
                .query(&EventQuery {
                    camera_ids,
                    kind: kind.map(index_kind),
                    from_utc,
                    to_utc,
                    limit,
                    cursor,
                })
                .map_err(map_event_query_index_error)?
        };
        self.review_page_dto(page)
    }

    pub fn review_get(&self, event_id: u64) -> Result<Option<EventReviewRowDto>, EventError> {
        let row = {
            let index = self.event_index.lock().map_err(|_| EventError::Internal)?;
            let index = index.as_ref().ok_or(EventError::PersistenceFailed)?;
            index
                .get(event_id)
                .map_err(|_| EventError::PersistenceFailed)?
        };
        row.map(|row| self.review_row_dto(row)).transpose()
    }

    fn review_page_dto(
        &self,
        page: nian_index::EventPage,
    ) -> Result<EventReviewPageDto, EventError> {
        let rows = page
            .rows
            .into_iter()
            .map(|row| self.review_row_dto(row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EventReviewPageDto {
            rows,
            next_cursor: page.next_cursor.map(EventCursor::encode),
        })
    }

    fn review_row_dto(
        &self,
        row: nian_index::EventRecord,
    ) -> Result<EventReviewRowDto, EventError> {
        let camera_display_name = self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .get_camera(&row.camera_id)
            .map_err(|_| EventError::Settings)?
            .map(|camera| camera.display_name().to_owned())
            .unwrap_or_else(|| row.camera_id.as_str().to_owned());
        Ok(EventReviewRowDto {
            event_id: row.event_id,
            camera_id: row.camera_id.as_str().to_owned(),
            camera_display_name,
            kind: history_kind(row.kind),
            device_time_utc: row.device_time_utc.map(|value| value.to_rfc3339()),
            received_time_utc: row.received_time_utc.to_rfc3339(),
        })
    }

    pub fn restore_desired(&self) -> Result<(), EventError> {
        self.require_accepting()?;
        let cameras = self
            .repository
            .lock()
            .map_err(|_| EventError::Internal)?
            .event_monitoring_enabled_cameras()
            .map_err(|_| EventError::Settings)?;
        for camera_id in cameras {
            if let Err(error) = self.ensure_session(&camera_id)
                && !matches!(error, EventError::Busy)
            {
                self.set_failed_status(&camera_id, error_code(&error));
            }
        }
        Ok(())
    }

    pub fn stop_accepting_and_stop_all(&self) -> Result<(), EventError> {
        let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
        self.accepting.store(false, Ordering::Release);
        self.lifecycle_generation.fetch_add(1, Ordering::AcqRel);
        let mut registry = self.registry.lock().map_err(|_| EventError::Internal)?;
        for opening in registry.opening.values() {
            opening.cancelled.store(true, Ordering::Release);
        }
        for mutation in registry.mutating.values() {
            mutation.cancel();
        }
        let active = std::mem::take(&mut registry.active);
        for (_, handle) in active {
            handle.session.cancel.store(true, Ordering::Release);
            if let Ok(mut status) = handle.session.status.lock() {
                status.state = EventRuntimeState::Stopping;
                status.motion_active = None;
            }
            let drain = DrainState::from_handle(handle);
            registry.draining.insert(drain.session.session_id, drain);
        }
        Ok(())
    }

    pub fn begin_shutdown_sessions(&self) -> EventTeardownBatch {
        let mut opening = Vec::new();
        let mut draining_ids = Vec::new();
        let mut mutations = Vec::new();
        if let Ok(registry) = self.registry.lock() {
            opening.extend(registry.opening.values().cloned());
            draining_ids.extend(registry.draining.keys().copied());
            mutations.extend(registry.mutating.values().cloned());
        }
        EventTeardownBatch {
            opening,
            draining_ids,
            mutations,
        }
    }

    pub fn finish_shutdown_sessions(&self, batch: EventTeardownBatch) {
        for opening in batch.opening {
            wait_opening(&opening);
        }
        for id in batch.draining_ids {
            self.reap_drain(id);
        }
        for mutation in batch.mutations {
            self.wait_mutation(&mutation);
        }
    }

    pub fn shutdown_sessions(&self) {
        let _ = self.stop_accepting_and_stop_all();
        loop {
            let batch = self.begin_shutdown_sessions();
            if batch.opening.is_empty()
                && batch.draining_ids.is_empty()
                && batch.mutations.is_empty()
            {
                break;
            }
            self.finish_shutdown_sessions(batch);
        }
    }

    pub fn resume_accepting(&self) {
        if let Ok(_commit) = self.commit_gate.lock() {
            self.accepting.store(true, Ordering::Release);
        }
    }

    pub fn owned_worker_count(&self) -> usize {
        self.registry
            .lock()
            .map(|registry| registry.owned_worker_count())
            .unwrap_or(MAX_ACTIVE_EVENT_SESSIONS)
    }

    #[cfg(test)]
    fn ownership_counts(&self) -> (usize, usize, usize, usize) {
        self.registry
            .lock()
            .map(|registry| {
                (
                    registry.opening.len(),
                    registry.active.len(),
                    registry.draining.len(),
                    registry.mutating.len(),
                )
            })
            .unwrap_or((usize::MAX, usize::MAX, usize::MAX, usize::MAX))
    }

    fn ensure_session(&self, camera_id: &CameraId) -> Result<EventStatusDto, EventError> {
        let reservation_id = self.reservation_ids.fetch_add(1, Ordering::AcqRel) + 1;
        let opening;
        {
            let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
            self.require_accepting()?;
            if !self.event_index_configured() {
                return Err(EventError::PersistenceFailed);
            }
            let lifecycle_generation = self.lifecycle_generation.load(Ordering::Acquire);
            opening = Arc::new(OpeningState::new(reservation_id, lifecycle_generation));
            let mut registry = self.registry.lock().map_err(|_| EventError::Internal)?;
            if registry.mutating.contains_key(camera_id) {
                return Err(EventError::Busy);
            }
            if registry.active.contains_key(camera_id)
                || registry.opening.contains_key(camera_id)
                || registry
                    .draining
                    .values()
                    .any(|handle| &handle.camera_id == camera_id)
            {
                return Err(EventError::Busy);
            }
            if registry.owned_worker_count() >= MAX_ACTIVE_EVENT_SESSIONS {
                return Err(EventError::Capacity);
            }
            registry.opening.insert(camera_id.clone(), opening.clone());
            registry
                .statuses
                .entry(camera_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(RuntimeStatus::starting())));
        }

        let result = self.prepare_session(camera_id, &opening);
        mark_opening_done(&opening);
        if result.is_err()
            && let Ok(mut registry) = self.registry.lock()
            && registry
                .opening
                .get(camera_id)
                .is_some_and(|current| current.reservation_id == reservation_id)
        {
            registry.opening.remove(camera_id);
        }
        result
    }

    fn prepare_session(
        &self,
        camera_id: &CameraId,
        opening: &Arc<OpeningState>,
    ) -> Result<EventStatusDto, EventError> {
        let (camera, binding) = {
            let repository = self.repository.lock().map_err(|_| EventError::Internal)?;
            let camera = repository
                .get_camera(camera_id)
                .map_err(|_| EventError::Settings)?
                .ok_or(EventError::CameraNotFound)?;
            let binding = repository
                .get_event_binding(camera_id)
                .map_err(|_| EventError::Settings)?
                .ok_or(EventError::NotConfigured)?;
            if !repository
                .event_monitoring_enabled(camera_id)
                .map_err(|_| EventError::Settings)?
            {
                return Err(EventError::LifecycleCancelled);
            }
            (camera, binding)
        };
        validate_runtime_authority(&camera, &binding)?;
        if opening.cancelled.load(Ordering::Acquire) {
            return Err(EventError::LifecycleCancelled);
        }
        let credentials = self.credentials.get(binding.credential_ref())?;
        let onvif_credentials = OnvifCredentials {
            username: credentials.username.clone(),
            password: credentials.password().to_owned(),
        };
        let device_service = binding_device_service(&binding);
        let control = self
            .backend
            .control(&device_service, &onvif_credentials)
            .map_err(map_protocol_error)?;
        if !control.properties().motion_supported {
            return Err(EventError::Unsupported);
        }
        if opening.cancelled.load(Ordering::Acquire) {
            return Err(EventError::LifecycleCancelled);
        }

        let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
        self.require_generation(opening.lifecycle_generation)?;
        if opening.cancelled.load(Ordering::Acquire) {
            return Err(EventError::LifecycleCancelled);
        }
        let (current_binding, desired) = {
            let repository = self.repository.lock().map_err(|_| EventError::Internal)?;
            let binding = repository
                .get_event_binding(camera_id)
                .map_err(|_| EventError::Settings)?
                .ok_or(EventError::NotConfigured)?;
            let desired = repository
                .event_monitoring_enabled(camera_id)
                .map_err(|_| EventError::Settings)?;
            (binding, desired)
        };
        if !desired || current_binding != binding {
            return Err(EventError::LifecycleCancelled);
        }
        let mut registry = self.registry.lock().map_err(|_| EventError::Internal)?;
        let current = registry
            .opening
            .get(camera_id)
            .ok_or(EventError::LifecycleCancelled)?;
        if current.reservation_id != opening.reservation_id {
            return Err(EventError::LifecycleCancelled);
        }
        let status = registry
            .statuses
            .entry(camera_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(RuntimeStatus::starting())))
            .clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let session_id = self.session_ids.fetch_add(1, Ordering::AcqRel) + 1;
        let worker_camera = camera_id.clone();
        let worker_backend = self.backend.clone();
        let worker_index = self.event_index.clone();
        let worker_display_name = camera.display_name().to_owned();
        let worker_sink = self
            .persisted_event_sink
            .lock()
            .map_err(|_| EventError::Internal)?
            .clone();
        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let worker_retention = *self
            .retention_days
            .lock()
            .map_err(|_| EventError::Internal)?;
        let worker_credentials = onvif_credentials.clone();
        let worker_device_service = device_service.clone();
        let join = thread::Builder::new()
            .name(format!("nian-events-{}", camera_id.as_str()))
            .spawn(move || {
                event_worker(
                    worker_camera,
                    worker_display_name,
                    worker_device_service,
                    worker_credentials,
                    worker_backend,
                    worker_index,
                    worker_sink,
                    worker_status,
                    worker_cancel,
                    worker_retention,
                );
            })
            .map_err(|_| EventError::Internal)?;
        let handle = EventSessionHandle {
            camera_id: camera_id.clone(),
            session: EventSessionRef {
                session_id,
                cancel,
                status,
            },
            join,
        };
        registry.opening.remove(camera_id);
        registry.active.insert(camera_id.clone(), handle);
        drop(registry);
        drop(_commit);
        self.status_for(camera_id)
    }

    fn retire_camera(&self, camera_id: &CameraId) {
        if let Ok(mut registry) = self.registry.lock() {
            if let Some(opening) = registry.opening.get(camera_id) {
                opening.cancelled.store(true, Ordering::Release);
            }
            if let Some(handle) = registry.active.remove(camera_id) {
                handle.session.cancel.store(true, Ordering::Release);
                if let Ok(mut status) = handle.session.status.lock() {
                    status.state = EventRuntimeState::Stopping;
                    status.motion_active = None;
                }
                let drain = DrainState::from_handle(handle);
                registry.draining.insert(drain.session.session_id, drain);
            }
        }
    }

    fn reap_camera(&self, camera_id: &CameraId) {
        loop {
            let opening = self
                .registry
                .lock()
                .ok()
                .and_then(|registry| registry.opening.get(camera_id).cloned());
            if let Some(opening) = opening {
                wait_opening(&opening);
            }
            let drain = self.registry.lock().ok().and_then(|registry| {
                registry
                    .draining
                    .iter()
                    .find_map(|(id, handle)| (&handle.camera_id == camera_id).then_some(*id))
            });
            let Some(id) = drain else {
                break;
            };
            self.reap_drain(id);
        }
    }

    fn reap_drain(&self, session_id: u64) {
        let drain = self
            .registry
            .lock()
            .ok()
            .and_then(|registry| registry.draining.get(&session_id).cloned());
        let Some(drain) = drain else {
            return;
        };

        let join = drain.join.lock().ok().and_then(|mut join| join.take());
        if let Some(join) = join {
            let camera_id = drain.camera_id.clone();
            let _ = join.join();
            let desired = self
                .repository
                .lock()
                .ok()
                .and_then(|repository| repository.event_monitoring_enabled(&camera_id).ok())
                .unwrap_or(false);
            if let Ok(mut registry) = self.registry.lock() {
                if registry
                    .draining
                    .get(&session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &drain))
                {
                    registry.draining.remove(&session_id);
                }
                let still_owned = registry.active.contains_key(&camera_id)
                    || registry.opening.contains_key(&camera_id)
                    || registry
                        .draining
                        .values()
                        .any(|other| other.camera_id == camera_id);
                if !still_owned
                    && let Some(status) = registry.statuses.get(&camera_id)
                    && let Ok(mut status) = status.lock()
                {
                    status.motion_active = None;
                    if desired && self.accepting.load(Ordering::Acquire) {
                        status.state = EventRuntimeState::Failed;
                        status.last_error_code = Some("worker_exited".to_owned());
                    } else {
                        status.state = EventRuntimeState::Disabled;
                    }
                }
            }
            if let Ok(mut done) = drain.done.lock() {
                *done = true;
                drain.done_cv.notify_all();
            }
        } else {
            drain.wait_done();
        }
    }

    fn status_for(&self, camera_id: &CameraId) -> Result<EventStatusDto, EventError> {
        let repository = self.repository.lock().map_err(|_| EventError::Internal)?;
        let configured = repository
            .get_event_binding(camera_id)
            .map_err(|_| EventError::Settings)?
            .is_some();
        let desired = repository
            .event_monitoring_enabled(camera_id)
            .map_err(|_| EventError::Settings)?;
        drop(repository);
        let status = self
            .registry
            .lock()
            .map_err(|_| EventError::Internal)?
            .statuses
            .get(camera_id)
            .cloned();
        let runtime = status
            .as_ref()
            .and_then(|status| status.lock().ok().map(|value| value.clone()))
            .unwrap_or(RuntimeStatus {
                state: EventRuntimeState::Disabled,
                motion_active: None,
                last_event_at: None,
                last_error_code: None,
            });
        Ok(EventStatusDto {
            camera_id: camera_id.as_str().to_owned(),
            configured,
            desired,
            state: if desired {
                runtime.state
            } else {
                EventRuntimeState::Disabled
            },
            motion_active: desired.then_some(runtime.motion_active).flatten(),
            last_event_at: runtime.last_event_at.map(|value| value.to_rfc3339()),
            last_error_code: runtime.last_error_code,
        })
    }

    fn set_failed_status(&self, camera_id: &CameraId, code: &'static str) {
        if let Ok(mut registry) = self.registry.lock() {
            let status = registry
                .statuses
                .entry(camera_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(RuntimeStatus::starting())))
                .clone();
            if let Ok(mut status) = status.lock() {
                status.state = EventRuntimeState::Failed;
                status.last_error_code = Some(code.to_owned());
                status.motion_active = None;
            }
        }
    }

    pub fn coordinate_camera_update<T, F>(
        &self,
        camera_id: &CameraId,
        operation: F,
    ) -> Result<T, EventError>
    where
        F: FnOnce() -> T,
    {
        let _mutation = self.acquire_camera_mutation(camera_id, false)?;
        Ok(operation())
    }

    pub fn coordinate_camera_delete<T, F>(
        &self,
        camera_id: &CameraId,
        operation: F,
    ) -> Result<T, EventError>
    where
        F: FnOnce() -> T,
    {
        let _mutation = self.acquire_camera_mutation(camera_id, true)?;
        Ok(operation())
    }

    fn acquire_camera_mutation(
        &self,
        camera_id: &CameraId,
        retire: bool,
    ) -> Result<EventMutationLease<'_>, EventError> {
        let mutation_id = self.mutation_ids.fetch_add(1, Ordering::AcqRel) + 1;
        let (state, opening, drain_ids) = {
            let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
            self.require_accepting()?;
            let lifecycle_generation = self.lifecycle_generation.load(Ordering::Acquire);
            let mut registry = self.registry.lock().map_err(|_| EventError::Internal)?;
            if registry.mutating.contains_key(camera_id) {
                return Err(EventError::Busy);
            }
            let state = Arc::new(MutationState::new(
                camera_id.clone(),
                mutation_id,
                lifecycle_generation,
            ));
            registry.mutating.insert(camera_id.clone(), state.clone());
            let opening = registry.opening.get(camera_id).cloned();
            if let Some(opening) = &opening {
                opening.cancelled.store(true, Ordering::Release);
            }
            if retire && let Some(handle) = registry.active.remove(camera_id) {
                handle.session.cancel.store(true, Ordering::Release);
                if let Ok(mut status) = handle.session.status.lock() {
                    status.state = EventRuntimeState::Stopping;
                    status.motion_active = None;
                }
                let drain = DrainState::from_handle(handle);
                registry.draining.insert(drain.session.session_id, drain);
            }
            let drain_ids = registry
                .draining
                .iter()
                .filter_map(|(id, handle)| (&handle.camera_id == camera_id).then_some(*id))
                .collect::<Vec<_>>();
            (state, opening, drain_ids)
        };
        let lease = EventMutationLease {
            controller: self,
            state,
        };
        if let Some(opening) = opening {
            wait_opening(&opening);
        }
        for id in drain_ids {
            self.reap_drain(id);
        }
        self.require_mutation_current(&lease.state)?;
        Ok(lease)
    }

    fn require_mutation_current(&self, mutation: &Arc<MutationState>) -> Result<(), EventError> {
        let _commit = self.commit_gate.lock().map_err(|_| EventError::Internal)?;
        self.require_generation(mutation.lifecycle_generation)?;
        self.require_mutation_registered(mutation)
    }

    fn require_mutation_registered(&self, mutation: &Arc<MutationState>) -> Result<(), EventError> {
        if mutation.cancelled.load(Ordering::Acquire) {
            return Err(EventError::LifecycleCancelled);
        }
        let registry = self.registry.lock().map_err(|_| EventError::Internal)?;
        let current = registry
            .mutating
            .get(&mutation.camera_id)
            .is_some_and(|state| {
                Arc::ptr_eq(state, mutation) && state.mutation_id == mutation.mutation_id
            });
        current.then_some(()).ok_or(EventError::LifecycleCancelled)
    }

    fn finish_mutation(&self, mutation: &Arc<MutationState>) {
        if let Ok(mut registry) = self.registry.lock()
            && registry
                .mutating
                .get(&mutation.camera_id)
                .is_some_and(|current| Arc::ptr_eq(current, mutation))
        {
            registry.mutating.remove(&mutation.camera_id);
            mark_mutation_done(mutation);
            return;
        }
        mark_mutation_done(mutation);
    }

    fn wait_mutation(&self, mutation: &Arc<MutationState>) {
        if let Ok(mut done) = mutation.done.lock() {
            while !*done {
                match mutation.done_cv.wait(done) {
                    Ok(next) => done = next,
                    Err(_) => break,
                }
            }
        }
    }

    fn allocate_event_credential_ref(
        &self,
        camera_id: &CameraId,
    ) -> Result<CredentialRef, EventError> {
        for _ in 0..MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS {
            let reference = CredentialRef::parse(format!(
                "nian-vision/{}/events/{}",
                camera_id.as_str(),
                Uuid::new_v4()
            ))
            .map_err(|_| EventError::Internal)?;
            if self.credentials.get(&reference).is_err() {
                return Ok(reference);
            }
        }
        Err(EventError::Internal)
    }

    fn require_accepting(&self) -> Result<(), EventError> {
        if self.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(EventError::LifecycleCancelled)
        }
    }

    fn require_generation(&self, generation: u64) -> Result<(), EventError> {
        if self.accepting.load(Ordering::Acquire)
            && self.lifecycle_generation.load(Ordering::Acquire) == generation
        {
            Ok(())
        } else {
            Err(EventError::LifecycleCancelled)
        }
    }
}

impl Drop for EventController {
    fn drop(&mut self) {
        self.shutdown_sessions();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionState {
    Unknown,
    Idle,
    Active,
}

struct MotionDecision {
    source_key: Option<String>,
    next: MotionState,
    transition: Option<EventInsert>,
    track_source: bool,
    overflowed: bool,
}

#[derive(Default)]
struct MotionNormalizer {
    sources: HashMap<Option<String>, MotionState>,
    overflowed: bool,
}

impl MotionNormalizer {
    fn prepare(
        &self,
        camera_id: &CameraId,
        notification: &MotionNotification,
        received_time_utc: DateTime<Utc>,
    ) -> MotionDecision {
        let key = notification.source_key.clone();
        let current = self
            .sources
            .get(&key)
            .copied()
            .unwrap_or(MotionState::Unknown);
        let next = if notification.active {
            MotionState::Active
        } else {
            MotionState::Idle
        };
        let source_is_known = self.sources.contains_key(&key);
        if current == MotionState::Unknown
            && !source_is_known
            && self.sources.len() >= MAX_EVENT_SOURCES_PER_SESSION
        {
            return MotionDecision {
                source_key: key,
                next,
                transition: None,
                track_source: false,
                overflowed: true,
            };
        }

        let transition = if notification.synchronization_baseline {
            None
        } else {
            let kind = match (current, next) {
                (MotionState::Unknown, MotionState::Active)
                | (MotionState::Idle, MotionState::Active) => Some(EventKind::MotionStarted),
                (MotionState::Active, MotionState::Idle) => Some(EventKind::MotionEnded),
                (MotionState::Unknown, MotionState::Idle)
                | (MotionState::Idle, MotionState::Idle)
                | (MotionState::Active, MotionState::Active)
                | (MotionState::Unknown, MotionState::Unknown)
                | (MotionState::Idle, MotionState::Unknown)
                | (MotionState::Active, MotionState::Unknown) => None,
            };
            kind.map(|kind| EventInsert {
                camera_id: camera_id.clone(),
                kind,
                source_key: key.clone(),
                device_time_utc: notification.device_time_utc,
                received_time_utc,
                fingerprint: notification.device_time_utc.map(|device_time| {
                    event_fingerprint(camera_id, kind, key.as_deref(), device_time)
                }),
            })
        };

        MotionDecision {
            source_key: key,
            next,
            transition,
            track_source: true,
            overflowed: false,
        }
    }

    fn commit(&mut self, decision: MotionDecision) {
        if decision.overflowed {
            self.overflowed = true;
        }
        if decision.track_source {
            self.sources.insert(decision.source_key, decision.next);
        }
    }

    fn aggregate_motion(&self) -> Option<bool> {
        if self.overflowed {
            return None;
        }
        if self
            .sources
            .values()
            .any(|state| *state == MotionState::Active)
        {
            Some(true)
        } else if self
            .sources
            .values()
            .any(|state| *state == MotionState::Idle)
        {
            Some(false)
        } else {
            None
        }
    }
}

fn persist_motion_transition(
    event_index: &Mutex<Option<EventIndex>>,
    insert: &EventInsert,
    retention_days: Option<u32>,
) -> Result<Option<u64>, ()> {
    let mut index = event_index.lock().map_err(|_| ())?;
    let index = index.as_mut().ok_or(())?;
    index
        .insert_and_cleanup(insert, Utc::now(), retention_days)
        .map_err(|_| ())
}

#[derive(Clone, Copy)]
struct MotionNotificationProjection<'a> {
    camera_display_name: &'a str,
    persisted_event_sink: &'a dyn PersistedEventSink,
}

fn apply_motion_notification_with_sink(
    normalizer: &mut MotionNormalizer,
    camera_id: &CameraId,
    notification: &MotionNotification,
    received_time_utc: DateTime<Utc>,
    event_index: &Mutex<Option<EventIndex>>,
    projection: MotionNotificationProjection<'_>,
    retention_days: Option<u32>,
) -> Result<(), ()> {
    let decision = normalizer.prepare(camera_id, notification, received_time_utc);
    if let Some(insert) = decision.transition.as_ref()
        && let Some(event_id) = persist_motion_transition(event_index, insert, retention_days)?
    {
        projection
            .persisted_event_sink
            .try_publish(PersistedEventSignal {
                event_id,
                camera_id: camera_id.as_str().to_owned(),
                camera_display_name: projection.camera_display_name.to_owned(),
                kind: history_kind(insert.kind),
                received_time_utc: insert.received_time_utc,
            });
    }
    normalizer.commit(decision);
    Ok(())
}

#[cfg(test)]
fn apply_motion_notification(
    normalizer: &mut MotionNormalizer,
    camera_id: &CameraId,
    notification: &MotionNotification,
    received_time_utc: DateTime<Utc>,
    event_index: &Mutex<Option<EventIndex>>,
    retention_days: Option<u32>,
) -> Result<(), ()> {
    apply_motion_notification_with_sink(
        normalizer,
        camera_id,
        notification,
        received_time_utc,
        event_index,
        MotionNotificationProjection {
            camera_display_name: camera_id.as_str(),
            persisted_event_sink: &NoopPersistedEventSink,
        },
        retention_days,
    )
}

#[allow(clippy::too_many_arguments)]
fn event_worker(
    camera_id: CameraId,
    camera_display_name: String,
    device_service: String,
    credentials: OnvifCredentials,
    backend: Arc<dyn EventBackend>,
    event_index: Arc<Mutex<Option<EventIndex>>>,
    persisted_event_sink: Arc<dyn PersistedEventSink>,
    status: Arc<Mutex<RuntimeStatus>>,
    cancel: Arc<AtomicBool>,
    retention_days: Option<u32>,
) {
    let mut backoff = ReconnectBackoff::default();
    let mut normalizer = MotionNormalizer::default();

    while !cancel.load(Ordering::Acquire) {
        update_status(&status, EventRuntimeState::Starting, None, true);
        let control = match backend.control(&device_service, &credentials) {
            Ok(control) if control.properties().motion_supported => control,
            Ok(_) => {
                update_status(
                    &status,
                    EventRuntimeState::Failed,
                    Some("unsupported"),
                    true,
                );
                wait_failed_until_cancel(&cancel);
                break;
            }
            Err(error) => {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                if is_terminal_control_error(&error) {
                    update_status(
                        &status,
                        EventRuntimeState::Failed,
                        Some(event_control_error_code(&error)),
                        true,
                    );
                    wait_failed_until_cancel(&cancel);
                    break;
                }
                update_status(
                    &status,
                    EventRuntimeState::Backoff,
                    Some(event_control_error_code(&error)),
                    true,
                );
                backend.wait_reconnect(&cancel, backoff.next_delay());
                continue;
            }
        };

        update_status(&status, EventRuntimeState::Subscribing, None, true);
        let mut subscription = match backend.create_subscription(&control, &credentials) {
            Ok(subscription) => subscription,
            Err(error) => {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                update_status(
                    &status,
                    EventRuntimeState::Backoff,
                    Some(subscription_error_code(&error)),
                    true,
                );
                backend.wait_reconnect(&cancel, backoff.next_delay());
                continue;
            }
        };

        let _ = backend.synchronize(&subscription, &credentials);
        if cancel.load(Ordering::Acquire) {
            let _ = backend.unsubscribe(&subscription, &credentials);
            break;
        }
        update_status(&status, EventRuntimeState::Polling, None, true);
        let mut renew_at = match subscription_renew_deadline(&subscription) {
            Ok(deadline) => deadline,
            Err(error) => {
                let _ = backend.unsubscribe(&subscription, &credentials);
                update_status(
                    &status,
                    EventRuntimeState::Backoff,
                    Some(subscription_error_code(&error)),
                    true,
                );
                backend.wait_reconnect(&cancel, backoff.next_delay());
                continue;
            }
        };
        let mut recreate_reason: Option<&'static str> = None;

        while !cancel.load(Ordering::Acquire) {
            if Instant::now() >= renew_at {
                if let Err(error) = backend.renew(&mut subscription, &credentials) {
                    recreate_reason = Some(renew_error_code(&error));
                    break;
                }
                renew_at = match subscription_renew_deadline(&subscription) {
                    Ok(deadline) => deadline,
                    Err(error) => {
                        recreate_reason = Some(subscription_error_code(&error));
                        break;
                    }
                };
            }

            match backend.pull(&subscription, &credentials) {
                Ok(notifications) => {
                    backoff.reset();
                    let mut persistence_failed = false;
                    for notification in notifications {
                        let received = Utc::now();
                        if apply_motion_notification_with_sink(
                            &mut normalizer,
                            &camera_id,
                            &notification,
                            received,
                            &event_index,
                            MotionNotificationProjection {
                                camera_display_name: &camera_display_name,
                                persisted_event_sink: persisted_event_sink.as_ref(),
                            },
                            retention_days,
                        )
                        .is_err()
                        {
                            persistence_failed = true;
                            if let Ok(mut runtime) = status.lock() {
                                runtime.motion_active = None;
                                runtime.last_error_code = Some("persistence_failed".to_owned());
                            }
                            break;
                        }
                        if let Ok(mut runtime) = status.lock() {
                            runtime.motion_active = normalizer.aggregate_motion();
                            runtime.last_event_at = Some(received);
                            runtime.last_error_code = None;
                            runtime.state = EventRuntimeState::Polling;
                        }
                    }
                    if persistence_failed {
                        recreate_reason = Some("persistence_failed");
                        break;
                    }
                }
                Err(OnvifError::Timeout) => {
                    // A completed bounded long-poll timeout proves the subscription
                    // stayed healthy for the request window, so it resets the
                    // reconnect failure streak without recreating the subscription.
                    backoff.reset();
                }
                Err(error) => {
                    recreate_reason = Some(pull_error_code(&error));
                    break;
                }
            }
        }

        let _ = backend.unsubscribe(&subscription, &credentials);
        if cancel.load(Ordering::Acquire) {
            break;
        }
        update_status(
            &status,
            EventRuntimeState::Backoff,
            recreate_reason.or(Some("subscription_failed")),
            true,
        );
        backend.wait_reconnect(&cancel, backoff.next_delay());
    }

    if let Ok(mut runtime) = status.lock() {
        runtime.state = EventRuntimeState::Disabled;
        runtime.motion_active = None;
    }
}

fn event_fingerprint(
    camera_id: &CameraId,
    kind: EventKind,
    source_key: Option<&str>,
    device_time: DateTime<Utc>,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(camera_id.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(match kind {
        EventKind::MotionStarted => b"motion_started".as_slice(),
        EventKind::MotionEnded => b"motion_ended".as_slice(),
    });
    hasher.update([0]);
    if let Some(source_key) = source_key {
        hasher.update(source_key.as_bytes());
    }
    hasher.update([0]);
    hasher.update(device_time.timestamp_millis().to_be_bytes());
    hasher.finalize().into()
}

fn subscription_renew_deadline(
    subscription: &PullPointSubscription,
) -> Result<Instant, OnvifError> {
    let lifetime_secs = subscription
        .bounded_lifetime_secs()?
        .unwrap_or(RENEW_FALLBACK_SECS);
    let delay_secs = lifetime_secs.saturating_mul(2) / 3;
    Instant::now()
        .checked_add(Duration::from_secs(delay_secs.max(1)))
        .ok_or(OnvifError::Protocol)
}

fn is_terminal_control_error(error: &OnvifError) -> bool {
    matches!(
        error,
        OnvifError::AuthFailed | OnvifError::Unsupported | OnvifError::AuthorityRejected
    )
}

fn wait_failed_until_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(WORKER_IDLE_SLEEP_MS));
    }
}

fn sleep_cancellable(cancel: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(WORKER_IDLE_SLEEP_MS)));
    }
}

fn update_status(
    status: &Mutex<RuntimeStatus>,
    state: EventRuntimeState,
    error: Option<&str>,
    reset_motion: bool,
) {
    if let Ok(mut status) = status.lock() {
        status.state = state;
        status.last_error_code = error.map(str::to_owned);
        if reset_motion {
            status.motion_active = None;
        }
    }
}

fn mark_opening_done(opening: &OpeningState) {
    if let Ok(mut done) = opening.done.lock() {
        *done = true;
        opening.done_cv.notify_all();
    }
}

fn mark_mutation_done(mutation: &MutationState) {
    if let Ok(mut done) = mutation.done.lock()
        && !*done
    {
        *done = true;
        mutation.done_cv.notify_all();
    }
}

fn wait_opening(opening: &OpeningState) {
    if let Ok(mut done) = opening.done.lock() {
        while !*done {
            match opening.done_cv.wait(done) {
                Ok(next) => done = next,
                Err(_) => break,
            }
        }
    }
}

fn parse_camera_id(camera_id: &str) -> Result<CameraId, EventError> {
    CameraId::parse(camera_id).map_err(|_| EventError::CameraNotFound)
}

fn history_kind(kind: EventKind) -> EventHistoryKind {
    match kind {
        EventKind::MotionStarted => EventHistoryKind::MotionStarted,
        EventKind::MotionEnded => EventHistoryKind::MotionEnded,
    }
}

fn index_kind(kind: EventHistoryKind) -> EventKind {
    match kind {
        EventHistoryKind::MotionStarted => EventKind::MotionStarted,
        EventHistoryKind::MotionEnded => EventKind::MotionEnded,
    }
}

fn map_event_query_index_error(error: IndexError) -> EventError {
    match error {
        IndexError::InvalidData(message) => EventError::InvalidQuery(message),
        _ => EventError::PersistenceFailed,
    }
}

fn error_code(error: &EventError) -> &'static str {
    match error {
        EventError::CameraNotFound => "camera_not_found",
        EventError::NotConfigured => "not_configured",
        EventError::Unsupported => "unsupported",
        EventError::AuthFailed => "auth_failed",
        EventError::DeviceUnreachable => "device_unreachable",
        EventError::SubscriptionFailed => "subscription_failed",
        EventError::PullTimeout => "pull_timeout",
        EventError::ProtocolError => "protocol_error",
        EventError::PersistenceFailed => "persistence_failed",
        EventError::InvalidQuery(_) => "invalid_query",
        EventError::Capacity => "event_capacity",
        EventError::Busy => "event_busy",
        EventError::LifecycleCancelled => "lifecycle_cancelled",
        EventError::AuthorityMismatch => "authority_rejected",
        EventError::Settings => "settings_failed",
        EventError::CredentialStore(_) => "credential_store",
        EventError::CredentialRollbackCleanup => "credential_rollback_cleanup",
        EventError::Internal => "internal",
    }
}

struct DeviceAuthority {
    scheme: OnvifScheme,
    host: Host,
    port: u16,
    path: String,
}

fn parse_device_service(raw: &str) -> Result<DeviceAuthority, EventError> {
    let url = Url::parse(raw).map_err(|_| EventError::AuthorityMismatch)?;
    let scheme = match url.scheme() {
        "http" => OnvifScheme::Http,
        "https" => OnvifScheme::Https,
        _ => return Err(EventError::AuthorityMismatch),
    };
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EventError::AuthorityMismatch);
    }
    let host = Host::parse(url.host_str().ok_or(EventError::AuthorityMismatch)?)
        .map_err(|_| EventError::AuthorityMismatch)?;
    let port = url
        .port_or_known_default()
        .ok_or(EventError::AuthorityMismatch)?;
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

fn binding_device_service(binding: &EventBinding) -> String {
    let default_port = match binding.scheme() {
        OnvifScheme::Http => 80,
        OnvifScheme::Https => 443,
    };
    let host = binding.host().as_str();
    let authority = if binding.port() == default_port {
        host.to_owned()
    } else {
        format!("{host}:{}", binding.port())
    };
    format!(
        "{}://{}{}",
        binding.scheme().as_str(),
        authority,
        binding.device_path()
    )
}

fn validate_runtime_authority(
    camera: &CameraConfig,
    binding: &EventBinding,
) -> Result<(), EventError> {
    if camera.camera_id() != binding.camera_id() {
        return Err(EventError::AuthorityMismatch);
    }
    let CameraSource::Rtsp(endpoint) = camera.source();
    if !endpoint
        .host()
        .as_str()
        .eq_ignore_ascii_case(binding.host().as_str())
    {
        return Err(EventError::AuthorityMismatch);
    }
    Ok(())
}

fn event_control_error_code(error: &OnvifError) -> &'static str {
    match error {
        OnvifError::AuthFailed => "auth_failed",
        OnvifError::Timeout => "event_control_timeout",
        OnvifError::DeviceUnreachable => "device_unreachable",
        OnvifError::Unsupported => "unsupported",
        OnvifError::EventServiceUnsupported => "event_service_unsupported",
        OnvifError::MotionEventUnsupported => "motion_topic_unsupported",
        OnvifError::AuthorityRejected => "authority_rejected",
        OnvifError::Cancelled => "lifecycle_cancelled",
        _ => "event_control_protocol",
    }
}

fn pull_error_code(error: &OnvifError) -> &'static str {
    match error {
        OnvifError::AuthFailed => "auth_failed",
        OnvifError::Timeout => "pull_timeout",
        OnvifError::DeviceUnreachable => "device_unreachable",
        OnvifError::AuthorityRejected => "authority_rejected",
        OnvifError::Cancelled => "lifecycle_cancelled",
        _ => "event_pull_protocol",
    }
}

fn renew_error_code(error: &OnvifError) -> &'static str {
    match error {
        OnvifError::AuthFailed => "auth_failed",
        OnvifError::Timeout => "event_renew_timeout",
        OnvifError::DeviceUnreachable => "device_unreachable",
        OnvifError::AuthorityRejected => "authority_rejected",
        OnvifError::Cancelled => "lifecycle_cancelled",
        _ => "event_renew_protocol",
    }
}

fn subscription_error_code(error: &OnvifError) -> &'static str {
    match error {
        OnvifError::AuthFailed => "auth_failed",
        OnvifError::Timeout => "subscription_failed",
        OnvifError::DeviceUnreachable => "device_unreachable",
        OnvifError::AuthorityRejected => "authority_rejected",
        _ => "subscription_failed",
    }
}

fn map_protocol_error(error: OnvifError) -> EventError {
    match error {
        OnvifError::AuthFailed => EventError::AuthFailed,
        OnvifError::Timeout => EventError::PullTimeout,
        OnvifError::DeviceUnreachable => EventError::DeviceUnreachable,
        OnvifError::Unsupported => EventError::Unsupported,
        OnvifError::Cancelled => EventError::LifecycleCancelled,
        OnvifError::AuthorityRejected => EventError::AuthorityMismatch,
        _ => EventError::ProtocolError,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;
    use std::time::Duration;

    use nian_domain::{AudioPolicy, CameraEndpoint, Credentials, EventBinding, OnvifScheme};
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::MemoryCredentialStore;

    #[derive(Default)]
    struct PullGate {
        entered: bool,
        release: bool,
    }

    struct FakeBackend {
        control_calls: AtomicUsize,
        unsubscribe_calls: AtomicUsize,
        fail_control: AtomicBool,
        unsupported_after_first_control: AtomicBool,
        block_pull: AtomicBool,
        scripted_pull_failures: AtomicUsize,
        scripted_pull_successes: AtomicUsize,
        scripted_pull_protocol_after: AtomicBool,
        skip_backoff_sleep: AtomicBool,
        backoff_delays: Mutex<Vec<u64>>,
        pull_gate: (Mutex<PullGate>, Condvar),
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                control_calls: AtomicUsize::new(0),
                unsubscribe_calls: AtomicUsize::new(0),
                fail_control: AtomicBool::new(false),
                unsupported_after_first_control: AtomicBool::new(false),
                block_pull: AtomicBool::new(false),
                scripted_pull_failures: AtomicUsize::new(0),
                scripted_pull_successes: AtomicUsize::new(0),
                scripted_pull_protocol_after: AtomicBool::new(false),
                skip_backoff_sleep: AtomicBool::new(false),
                backoff_delays: Mutex::new(Vec::new()),
                pull_gate: (Mutex::new(PullGate::default()), Condvar::new()),
            })
        }

        fn wait_for_pull(&self) {
            let (lock, cv) = &self.pull_gate;
            let mut gate = lock.lock().unwrap();
            while !gate.entered {
                gate = cv.wait(gate).unwrap();
            }
        }

        fn release_pull(&self) {
            let (lock, cv) = &self.pull_gate;
            let mut gate = lock.lock().unwrap();
            gate.release = true;
            cv.notify_all();
        }
    }

    impl EventBackend for FakeBackend {
        fn control(
            &self,
            _device_service: &str,
            _credentials: &OnvifCredentials,
        ) -> Result<EventControl, OnvifError> {
            let call = self.control_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_control.load(Ordering::Acquire) {
                Err(OnvifError::DeviceUnreachable)
            } else if self.unsupported_after_first_control.load(Ordering::Acquire) && call >= 1 {
                Err(OnvifError::Unsupported)
            } else {
                Ok(EventControl::test_fixture())
            }
        }

        fn create_subscription(
            &self,
            _control: &EventControl,
            _credentials: &OnvifCredentials,
        ) -> Result<PullPointSubscription, OnvifError> {
            Ok(PullPointSubscription::test_fixture(60))
        }

        fn synchronize(
            &self,
            _subscription: &PullPointSubscription,
            _credentials: &OnvifCredentials,
        ) -> Result<(), OnvifError> {
            Ok(())
        }

        fn pull(
            &self,
            _subscription: &PullPointSubscription,
            _credentials: &OnvifCredentials,
        ) -> Result<Vec<MotionNotification>, OnvifError> {
            if self.block_pull.load(Ordering::Acquire) {
                let (lock, cv) = &self.pull_gate;
                let mut gate = lock.lock().unwrap();
                gate.entered = true;
                cv.notify_all();
                while !gate.release {
                    gate = cv.wait(gate).unwrap();
                }
                return Err(OnvifError::Timeout);
            }
            if self
                .scripted_pull_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
            {
                return Err(OnvifError::Protocol);
            }
            if self
                .scripted_pull_successes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(Vec::new());
            }
            if self.scripted_pull_protocol_after.load(Ordering::Acquire) {
                return Err(OnvifError::Protocol);
            }
            thread::sleep(Duration::from_millis(5));
            Err(OnvifError::Timeout)
        }

        fn renew(
            &self,
            _subscription: &mut PullPointSubscription,
            _credentials: &OnvifCredentials,
        ) -> Result<(), OnvifError> {
            Ok(())
        }

        fn unsubscribe(
            &self,
            _subscription: &PullPointSubscription,
            _credentials: &OnvifCredentials,
        ) -> Result<(), OnvifError> {
            self.unsubscribe_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn wait_reconnect(&self, cancel: &AtomicBool, duration: Duration) {
            self.backoff_delays.lock().unwrap().push(duration.as_secs());
            if self.skip_backoff_sleep.load(Ordering::Acquire) {
                thread::yield_now();
            } else {
                sleep_cancellable(cancel, duration);
            }
        }
    }

    fn commit_motion(
        normalizer: &mut MotionNormalizer,
        camera_id: &CameraId,
        notification: &MotionNotification,
        received: DateTime<Utc>,
    ) -> Option<EventInsert> {
        let decision = normalizer.prepare(camera_id, notification, received);
        let transition = decision.transition.clone();
        normalizer.commit(decision);
        transition
    }

    #[derive(Default)]
    struct RecordingEventSink {
        signals: Mutex<Vec<PersistedEventSignal>>,
    }

    impl PersistedEventSink for RecordingEventSink {
        fn try_publish(&self, signal: PersistedEventSignal) {
            self.signals.lock().unwrap().push(signal);
        }
    }

    fn controller_fixture(
        backend: Arc<FakeBackend>,
        camera_host: &str,
        binding_host: &str,
        desired: bool,
    ) -> (Arc<EventController>, Arc<MemoryCredentialStore>, TempDir) {
        let temp = tempdir().unwrap();
        let mut settings = SettingsStore::open(temp.path().join("settings.sqlite3")).unwrap();
        let camera_id = CameraId::parse("front-door").unwrap();
        let credential_ref = CredentialRef::parse("nian-vision/front-door/camera").unwrap();
        let camera = CameraConfig::new(
            camera_id.clone(),
            "Front Door",
            CameraSource::Rtsp(
                CameraEndpoint::new(Host::parse(camera_host).unwrap(), 554, "/stream").unwrap(),
            ),
            AudioPolicy::CopyAll,
            credential_ref.clone(),
        )
        .unwrap();
        settings.insert_camera(&camera).unwrap();
        settings
            .save_event_binding(
                &EventBinding::new(
                    camera_id.clone(),
                    OnvifScheme::Http,
                    Host::parse(binding_host).unwrap(),
                    80,
                    "/onvif/device_service",
                    "urn:uuid:front-door",
                    credential_ref.clone(),
                    false,
                )
                .unwrap(),
            )
            .unwrap();
        if desired {
            settings
                .set_event_monitoring_enabled(&camera_id, true)
                .unwrap();
        }
        let credentials = Arc::new(MemoryCredentialStore::default());
        credentials
            .put(&credential_ref, &Credentials::new("admin", "secret"))
            .unwrap();
        let index = EventIndex::open(temp.path().join("events.sqlite3")).unwrap();
        let controller = Arc::new(EventController::with_backend(
            Box::new(settings),
            credentials.clone(),
            backend,
            Some(index),
            Some(30),
        ));
        (controller, credentials, temp)
    }

    #[test]
    fn synchronization_baseline_and_repeated_states_do_not_create_fake_transitions() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let baseline = MotionNotification {
            active: true,
            device_time_utc: Some(now),
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: true,
        };
        assert!(commit_motion(&mut normalizer, &camera_id, &baseline, now).is_none());
        assert_eq!(normalizer.aggregate_motion(), Some(true));

        let repeated = MotionNotification {
            synchronization_baseline: false,
            ..baseline.clone()
        };
        assert!(commit_motion(&mut normalizer, &camera_id, &repeated, now).is_none());

        let ended = MotionNotification {
            active: false,
            synchronization_baseline: false,
            ..baseline
        };
        let transition = commit_motion(&mut normalizer, &camera_id, &ended, now).unwrap();
        assert_eq!(transition.kind, EventKind::MotionEnded);
        assert_eq!(normalizer.aggregate_motion(), Some(false));
        assert!(commit_motion(&mut normalizer, &camera_id, &ended, now).is_none());
    }

    #[test]
    fn unknown_false_is_baseline_but_later_live_start_persists_once() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let idle = MotionNotification {
            active: false,
            device_time_utc: Some(now),
            source_key: None,
            synchronization_baseline: false,
        };
        assert!(commit_motion(&mut normalizer, &camera_id, &idle, now).is_none());
        let active = MotionNotification {
            active: true,
            ..idle.clone()
        };
        let transition = commit_motion(&mut normalizer, &camera_id, &active, now).unwrap();
        assert_eq!(transition.kind, EventKind::MotionStarted);
        assert!(commit_motion(&mut normalizer, &camera_id, &active, now).is_none());
    }

    #[test]
    fn failed_motion_started_persistence_does_not_advance_and_replay_retries_once() {
        let temp = tempdir().unwrap();
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let event_index = Mutex::new(None);
        let idle = MotionNotification {
            active: false,
            device_time_utc: None,
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: false,
        };
        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &idle,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        assert_eq!(normalizer.aggregate_motion(), Some(false));

        let started = MotionNotification {
            active: true,
            ..idle.clone()
        };
        assert_eq!(
            apply_motion_notification(
                &mut normalizer,
                &camera_id,
                &started,
                now,
                &event_index,
                Some(30),
            ),
            Err(())
        );
        assert_eq!(normalizer.aggregate_motion(), Some(false));

        *event_index.lock().unwrap() =
            Some(EventIndex::open(temp.path().join("events.sqlite3")).unwrap());
        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &started,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        assert_eq!(normalizer.aggregate_motion(), Some(true));
        let rows = event_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .recent(&camera_id, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EventKind::MotionStarted);

        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &started,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        let rows = event_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .recent(&camera_id, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn failed_motion_ended_persistence_does_not_advance_and_replay_retries_once() {
        let temp = tempdir().unwrap();
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let event_index = Mutex::new(None);
        let baseline = MotionNotification {
            active: true,
            device_time_utc: None,
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: true,
        };
        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &baseline,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        assert_eq!(normalizer.aggregate_motion(), Some(true));

        let ended = MotionNotification {
            active: false,
            synchronization_baseline: false,
            ..baseline
        };
        assert_eq!(
            apply_motion_notification(
                &mut normalizer,
                &camera_id,
                &ended,
                now,
                &event_index,
                Some(30),
            ),
            Err(())
        );
        assert_eq!(normalizer.aggregate_motion(), Some(true));

        *event_index.lock().unwrap() =
            Some(EventIndex::open(temp.path().join("events.sqlite3")).unwrap());
        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &ended,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        assert_eq!(normalizer.aggregate_motion(), Some(false));
        let rows = event_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .recent(&camera_id, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EventKind::MotionEnded);
    }

    #[test]
    fn duplicate_persisted_transition_commits_runtime_state_without_extra_row() {
        let temp = tempdir().unwrap();
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let event_index = Mutex::new(Some(
            EventIndex::open(temp.path().join("events.sqlite3")).unwrap(),
        ));
        let idle = MotionNotification {
            active: false,
            device_time_utc: Some(now),
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: false,
        };
        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &idle,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        let started = MotionNotification {
            active: true,
            ..idle
        };
        let prepared = normalizer.prepare(&camera_id, &started, now);
        let insert = prepared.transition.as_ref().unwrap().clone();
        assert!(
            event_index
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .insert(&insert)
                .unwrap()
                .is_some()
        );
        assert_eq!(normalizer.aggregate_motion(), Some(false));

        apply_motion_notification(
            &mut normalizer,
            &camera_id,
            &started,
            now,
            &event_index,
            Some(30),
        )
        .unwrap();
        assert_eq!(normalizer.aggregate_motion(), Some(true));
        let rows = event_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .recent(&camera_id, 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EventKind::MotionStarted);
    }

    #[test]
    fn notification_signal_requires_new_successful_event_insert() {
        let temp = tempdir().unwrap();
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let event_index = Mutex::new(Some(
            EventIndex::open(temp.path().join("events.sqlite3")).unwrap(),
        ));
        let sink = RecordingEventSink::default();
        let idle = MotionNotification {
            active: false,
            device_time_utc: Some(now),
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: false,
        };
        let started = MotionNotification {
            active: true,
            ..idle.clone()
        };

        let mut normalizer = MotionNormalizer::default();
        apply_motion_notification_with_sink(
            &mut normalizer,
            &camera_id,
            &idle,
            now,
            &event_index,
            MotionNotificationProjection {
                camera_display_name: "Front Door",
                persisted_event_sink: &sink,
            },
            Some(30),
        )
        .unwrap();
        apply_motion_notification_with_sink(
            &mut normalizer,
            &camera_id,
            &started,
            now,
            &event_index,
            MotionNotificationProjection {
                camera_display_name: "Front Door",
                persisted_event_sink: &sink,
            },
            Some(30),
        )
        .unwrap();
        assert_eq!(sink.signals.lock().unwrap().len(), 1);

        // A fresh normalizer can rediscover the same device-time transition after
        // restart/reconnect, but EventIndex fingerprint dedupe must suppress the
        // notification projection because no new row was committed.
        let mut replay = MotionNormalizer::default();
        apply_motion_notification_with_sink(
            &mut replay,
            &camera_id,
            &idle,
            now,
            &event_index,
            MotionNotificationProjection {
                camera_display_name: "Front Door",
                persisted_event_sink: &sink,
            },
            Some(30),
        )
        .unwrap();
        apply_motion_notification_with_sink(
            &mut replay,
            &camera_id,
            &started,
            now,
            &event_index,
            MotionNotificationProjection {
                camera_display_name: "Front Door",
                persisted_event_sink: &sink,
            },
            Some(30),
        )
        .unwrap();
        assert_eq!(sink.signals.lock().unwrap().len(), 1);
    }

    #[test]
    fn persistence_failure_never_publishes_notification_signal() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let event_index = Mutex::new(None);
        let sink = RecordingEventSink::default();
        let idle = MotionNotification {
            active: false,
            device_time_utc: Some(now),
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: false,
        };
        let started = MotionNotification {
            active: true,
            ..idle.clone()
        };
        let mut normalizer = MotionNormalizer::default();
        apply_motion_notification_with_sink(
            &mut normalizer,
            &camera_id,
            &idle,
            now,
            &event_index,
            MotionNotificationProjection {
                camera_display_name: "Front Door",
                persisted_event_sink: &sink,
            },
            Some(30),
        )
        .unwrap();
        assert_eq!(
            apply_motion_notification_with_sink(
                &mut normalizer,
                &camera_id,
                &started,
                now,
                &event_index,
                MotionNotificationProjection {
                    camera_display_name: "Front Door",
                    persisted_event_sink: &sink,
                },
                Some(30),
            ),
            Err(())
        );
        assert!(sink.signals.lock().unwrap().is_empty());
        assert_eq!(normalizer.aggregate_motion(), Some(false));
    }

    #[test]
    fn desired_on_survives_runtime_start_failure() {
        let backend = FakeBackend::new();
        backend.fail_control.store(true, Ordering::Release);
        let (controller, _credentials, _temp) =
            controller_fixture(backend.clone(), "192.168.1.8", "192.168.1.8", true);
        controller.restore_desired().unwrap();
        let status = controller.status("front-door").unwrap();
        assert!(status.desired);
        assert_eq!(status.state, EventRuntimeState::Failed);
        assert_eq!(
            status.last_error_code.as_deref(),
            Some("device_unreachable")
        );
        assert_eq!(backend.control_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn stale_binding_authority_is_rejected_before_authenticated_event_traffic() {
        let backend = FakeBackend::new();
        let (controller, _credentials, _temp) =
            controller_fixture(backend.clone(), "192.168.1.8", "192.168.1.90", true);
        controller.restore_desired().unwrap();
        let status = controller.status("front-door").unwrap();
        assert!(status.desired);
        assert_eq!(status.state, EventRuntimeState::Failed);
        assert_eq!(
            status.last_error_code.as_deref(),
            Some("authority_rejected")
        );
        assert_eq!(backend.control_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn shutdown_waits_for_blocked_pull_and_unsubscribes_before_returning() {
        let backend = FakeBackend::new();
        backend.block_pull.store(true, Ordering::Release);
        let (controller, _credentials, _temp) =
            controller_fixture(backend.clone(), "192.168.1.8", "192.168.1.8", true);
        controller.restore_desired().unwrap();
        backend.wait_for_pull();

        let (done_tx, done_rx) = mpsc::channel();
        let worker = controller.clone();
        let shutdown = thread::spawn(move || {
            worker.shutdown_sessions();
            done_tx.send(()).unwrap();
        });
        thread::sleep(Duration::from_millis(30));
        assert!(done_rx.try_recv().is_err());
        backend.release_pull();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        shutdown.join().unwrap();
        assert_eq!(controller.owned_worker_count(), 0);
        assert!(backend.unsubscribe_calls.load(Ordering::Acquire) >= 1);
    }

    #[test]
    fn motion_source_state_is_bounded_and_overflow_becomes_unknown() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        for index in 0..(MAX_EVENT_SOURCES_PER_SESSION + 16) {
            let notification = MotionNotification {
                active: true,
                device_time_utc: None,
                source_key: Some(format!("source-{index}")),
                synchronization_baseline: false,
            };
            let _ = commit_motion(&mut normalizer, &camera_id, &notification, now);
        }
        assert_eq!(normalizer.sources.len(), MAX_EVENT_SOURCES_PER_SESSION);
        assert!(normalizer.overflowed);
        assert_eq!(normalizer.aggregate_motion(), None);

        let known_end = MotionNotification {
            active: false,
            device_time_utc: None,
            source_key: Some("source-0".to_owned()),
            synchronization_baseline: false,
        };
        assert!(commit_motion(&mut normalizer, &camera_id, &known_end, now).is_some());
        assert_eq!(normalizer.sources.len(), MAX_EVENT_SOURCES_PER_SESSION);
        assert_eq!(normalizer.aggregate_motion(), None);
    }

    #[test]
    fn timestamp_less_reconnect_replay_is_deduped_but_later_transition_persists() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let now = Utc::now();
        let mut normalizer = MotionNormalizer::default();
        let started = MotionNotification {
            active: true,
            device_time_utc: None,
            source_key: Some("source-a".to_owned()),
            synchronization_baseline: false,
        };
        assert_eq!(
            commit_motion(&mut normalizer, &camera_id, &started, now)
                .unwrap()
                .kind,
            EventKind::MotionStarted
        );
        // Subscription recreation deliberately preserves the bounded normalizer.
        assert!(commit_motion(&mut normalizer, &camera_id, &started, now).is_none());

        let ended = MotionNotification {
            active: false,
            ..started.clone()
        };
        assert_eq!(
            commit_motion(&mut normalizer, &camera_id, &ended, now)
                .unwrap()
                .kind,
            EventKind::MotionEnded
        );
        assert_eq!(
            commit_motion(&mut normalizer, &camera_id, &started, now)
                .unwrap()
                .kind,
            EventKind::MotionStarted
        );
    }

    #[test]
    fn subscription_renew_deadline_rejects_short_remote_lifetime_and_bounds_long_lifetime() {
        let before = Instant::now();
        let normal = subscription_renew_deadline(&PullPointSubscription::test_fixture(60)).unwrap();
        let normal_delay = normal.saturating_duration_since(before).as_secs();
        assert!((39..=40).contains(&normal_delay));

        let minimum = nian_onvif::MIN_EVENT_SUBSCRIPTION_LIFETIME_SECS;
        assert!(
            subscription_renew_deadline(&PullPointSubscription::test_fixture(minimum as i64))
                .is_ok()
        );
        assert_eq!(
            subscription_renew_deadline(&PullPointSubscription::test_fixture(minimum as i64 - 1)),
            Err(OnvifError::Protocol)
        );
        assert_eq!(
            subscription_renew_deadline(&PullPointSubscription::test_fixture(1)),
            Err(OnvifError::Protocol)
        );

        let before = Instant::now();
        let long =
            subscription_renew_deadline(&PullPointSubscription::test_fixture(10 * 24 * 60 * 60))
                .unwrap();
        let long_delay = long.saturating_duration_since(before).as_secs();
        let expected = nian_onvif::MAX_EVENT_SUBSCRIPTION_LIFETIME_SECS * 2 / 3;
        assert!((expected.saturating_sub(1)..=expected).contains(&long_delay));

        assert_eq!(
            subscription_renew_deadline(&PullPointSubscription::test_fixture(0)),
            Err(OnvifError::Protocol)
        );
        assert_eq!(
            subscription_renew_deadline(&PullPointSubscription::test_fixture(-1)),
            Err(OnvifError::Protocol)
        );
        let fallback = PullPointSubscription::test_fixture_times(None, None);
        assert!(subscription_renew_deadline(&fallback).is_ok());
    }

    #[test]
    fn terminal_runtime_failure_stays_owned_failed_until_intentional_teardown() {
        let backend = FakeBackend::new();
        backend
            .unsupported_after_first_control
            .store(true, Ordering::Release);
        let (controller, _credentials, _temp) =
            controller_fixture(backend, "192.168.1.8", "192.168.1.8", true);
        controller.restore_desired().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = controller.status("front-door").unwrap();
            if status.state == EventRuntimeState::Failed {
                assert!(status.desired);
                assert_eq!(status.last_error_code.as_deref(), Some("unsupported"));
                break;
            }
            assert!(Instant::now() < deadline, "worker never entered Failed");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(controller.ownership_counts(), (0, 1, 0, 0));
        controller.shutdown_sessions();
        assert_eq!(controller.ownership_counts(), (0, 0, 0, 0));
    }

    #[test]
    fn concurrent_shutdown_callers_wait_for_the_same_controller_owned_drain() {
        let backend = FakeBackend::new();
        backend.block_pull.store(true, Ordering::Release);
        let (controller, _credentials, _temp) =
            controller_fixture(backend.clone(), "192.168.1.8", "192.168.1.8", true);
        controller.restore_desired().unwrap();
        backend.wait_for_pull();

        let (a_tx, a_rx) = mpsc::channel();
        let first = controller.clone();
        let a = thread::spawn(move || {
            first.shutdown_sessions();
            a_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while controller.ownership_counts().2 != 1 {
            assert!(Instant::now() < deadline, "session never became draining");
            thread::sleep(Duration::from_millis(2));
        }

        let (b_tx, b_rx) = mpsc::channel();
        let second = controller.clone();
        let b = thread::spawn(move || {
            second.shutdown_sessions();
            b_tx.send(()).unwrap();
        });
        thread::sleep(Duration::from_millis(30));
        assert!(a_rx.try_recv().is_err());
        assert!(b_rx.try_recv().is_err());
        assert_eq!(controller.ownership_counts().2, 1);

        backend.release_pull();
        a_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        b_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(controller.ownership_counts(), (0, 0, 0, 0));
        assert_eq!(backend.unsubscribe_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn runtime_event_failures_keep_the_operation_stage_in_the_status_code() {
        assert_eq!(
            event_control_error_code(&OnvifError::Protocol),
            "event_control_protocol"
        );
        assert_eq!(
            pull_error_code(&OnvifError::Protocol),
            "event_pull_protocol"
        );
        assert_eq!(
            renew_error_code(&OnvifError::Protocol),
            "event_renew_protocol"
        );
        assert_eq!(pull_error_code(&OnvifError::Timeout), "pull_timeout");
    }

    #[test]
    fn pull_failure_backoff_escalates_and_healthy_pull_resets_the_streak() {
        let backend = FakeBackend::new();
        backend.scripted_pull_failures.store(5, Ordering::Release);
        backend.scripted_pull_successes.store(1, Ordering::Release);
        backend
            .scripted_pull_protocol_after
            .store(true, Ordering::Release);
        backend.skip_backoff_sleep.store(true, Ordering::Release);
        let (controller, _credentials, _temp) =
            controller_fixture(backend.clone(), "192.168.1.8", "192.168.1.8", true);
        controller.restore_desired().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let delays = backend.backoff_delays.lock().unwrap().clone();
            if delays.len() >= 6 {
                assert_eq!(&delays[..6], &[2, 5, 10, 30, 60, 2]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "backoff sequence did not advance"
            );
            thread::sleep(Duration::from_millis(2));
        }
        controller.shutdown_sessions();
    }
}
