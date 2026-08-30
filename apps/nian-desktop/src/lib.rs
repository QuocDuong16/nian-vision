//! Desktop host process: owns managed application state and narrow Tauri commands.
//!
//! Media work never happens here; recording and probing are delegated to the
//! `nian-media-worker` process. The host resolves platform paths, owns native
//! credential-store plumbing, and maps typed application errors to stable DTOs.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::NaiveDateTime;
use nian_application::{
    ApplicationSettingsDto, CameraDraft, CameraService, CameraServiceError, CameraSummary,
    CredentialStore, CredentialStoreError, PlaybackController, PlaybackError, PlaybackOpenDto,
    ProbeController, ProbeError, ProbeResult, RecordingController, RecordingControllerError,
    RecordingDto, RecordingStatus, SupervisorRecordingRunner, WorkerProbeRunner,
};
use nian_domain::{
    AudioPolicy, CameraId, CredentialRef, Credentials, RetentionPolicy, StorageQuota,
};
use nian_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tracing_subscriber::EnvFilter;

const CREDENTIAL_SERVICE: &str = "Nian Vision";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesktopErrorDto {
    pub code: &'static str,
    pub message: String,
}

impl DesktopErrorDto {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Command input carrying credentials only from React toward Rust. It is
/// intentionally neither Debug nor Serialize, so accidental response/log paths
/// cannot render the password.
#[derive(Deserialize)]
struct CameraCommandInput {
    camera_id: String,
    display_name: String,
    host: String,
    port: u16,
    path: String,
    audio_policy: AudioPolicy,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

impl CameraCommandInput {
    fn into_draft(self) -> Result<CameraDraft, DesktopErrorDto> {
        let replacement_credentials = match (self.username.is_empty(), self.password.is_empty()) {
            (true, true) => None,
            (false, false) => Some(Credentials::new(self.username, self.password)),
            _ => {
                return Err(DesktopErrorDto::new(
                    "validation",
                    "username and password must be supplied together",
                ));
            }
        };
        Ok(CameraDraft {
            camera_id: self.camera_id,
            display_name: self.display_name,
            host: self.host,
            port: self.port,
            path: self.path,
            audio_policy: self.audio_policy,
            replacement_credentials,
        })
    }
}

#[derive(Deserialize)]
struct ProbeCommandInput {
    camera: CameraCommandInput,
    #[serde(default = "default_probe_timeout_ms")]
    timeout_ms: u64,
}

const fn default_probe_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Default)]
struct NativeCredentialStore;

#[derive(Serialize, Deserialize)]
struct StoredCredential {
    username: String,
    password: String,
}

impl NativeCredentialStore {
    fn entry(reference: &CredentialRef) -> Result<keyring::v1::Entry, CredentialStoreError> {
        keyring::v1::Entry::new(CREDENTIAL_SERVICE, reference.as_str())
            .map_err(|_| CredentialStoreError::new("entry"))
    }
}

impl CredentialStore for NativeCredentialStore {
    fn exists(&self, reference: &CredentialRef) -> Result<bool, CredentialStoreError> {
        match Self::entry(reference)?.get_secret() {
            Ok(_) => Ok(true),
            Err(keyring::v1::Error::NoEntry) => Ok(false),
            Err(_) => Err(CredentialStoreError::new("exists")),
        }
    }

    fn put(
        &self,
        reference: &CredentialRef,
        credentials: &Credentials,
    ) -> Result<(), CredentialStoreError> {
        let bytes = serde_json::to_vec(&StoredCredential {
            username: credentials.username.clone(),
            password: credentials.password().to_owned(),
        })
        .map_err(|_| CredentialStoreError::new("encode"))?;
        Self::entry(reference)?
            .set_secret(&bytes)
            .map_err(|_| CredentialStoreError::new("put"))
    }

    fn get(&self, reference: &CredentialRef) -> Result<Credentials, CredentialStoreError> {
        let bytes = Self::entry(reference)?
            .get_secret()
            .map_err(|_| CredentialStoreError::new("get"))?;
        let stored: StoredCredential =
            serde_json::from_slice(&bytes).map_err(|_| CredentialStoreError::new("decode"))?;
        Ok(Credentials::new(stored.username, stored.password))
    }

    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError> {
        Self::entry(reference)?
            .delete_credential()
            .map_err(|_| CredentialStoreError::new("delete"))
    }
}

struct DesktopState {
    camera_service: Mutex<CameraService>,
    recording_controller: Mutex<RecordingController>,
    playback_controller: Mutex<PlaybackController>,
    probe_controller: ProbeController,
    /// Serializes operations whose correctness depends on a stable recording
    /// ownership snapshot: start/stop, critical edit/delete, and settings write.
    control_gate: Mutex<()>,
}

impl std::fmt::Debug for DesktopState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopState").finish_non_exhaustive()
    }
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "Nian Vision".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

#[tauri::command]
fn camera_list(
    state: tauri::State<'_, DesktopState>,
) -> Result<Vec<CameraSummary>, DesktopErrorDto> {
    lock(&state.camera_service)?
        .list_cameras()
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_create(
    state: tauri::State<'_, DesktopState>,
    input: CameraCommandInput,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    lock(&state.camera_service)?
        .create_camera(input.into_draft()?)
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_update(
    state: tauri::State<'_, DesktopState>,
    input: CameraCommandInput,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    let active = lock(&state.recording_controller)?
        .active_camera()
        .map_err(map_recording_error)?;
    lock(&state.camera_service)?
        .update_camera(input.into_draft()?, active.as_ref())
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_delete(
    state: tauri::State<'_, DesktopState>,
    camera_id: String,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    let active = lock(&state.recording_controller)?
        .active_camera()
        .map_err(map_recording_error)?;
    lock(&state.camera_service)?
        .delete_camera(&camera_id, active.as_ref())
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_probe(
    state: tauri::State<'_, DesktopState>,
    input: ProbeCommandInput,
) -> Result<ProbeResult, DesktopErrorDto> {
    // Prepare a secret-bearing immutable snapshot while holding the service
    // lock, then release it before the bounded worker process does network I/O.
    let draft = input.camera.into_draft()?;
    let request = lock(&state.camera_service)?
        .prepare_probe_draft(&draft, input.timeout_ms)
        .map_err(map_camera_error)?;
    state
        .probe_controller
        .probe(request)
        .map_err(map_probe_error)
}

#[tauri::command]
fn recording_start(
    state: tauri::State<'_, DesktopState>,
    camera_id: String,
) -> Result<RecordingStatus, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    let desired = lock(&state.camera_service)?
        .prepare_recording(&camera_id)
        .map_err(map_camera_error)?;
    let id = CameraId::parse(&camera_id)
        .map_err(|error| DesktopErrorDto::new("validation", error.to_string()))?;
    lock(&state.recording_controller)?
        .start(id, desired)
        .map_err(map_recording_error)
}

#[tauri::command]
fn recording_stop(
    state: tauri::State<'_, DesktopState>,
) -> Result<RecordingStatus, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    lock(&state.recording_controller)?
        .stop()
        .map_err(map_recording_error)
}

#[tauri::command]
fn recording_status(
    state: tauri::State<'_, DesktopState>,
) -> Result<RecordingStatus, DesktopErrorDto> {
    lock(&state.recording_controller)?
        .status()
        .map_err(map_recording_error)
}

#[tauri::command]
fn recordings_refresh(state: tauri::State<'_, DesktopState>) -> Result<(), DesktopErrorDto> {
    lock(&state.playback_controller)?
        .refresh_index()
        .map_err(map_playback_error)
}

#[tauri::command]
fn recording_days(
    state: tauri::State<'_, DesktopState>,
    camera_id: String,
) -> Result<Vec<String>, DesktopErrorDto> {
    let camera_id = CameraId::parse(&camera_id)
        .map_err(|error| DesktopErrorDto::new("validation", error.to_string()))?;
    lock(&state.playback_controller)?
        .recording_days(&camera_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn recording_timeline(
    state: tauri::State<'_, DesktopState>,
    camera_id: String,
    start: String,
    end: String,
) -> Result<Vec<RecordingDto>, DesktopErrorDto> {
    let camera_id = CameraId::parse(&camera_id)
        .map_err(|error| DesktopErrorDto::new("validation", error.to_string()))?;
    let start = parse_local_wall_time(&start)?;
    let end = parse_local_wall_time(&end)?;
    if end <= start {
        return Err(DesktopErrorDto::new(
            "validation",
            "timeline end must be after start",
        ));
    }
    lock(&state.playback_controller)?
        .timeline(&camera_id, start, end)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_open(
    state: tauri::State<'_, DesktopState>,
    recording_id: String,
) -> Result<PlaybackOpenDto, DesktopErrorDto> {
    lock(&state.playback_controller)?
        .open(&recording_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_close(
    state: tauri::State<'_, DesktopState>,
    session_id: String,
) -> Result<(), DesktopErrorDto> {
    lock(&state.playback_controller)?
        .close(&session_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_status(
    state: tauri::State<'_, DesktopState>,
    session_id: String,
) -> Result<bool, DesktopErrorDto> {
    lock(&state.playback_controller)?
        .session_active(&session_id)
        .map_err(map_playback_error)
}

fn parse_local_wall_time(value: &str) -> Result<NaiveDateTime, DesktopErrorDto> {
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"]
        .into_iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .ok_or_else(|| DesktopErrorDto::new("validation", "invalid local timeline timestamp"))
}

#[tauri::command]
fn settings_get(
    state: tauri::State<'_, DesktopState>,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    lock(&state.camera_service)?
        .application_settings()
        .map_err(map_camera_error)
}

#[tauri::command]
fn settings_update(
    state: tauri::State<'_, DesktopState>,
    settings: ApplicationSettingsDto,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    let active = lock(&state.recording_controller)?
        .status()
        .map_err(map_recording_error)?
        .state
        .is_active();
    let mut camera_service = lock(&state.camera_service)?;
    let mut playback = lock(&state.playback_controller)?;
    update_settings_transaction(&mut camera_service, &mut playback, settings, active)
}

fn update_settings_transaction(
    camera_service: &mut CameraService,
    playback: &mut PlaybackController,
    settings: ApplicationSettingsDto,
    recording_active: bool,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    let prepared_settings = camera_service
        .prepare_application_settings(settings, recording_active)
        .map_err(map_camera_error)?;
    let prepared_dto = prepared_settings.dto();
    let (storage_root, retention, quota) = playback_storage_config(&prepared_dto)?;
    let prepared_playback = playback
        .prepare_storage(storage_root, retention, quota)
        .map_err(map_playback_error)?;
    let saved = camera_service
        .commit_application_settings(prepared_settings)
        .map_err(map_camera_error)?;
    playback.commit_prepared_storage(prepared_playback);
    Ok(saved)
}

fn playback_storage_config(
    settings: &ApplicationSettingsDto,
) -> Result<(Option<PathBuf>, RetentionPolicy, Option<StorageQuota>), DesktopErrorDto> {
    let retention = RetentionPolicy {
        max_age_days: settings.max_age_days,
        max_storage_bytes: settings.max_storage_bytes,
    };
    let quota = match (settings.max_storage_bytes, settings.cleanup_target_bytes) {
        (Some(max_bytes), Some(cleanup_target_bytes)) => Some(StorageQuota {
            max_bytes,
            cleanup_target_bytes,
        }),
        (None, None) => None,
        _ => {
            return Err(DesktopErrorDto::new(
                "validation",
                "storage quota settings are incomplete",
            ));
        }
    };
    Ok((
        settings.storage_root.as_deref().map(PathBuf::from),
        retention,
        quota,
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, DesktopErrorDto> {
    mutex
        .lock()
        .map_err(|_| DesktopErrorDto::new("internal", "application state is unavailable"))
}

fn map_camera_error(error: CameraServiceError) -> DesktopErrorDto {
    match error {
        CameraServiceError::Validation(message) => DesktopErrorDto::new("validation", message),
        CameraServiceError::CameraNotFound => {
            DesktopErrorDto::new("camera_not_found", "camera was not found")
        }
        CameraServiceError::DuplicateCamera => {
            DesktopErrorDto::new("duplicate_camera", "camera ID already exists")
        }
        CameraServiceError::CameraBusy => {
            DesktopErrorDto::new("camera_busy", "camera is actively recording")
        }
        CameraServiceError::CredentialStore(_) => {
            DesktopErrorDto::new("credential_store", "credential store operation failed")
        }
        CameraServiceError::CredentialRollbackCleanup { .. } => DesktopErrorDto::new(
            "credential_rollback_cleanup",
            "credential rollback cleanup failed",
        ),
        CameraServiceError::CredentialRefGeneration(_)
        | CameraServiceError::CredentialRefCollision => DesktopErrorDto::new(
            "credential_identity",
            "credential reference allocation failed",
        ),
        CameraServiceError::StorageNotConfigured => {
            DesktopErrorDto::new("storage_failed", "recording storage is not configured")
        }
        CameraServiceError::Settings => {
            DesktopErrorDto::new("internal", "application settings could not be persisted")
        }
    }
}

fn map_recording_error(error: RecordingControllerError) -> DesktopErrorDto {
    match error {
        RecordingControllerError::AlreadyRecording => {
            DesktopErrorDto::new("already_recording", "another camera is already recording")
        }
        RecordingControllerError::NotRecording => {
            DesktopErrorDto::new("camera_in_use", "no active recording can be stopped")
        }
        RecordingControllerError::Synchronization | RecordingControllerError::ThreadStart => {
            DesktopErrorDto::new("worker_unavailable", "recording controller is unavailable")
        }
    }
}

fn map_playback_error(error: PlaybackError) -> DesktopErrorDto {
    match error {
        PlaybackError::RecordingNotFound => {
            DesktopErrorDto::new("recording_not_found", "recording was not found")
        }
        PlaybackError::RecordingMissing => DesktopErrorDto::new(
            "recording_missing",
            "recording file is missing; refresh the timeline",
        ),
        PlaybackError::RecordingStale => DesktopErrorDto::new(
            "recording_stale",
            "recording changed on disk; refresh the timeline",
        ),
        PlaybackError::RecordingNotFinalized => DesktopErrorDto::new(
            "recording_not_finalized",
            "recording is not finalized and cannot be played",
        ),
        PlaybackError::UnsupportedCodec => DesktopErrorDto::new(
            "unsupported_codec",
            "recording codec is not supported without transcoding",
        ),
        PlaybackError::UnsupportedContainer => DesktopErrorDto::new(
            "unsupported_container",
            "recording could not be prepared for browser playback",
        ),
        PlaybackError::MediaUnreadable => {
            DesktopErrorDto::new("media_unreadable", "recording media is unreadable")
        }
        PlaybackError::PlaybackSessionExpired => DesktopErrorDto::new(
            "playback_session_expired",
            "playback session expired; reopen the recording",
        ),
        PlaybackError::PlaybackBusy => {
            DesktopErrorDto::new("playback_busy", "too many playback sessions are active")
        }
        PlaybackError::WorkerUnavailable => {
            DesktopErrorDto::new("worker_unavailable", "media worker is unavailable")
        }
        PlaybackError::Internal => DesktopErrorDto::new("internal", "playback operation failed"),
    }
}

fn map_probe_error(error: ProbeError) -> DesktopErrorDto {
    match error {
        ProbeError::Busy => DesktopErrorDto::new(
            "camera_busy",
            "another camera connection test is already running",
        ),
        ProbeError::WorkerUnavailable => {
            DesktopErrorDto::new("worker_unavailable", "media worker is unavailable")
        }
        ProbeError::SourceOpenFailed => {
            DesktopErrorDto::new("source_open_failed", "camera source could not be opened")
        }
        ProbeError::SourceTimeout => {
            DesktopErrorDto::new("source_timeout", "camera source timed out")
        }
        ProbeError::Cancelled => {
            DesktopErrorDto::new("cancelled", "camera source test was cancelled")
        }
        ProbeError::InvalidSource => {
            DesktopErrorDto::new("validation", "camera source configuration is invalid")
        }
        ProbeError::Protocol | ProbeError::SourceProbeFailed => {
            DesktopErrorDto::new("internal", "camera source test failed")
        }
    }
}

/// Initializes logging and starts the Tauri runtime.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let result = tauri::Builder::default()
        .setup(|app| {
            let app_data = app.path().app_data_dir()?;
            let settings_path = app_data.join("settings.sqlite3");
            let settings = SettingsStore::open(settings_path)?;
            let credentials: Arc<dyn CredentialStore> = Arc::new(NativeCredentialStore);
            let camera_service = CameraService::new(Box::new(settings), credentials);
            let initial_settings = camera_service
                .application_settings()
                .map_err(|_| std::io::Error::other("application settings are unavailable"))?;

            let worker_name = if cfg!(windows) {
                "nian-media-worker.exe"
            } else {
                "nian-media-worker"
            };
            let worker_path = std::env::current_exe()?.with_file_name(worker_name);
            let worker_program = worker_path.to_string_lossy().into_owned();
            let recording_controller =
                RecordingController::new(Arc::new(SupervisorRecordingRunner {
                    worker_program: worker_program.clone(),
                }));
            let probe_controller = ProbeController::new(Arc::new(WorkerProbeRunner {
                worker_program: worker_program.clone(),
            }));
            let mut playback_controller =
                PlaybackController::new(worker_program, app_data.join("playback-cache"))
                    .map_err(|_| std::io::Error::other("playback service could not start"))?;
            let (storage_root, retention, quota) = playback_storage_config(&initial_settings)
                .map_err(|error| std::io::Error::other(error.message))?;
            if let Err(error) =
                playback_controller.configure_storage(storage_root, retention, quota)
            {
                tracing::warn!(code = ?error.code(), "playback storage is unavailable at startup");
            }

            app.manage(DesktopState {
                camera_service: Mutex::new(camera_service),
                recording_controller: Mutex::new(recording_controller),
                playback_controller: Mutex::new(playback_controller),
                probe_controller,
                control_gate: Mutex::new(()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            camera_list,
            camera_create,
            camera_update,
            camera_delete,
            camera_probe,
            recording_start,
            recording_stop,
            recording_status,
            recordings_refresh,
            recording_days,
            recording_timeline,
            playback_open,
            playback_close,
            playback_status,
            settings_get,
            settings_update,
        ])
        .run(tauri::generate_context!());

    if let Err(error) = result {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("nian-desktop: fatal: {error}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct TestRepositoryState {
        settings: nian_settings::ApplicationSettings,
        fail_save: bool,
        camera: Option<nian_domain::CameraConfig>,
    }

    #[derive(Clone)]
    struct TestRepository {
        state: Arc<Mutex<TestRepositoryState>>,
    }

    impl nian_application::SettingsRepository for TestRepository {
        fn list_cameras(
            &self,
        ) -> Result<Vec<nian_domain::CameraConfig>, nian_application::SettingsRepositoryError>
        {
            Ok(self.state.lock().unwrap().camera.iter().cloned().collect())
        }

        fn get_camera(
            &self,
            camera_id: &CameraId,
        ) -> Result<Option<nian_domain::CameraConfig>, nian_application::SettingsRepositoryError>
        {
            Ok(self
                .state
                .lock()
                .unwrap()
                .camera
                .as_ref()
                .filter(|camera| camera.camera_id() == camera_id)
                .cloned())
        }

        fn insert_camera(
            &mut self,
            _camera: &nian_domain::CameraConfig,
        ) -> Result<(), nian_application::SettingsRepositoryError> {
            Ok(())
        }

        fn update_camera(
            &mut self,
            _camera: &nian_domain::CameraConfig,
        ) -> Result<bool, nian_application::SettingsRepositoryError> {
            Ok(false)
        }

        fn delete_camera(
            &mut self,
            _camera_id: &CameraId,
        ) -> Result<bool, nian_application::SettingsRepositoryError> {
            Ok(false)
        }

        fn application_settings(
            &self,
        ) -> Result<nian_settings::ApplicationSettings, nian_application::SettingsRepositoryError>
        {
            Ok(self.state.lock().unwrap().settings.clone())
        }

        fn save_application_settings(
            &mut self,
            settings: &nian_settings::ApplicationSettings,
        ) -> Result<(), nian_application::SettingsRepositoryError> {
            let mut state = self.state.lock().unwrap();
            if state.fail_save {
                return Err(nian_application::SettingsRepositoryError::Persistence);
            }
            state.settings = settings.clone();
            Ok(())
        }
    }

    fn test_service(
        root: &std::path::Path,
    ) -> (
        CameraService,
        Arc<Mutex<TestRepositoryState>>,
        Arc<nian_application::MemoryCredentialStore>,
    ) {
        let state = Arc::new(Mutex::new(TestRepositoryState {
            settings: nian_settings::ApplicationSettings {
                storage_root: Some(root.to_path_buf()),
                segment_target_secs: 300,
                retention: RetentionPolicy::default(),
                quota: None,
            },
            fail_save: false,
            camera: None,
        }));
        let repository = TestRepository {
            state: state.clone(),
        };
        let credentials = Arc::new(nian_application::MemoryCredentialStore::default());
        let service = CameraService::new(Box::new(repository), credentials.clone());
        (service, state, credentials)
    }

    fn settings_for(root: &std::path::Path) -> ApplicationSettingsDto {
        ApplicationSettingsDto {
            storage_root: Some(root.to_string_lossy().into_owned()),
            segment_target_secs: 300,
            max_age_days: None,
            max_storage_bytes: None,
            cleanup_target_bytes: None,
        }
    }

    fn configured_playback(root: &std::path::Path, cache_root: PathBuf) -> PlaybackController {
        std::fs::create_dir_all(root).unwrap();
        let mut playback =
            PlaybackController::new("unused-test-worker".to_owned(), cache_root).unwrap();
        playback
            .configure_storage(Some(root.to_path_buf()), RetentionPolicy::default(), None)
            .unwrap();
        playback
    }

    fn input(username: &str, password: &str) -> CameraCommandInput {
        CameraCommandInput {
            camera_id: "front-door".to_owned(),
            display_name: "Front door".to_owned(),
            host: "192.168.1.50".to_owned(),
            port: 554,
            path: "/stream1".to_owned(),
            audio_policy: AudioPolicy::CopyAll,
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    #[test]
    fn desktop_csp_allows_only_loopback_playback_media() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let csp = config["app"]["security"]["csp"].as_str().unwrap();
        let media = csp
            .split(';')
            .map(str::trim)
            .find(|directive| directive.starts_with("media-src "))
            .unwrap();
        assert_eq!(media, "media-src 'self' http://127.0.0.1:*");
        assert!(!media.split_whitespace().any(|source| source == "*"));
        assert!(!csp.contains("192.168."));
        assert!(!csp.contains("default-src http:"));
    }

    #[test]
    fn failed_playback_candidate_does_not_commit_settings_or_swap_active_storage() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let bad_root_b = temp.path().join("recordings-b-file");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::write(&bad_root_b, b"not a directory").unwrap();
        let (mut service, _state, _credentials) = test_service(&root_a);
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));

        assert!(
            update_settings_transaction(
                &mut service,
                &mut playback,
                settings_for(&bad_root_b),
                false,
            )
            .is_err()
        );
        assert_eq!(
            service
                .application_settings()
                .unwrap()
                .storage_root
                .as_deref(),
            Some(root_a.to_string_lossy().as_ref())
        );
        assert_eq!(playback.configured_storage_root(), Some(root_a.as_path()));
    }

    #[test]
    fn failed_settings_persistence_discards_prepared_playback_storage() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, state, _credentials) = test_service(&root_a);
        state.lock().unwrap().fail_save = true;
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));

        assert!(
            update_settings_transaction(&mut service, &mut playback, settings_for(&root_b), false,)
                .is_err()
        );
        assert_eq!(
            service
                .application_settings()
                .unwrap()
                .storage_root
                .as_deref(),
            Some(root_a.to_string_lossy().as_ref())
        );
        assert_eq!(playback.configured_storage_root(), Some(root_a.as_path()));
    }

    #[test]
    fn successful_settings_transaction_commits_then_swaps_playback_storage() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, state, credentials) = test_service(&root_a);
        let credential_ref = CredentialRef::parse("settings-test-ref").unwrap();
        credentials
            .put(&credential_ref, &Credentials::new("admin", "secret"))
            .unwrap();
        state.lock().unwrap().camera = Some(
            nian_domain::CameraConfig::new(
                CameraId::parse("front-door").unwrap(),
                "Front door",
                nian_domain::CameraSource::Rtsp(
                    nian_domain::CameraEndpoint::new(
                        nian_domain::Host::parse("192.168.1.50").unwrap(),
                        554,
                        "/stream1",
                    )
                    .unwrap(),
                ),
                AudioPolicy::CopyAll,
                credential_ref,
            )
            .unwrap(),
        );
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));

        let saved =
            update_settings_transaction(&mut service, &mut playback, settings_for(&root_b), false)
                .unwrap();
        assert_eq!(
            saved.storage_root.as_deref(),
            Some(root_b.to_string_lossy().as_ref())
        );
        assert_eq!(
            service
                .application_settings()
                .unwrap()
                .storage_root
                .as_deref(),
            Some(root_b.to_string_lossy().as_ref())
        );
        assert_eq!(playback.configured_storage_root(), Some(root_b.as_path()));
        let desired = service.prepare_recording("front-door").unwrap();
        assert_eq!(
            PathBuf::from(desired.storage_root),
            root_b,
            "recording and playback must converge on the committed root",
        );
    }

    #[test]
    fn credential_fields_must_be_both_empty_or_both_supplied() {
        assert!(
            input("", "")
                .into_draft()
                .unwrap()
                .replacement_credentials
                .is_none()
        );

        let replacement = input("admin", "password")
            .into_draft()
            .unwrap()
            .replacement_credentials
            .unwrap();
        assert_eq!(replacement.username, "admin");
        assert_eq!(replacement.password(), "password");

        let username_only = match input("admin", "").into_draft() {
            Ok(_) => panic!("username-only credentials must be rejected"),
            Err(error) => error,
        };
        assert_eq!(username_only.code, "validation");
        let password_only = match input("", "password").into_draft() {
            Ok(_) => panic!("password-only credentials must be rejected"),
            Err(error) => error,
        };
        assert_eq!(password_only.code, "validation");
    }
}
