//! Nian Vision application layer.
//!
//! Sits between the UI/Tauri host and the infrastructure crates (storage,
//! media). Owns configuration validation and cross-cutting policies; must not
//! depend on FFmpeg or any concrete media implementation.
//!
//! The crate forbids `unsafe`.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod camera_service;
pub mod config;
pub mod desktop_lifecycle;
pub mod error;
pub mod event_controller;
pub mod live_controller;
pub mod onvif_controller;
pub mod playback;
pub mod probe_controller;
pub mod ptz_controller;
pub mod recording_controller;
pub mod storage_manager;
pub mod supervisor;
mod worker_process;

pub use camera_service::{
    ApplicationSettingsDto, CameraDraft, CameraMutation, CameraService, CameraServiceError,
    CameraSummary, CameraWarning, CredentialRefGenerator, CredentialRefGeneratorError,
    CredentialStore, CredentialStoreError, MemoryCredentialStore, PreparedApplicationSettings,
    PreparedProbe, RandomCredentialRefGenerator, SettingsRepository, SettingsRepositoryError,
};
pub use config::{AppConfig, SegmentTargetDuration};
pub use desktop_lifecycle::{DesktopLifecycle, DesktopLifecycleError, DesktopLifecycleState};
pub use error::ApplicationError;
pub use event_controller::{
    EventBackend, EventController, EventError, EventHistoryDto, EventHistoryKind, EventMutation,
    EventRuntimeState, EventSettingsRepository, EventStatusDto, EventTeardownBatch, EventWarning,
    MAX_ACTIVE_EVENT_SESSIONS,
};
pub use live_controller::{
    LiveError, LiveFailureCategory, LiveOpenDto, LiveRunner, LiveRunnerFactory, LiveState,
    LiveStatus, LiveTeardownBatch, LiveViewController, LiveWorkerStatus,
    MAX_SIMULTANEOUS_LIVE_VIEWS, PreparedLive, WorkerLiveRunnerFactory,
};
pub use onvif_controller::{
    OnvifConnectionDto, OnvifController, OnvifControllerError, OnvifDiscoveredDeviceDto,
    OnvifDiscoveryDto, OnvifMediaProfileDto, OnvifPreparedProfileDto, PreparedEventPairing,
    PreparedPtzPairing,
};
pub use playback::{
    AdjacentRecordingsDto, PlaybackBackend, PlaybackController, PlaybackError, PlaybackErrorCode,
    PlaybackInspectDto, PlaybackOpenDto, PreparedPlaybackStorage, RecordingDto,
    TimelineRecordingKind, WorkerPlaybackBackend,
};
pub use probe_controller::{
    ProbeController, ProbeError, ProbeResult, ProbeRunner, WorkerProbeRunner,
};
pub use ptz_controller::{
    MAX_ACTIVE_PTZ_SESSIONS, PTZ_COMMAND_QUEUE_CAPACITY, PTZ_MOVEMENT_LEASE_MS,
    PTZ_RENEW_INTERVAL_MS, PtzBackend, PtzCapabilitiesDto, PtzController, PtzDirection, PtzError,
    PtzMovementDto, PtzMutation, PtzRuntimeState, PtzSettingsRepository, PtzTeardownBatch,
    PtzWarning,
};
pub use recording_controller::{
    MAX_SIMULTANEOUS_RECORDINGS, RecordingController, RecordingControllerError,
    RecordingRunFailure, RecordingRunner, RecordingRunnerFactory, RecordingState, RecordingStatus,
    RecordingThreadSpawner, StdRecordingThreadSpawner, SupervisorRecordingRunner,
    SupervisorRecordingRunnerFactory,
};
pub use storage_manager::{
    ArtifactCleanupReport, PlaybackPins, ReconciliationFailure, ReconciliationFailureKind,
    ReconciliationReport, RecordingLookupError, RetentionFailure, RetentionFailureKind,
    RetentionReport, StorageManager, StorageManagerError, ValidatedRecording,
};
pub use supervisor::{
    BinaryLauncher, DesiredRecording, JobTerminal, SupervisorDeadlines, WorkerEnd, WorkerLauncher,
    WorkerSupervisor,
};
