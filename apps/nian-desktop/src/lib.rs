//! Desktop host process: owns managed application state and narrow Tauri commands.
//!
//! Media work never happens here; recording and probing are delegated to the
//! `nian-media-worker` process. The host resolves platform paths, owns native
//! credential-store plumbing, and maps typed application errors to stable DTOs.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use nian_application::{
    ApplicationSettingsDto, CameraDraft, CameraService, CameraServiceError, CameraSummary,
    CredentialStore, CredentialStoreError, ProbeController, ProbeError, ProbeResult,
    RecordingController, RecordingControllerError, RecordingStatus, SupervisorRecordingRunner,
    WorkerProbeRunner,
};
use nian_domain::{AudioPolicy, CameraId, CredentialRef, Credentials};
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
    fn into_draft(self) -> CameraDraft {
        let replacement_credentials = if self.password.is_empty() {
            None
        } else {
            Some(Credentials::new(self.username, self.password))
        };
        CameraDraft {
            camera_id: self.camera_id,
            display_name: self.display_name,
            host: self.host,
            port: self.port,
            path: self.path,
            audio_policy: self.audio_policy,
            replacement_credentials,
        }
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
        .create_camera(input.into_draft())
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
        .update_camera(input.into_draft(), active.as_ref())
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
    let draft = input.camera.into_draft();
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
    lock(&state.camera_service)?
        .save_application_settings(settings, active)
        .map_err(map_camera_error)
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
        CameraServiceError::CameraBusy => {
            DesktopErrorDto::new("camera_busy", "camera is actively recording")
        }
        CameraServiceError::CredentialStore(_) => {
            DesktopErrorDto::new("credential_store", "credential store operation failed")
        }
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
            let probe_controller =
                ProbeController::new(Arc::new(WorkerProbeRunner { worker_program }));

            app.manage(DesktopState {
                camera_service: Mutex::new(camera_service),
                recording_controller: Mutex::new(recording_controller),
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
