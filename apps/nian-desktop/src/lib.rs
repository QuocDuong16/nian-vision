//! Desktop host process: owns managed application state and narrow Tauri commands.
//!
//! Media work never happens here; recording and probing are delegated to the
//! `nian-media-worker` process. The host resolves platform paths, owns native
//! credential-store plumbing, and maps typed application errors to stable DTOs.

#![forbid(unsafe_code)]

use std::path::PathBuf;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use chrono::NaiveDateTime;
use nian_application::{
    ApplicationSettingsDto, CameraDraft, CameraService, CameraServiceError, CameraSummary,
    CredentialStore, CredentialStoreError, DesktopLifecycle, DesktopLifecycleError,
    DesktopLifecycleState, PlaybackController, PlaybackError, PlaybackOpenDto, ProbeController,
    ProbeError, ProbeResult, RecordingController, RecordingControllerError, RecordingDto,
    RecordingState, RecordingStatus, SupervisorRecordingRunner, WorkerProbeRunner,
};
use nian_domain::{
    AudioPolicy, CameraId, CredentialRef, Credentials, RetentionPolicy, StorageQuota,
};
use nian_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt as AutostartManagerExt;
use tauri_plugin_updater::UpdaterExt;
use tracing_subscriber::EnvFilter;

const CREDENTIAL_SERVICE: &str = "Nian Vision";
const STARTUP_HIDDEN_ARG: &str = "--startup-hidden";
static PENDING_MANUAL_ACTIVATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AvailableUpdateDto {
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateCheckDto {
    pub configured: bool,
    pub current_version: String,
    pub available: Option<AvailableUpdateDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingIntentDto {
    pub camera_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesktopLifecycleDto {
    pub state: DesktopLifecycleState,
    pub startup_error: Option<DesktopErrorDto>,
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
    lifecycle: DesktopLifecycle,
    power_subscription: Mutex<Option<Box<dyn PowerEventSubscription>>>,
    power_dispatch_tx: Mutex<Option<mpsc::Sender<PowerDispatchMessage>>>,
    power_thread: Mutex<Option<JoinHandle<()>>>,
    tray_watch_tx: Mutex<Option<mpsc::Sender<TrayWatchMessage>>>,
    tray_thread: Mutex<Option<JoinHandle<()>>>,
    startup_error: Mutex<Option<DesktopErrorDto>>,
    update_installing: std::sync::atomic::AtomicBool,
    startup_complete: std::sync::atomic::AtomicBool,
    /// Serializes operations whose correctness depends on lifecycle admission
    /// and a stable recording ownership snapshot.
    control_gate: Mutex<()>,
}

impl std::fmt::Debug for DesktopState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopState").finish_non_exhaustive()
    }
}

struct TrayUi {
    _tray: TrayIcon<tauri::Wry>,
    status: MenuItem<tauri::Wry>,
    stop: MenuItem<tauri::Wry>,
}

trait AutostartService {
    fn is_enabled(&self) -> Result<bool, ()>;
    fn set_enabled(&self, enabled: bool) -> Result<(), ()>;
}

struct NativeAutostartService<'a> {
    app: &'a AppHandle,
}

impl AutostartService for NativeAutostartService<'_> {
    fn is_enabled(&self) -> Result<bool, ()> {
        self.app.autolaunch().is_enabled().map_err(|_| ())
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), ()> {
        if enabled {
            self.app.autolaunch().enable().map_err(|_| ())
        } else {
            self.app.autolaunch().disable().map_err(|_| ())
        }
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerDispatchMessage {
    Event(nian_platform_windows::PowerEvent),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrayWatchMessage {
    Status(RecordingStatus),
    RefreshIntent,
    Shutdown,
}

#[cfg(any(windows, test))]
type PowerEventCallback = Arc<dyn Fn(nian_platform_windows::PowerEvent) + Send + Sync + 'static>;

trait PowerEventSubscription: Send {}

impl<T: Send> PowerEventSubscription for T {}

#[cfg(any(windows, test))]
trait PowerEventSource {
    fn subscribe(
        &self,
        callback: PowerEventCallback,
    ) -> Result<Box<dyn PowerEventSubscription>, ()>;
}

#[cfg(windows)]
struct NativePowerEventSource;

#[cfg(windows)]
impl PowerEventSource for NativePowerEventSource {
    fn subscribe(
        &self,
        callback: PowerEventCallback,
    ) -> Result<Box<dyn PowerEventSubscription>, ()> {
        nian_platform_windows::subscribe(move |event| callback(event))
            .map(|subscription| Box::new(subscription) as Box<dyn PowerEventSubscription>)
            .map_err(|_| ())
    }
}

trait WindowActions {
    fn show(&self) -> Result<(), ()>;
    fn hide(&self) -> Result<(), ()>;
    fn unminimize(&self) -> Result<(), ()>;
    fn focus(&self) -> Result<(), ()>;
}

impl WindowActions for tauri::WebviewWindow {
    fn show(&self) -> Result<(), ()> {
        tauri::WebviewWindow::show(self).map_err(|_| ())
    }
    fn hide(&self) -> Result<(), ()> {
        tauri::WebviewWindow::hide(self).map_err(|_| ())
    }
    fn unminimize(&self) -> Result<(), ()> {
        tauri::WebviewWindow::unminimize(self).map_err(|_| ())
    }
    fn focus(&self) -> Result<(), ()> {
        self.set_focus().map_err(|_| ())
    }
}

impl WindowActions for tauri::Window {
    fn show(&self) -> Result<(), ()> {
        tauri::Window::show(self).map_err(|_| ())
    }
    fn hide(&self) -> Result<(), ()> {
        tauri::Window::hide(self).map_err(|_| ())
    }
    fn unminimize(&self) -> Result<(), ()> {
        tauri::Window::unminimize(self).map_err(|_| ())
    }
    fn focus(&self) -> Result<(), ()> {
        self.set_focus().map_err(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    HideAndPrevent,
    Allow,
}

fn close_action(lifecycle: &DesktopLifecycle) -> Result<CloseAction, DesktopErrorDto> {
    match lifecycle.state().map_err(map_lifecycle_error)? {
        DesktopLifecycleState::Quitting => Ok(CloseAction::Allow),
        DesktopLifecycleState::Running | DesktopLifecycleState::Suspending => {
            Ok(CloseAction::HideAndPrevent)
        }
    }
}

fn activate_window(
    lifecycle: &DesktopLifecycle,
    window: &impl WindowActions,
) -> Result<(), DesktopErrorDto> {
    lifecycle.require_running().map_err(map_lifecycle_error)?;
    window
        .show()
        .and_then(|_| window.unminimize())
        .and_then(|_| window.focus())
        .map_err(|_| DesktopErrorDto::new("lifecycle_failed", "main window could not be activated"))
}

fn activate_main_window(app: &AppHandle) -> Result<(), DesktopErrorDto> {
    let state = app.state::<Arc<DesktopState>>();
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| DesktopErrorDto::new("lifecycle_failed", "main window is unavailable"))?;
    activate_window(&state.lifecycle, &window)
}

fn require_running(state: &DesktopState) -> Result<(), DesktopErrorDto> {
    if state
        .update_installing
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(DesktopErrorDto::new(
            "update_in_progress",
            "application update is in progress",
        ));
    }
    state
        .lifecycle
        .require_running()
        .map_err(map_lifecycle_error)
}

fn admit_running(state: &DesktopState) -> Result<(), DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(state)
}

fn updater_configured() -> bool {
    matches!(option_env!("NIAN_UPDATER_CONFIGURED"), Some("1"))
}

#[allow(dead_code)] // Variants are target-specific; policy tests exercise all supported branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateInstallPlatform {
    LinuxAppImage,
    WindowsNsis,
    Unsupported,
}

#[derive(Debug)]
struct VerifiedUpdateBytes(Vec<u8>);

fn current_update_install_platform() -> UpdateInstallPlatform {
    #[cfg(target_os = "linux")]
    {
        UpdateInstallPlatform::LinuxAppImage
    }
    #[cfg(target_os = "windows")]
    {
        UpdateInstallPlatform::WindowsNsis
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        UpdateInstallPlatform::Unsupported
    }
}

fn validate_update_install_platform(
    platform: UpdateInstallPlatform,
    appimage_present: bool,
) -> Result<(), DesktopErrorDto> {
    match platform {
        UpdateInstallPlatform::LinuxAppImage if !appimage_present => Err(DesktopErrorDto::new(
            "update_unsupported",
            "in-app updates require the packaged Nian Vision AppImage",
        )),
        UpdateInstallPlatform::LinuxAppImage | UpdateInstallPlatform::WindowsNsis => Ok(()),
        UpdateInstallPlatform::Unsupported => Err(DesktopErrorDto::new(
            "update_unsupported",
            "application updates are not packaged for this platform",
        )),
    }
}

fn handoff_verified_update(
    platform: UpdateInstallPlatform,
    bytes: VerifiedUpdateBytes,
    mut shutdown: impl FnMut() -> Result<(), DesktopErrorDto>,
    mut install: impl FnMut(Vec<u8>) -> Result<(), DesktopErrorDto>,
    mut restart: impl FnMut(),
) -> Result<(), DesktopErrorDto> {
    if let Err(error) = shutdown() {
        restart();
        return Err(error);
    }
    if let Err(error) = install(bytes.0) {
        restart();
        return Err(error);
    }
    if platform == UpdateInstallPlatform::LinuxAppImage {
        restart();
    }
    Ok(())
}

fn map_update_error(_error: tauri_plugin_updater::Error) -> DesktopErrorDto {
    DesktopErrorDto::new(
        "update_failed",
        "cryptographically verified update operation failed",
    )
}

fn map_lifecycle_error(error: DesktopLifecycleError) -> DesktopErrorDto {
    match error {
        DesktopLifecycleError::Quitting => {
            DesktopErrorDto::new("quitting", "application is quitting")
        }
        DesktopLifecycleError::Suspending => {
            DesktopErrorDto::new("suspending", "application is suspending")
        }
        DesktopLifecycleError::Synchronization => {
            DesktopErrorDto::new("lifecycle_failed", "desktop lifecycle state is unavailable")
        }
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
async fn update_check(app: AppHandle) -> Result<UpdateCheckDto, DesktopErrorDto> {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    if !updater_configured() {
        return Ok(UpdateCheckDto {
            configured: false,
            current_version,
            available: None,
        });
    }

    let update = app
        .updater()
        .map_err(map_update_error)?
        .check()
        .await
        .map_err(map_update_error)?;
    Ok(UpdateCheckDto {
        configured: true,
        current_version,
        available: update.map(|update| AvailableUpdateDto {
            version: update.version.to_string(),
            notes: update.body,
            date: update.date.map(|date| date.to_string()),
        }),
    })
}

#[tauri::command]
async fn update_install(
    app: AppHandle,
    state: tauri::State<'_, Arc<DesktopState>>,
    expected_version: String,
) -> Result<(), DesktopErrorDto> {
    let platform = current_update_install_platform();
    validate_update_install_platform(platform, std::env::var_os("APPIMAGE").is_some())?;
    if !updater_configured() {
        return Err(DesktopErrorDto::new(
            "update_unconfigured",
            "no production update channel is configured for this build",
        ));
    }
    state
        .update_installing
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .map_err(|_| {
            DesktopErrorDto::new(
                "update_in_progress",
                "application update is already in progress",
            )
        })?;

    let package = async {
        let update = app
            .updater()
            .map_err(map_update_error)?
            .check()
            .await
            .map_err(map_update_error)?
            .ok_or_else(|| {
                DesktopErrorDto::new("update_unavailable", "no application update is available")
            })?;
        if update.version != expected_version {
            return Err(DesktopErrorDto::new(
                "update_changed",
                "available update changed; check for updates again",
            ));
        }
        let bytes = update
            .download(|_, _| {}, || {})
            .await
            .map_err(map_update_error)?;
        Ok::<_, DesktopErrorDto>((update, VerifiedUpdateBytes(bytes)))
    }
    .await;

    let (update, bytes) = match package {
        Ok(package) => package,
        Err(error) => {
            state
                .update_installing
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
    };

    // Update::download verifies the Tauri updater signature before returning.
    // Only verified bytes may cross this boundary into M7's terminal teardown.
    // Desired recording intent is deliberately untouched, so startup restoration
    // resumes it after either platform's updater restarts the new build.
    handoff_verified_update(
        platform,
        bytes,
        || {
            begin_update_shutdown(&state).inspect_err(|error| {
                tracing::error!(
                    code = error.code,
                    "update lifecycle teardown failed; restarting current build"
                );
            })
        },
        |bytes| {
            update.install(bytes).map_err(|error| {
                tracing::error!(error = %error, "signed updater handoff failed; restarting current build");
                map_update_error(error)
            })
        },
        || app.restart(),
    )
}

#[tauri::command]
fn camera_list(
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<Vec<CameraSummary>, DesktopErrorDto> {
    lock(&state.camera_service)?
        .list_cameras()
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_create(
    state: tauri::State<'_, Arc<DesktopState>>,
    input: CameraCommandInput,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    lock(&state.camera_service)?
        .create_camera(input.into_draft()?)
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_update(
    state: tauri::State<'_, Arc<DesktopState>>,
    input: CameraCommandInput,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    let active = lock(&state.recording_controller)?
        .active_camera()
        .map_err(map_recording_error)?;
    lock(&state.camera_service)?
        .update_camera(input.into_draft()?, active.as_ref())
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_delete(
    state: tauri::State<'_, Arc<DesktopState>>,
    camera_id: String,
) -> Result<nian_application::CameraMutation<CameraSummary>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    let active = lock(&state.recording_controller)?
        .active_camera()
        .map_err(map_recording_error)?;
    lock(&state.camera_service)?
        .delete_camera(&camera_id, active.as_ref())
        .map_err(map_camera_error)
}

#[tauri::command]
fn camera_probe(
    state: tauri::State<'_, Arc<DesktopState>>,
    input: ProbeCommandInput,
) -> Result<ProbeResult, DesktopErrorDto> {
    // Admission + secret-bearing preparation is serialized with lifecycle
    // transitions. Network I/O happens after releasing the control gate.
    let request = {
        let _gate = lock(&state.control_gate)?;
        require_running(&state)?;
        let draft = input.camera.into_draft()?;
        lock(&state.camera_service)?
            .prepare_probe_draft(&draft, input.timeout_ms)
            .map_err(map_camera_error)?
    };
    state
        .probe_controller
        .probe(request)
        .map_err(map_probe_error)
}

fn start_recording(
    state: &DesktopState,
    camera_id: &str,
) -> Result<RecordingStatus, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(state)?;
    let id = CameraId::parse(camera_id)
        .map_err(|error| DesktopErrorDto::new("validation", error.to_string()))?;
    let desired = lock(&state.camera_service)?
        .prepare_recording(camera_id)
        .map_err(map_camera_error)?;
    let mut controller = lock(&state.recording_controller)?;
    // Admission is proven before desired-state mutation. The control gate and
    // controller guard remain owned until runtime start, so a rejected second
    // camera can never replace the authoritative persisted intent.
    controller.ensure_startable().map_err(map_recording_error)?;
    lock(&state.camera_service)?
        .set_recording_enabled(camera_id, true)
        .map_err(map_desired_state_error)?;
    wake_tray_intent(state);
    // From here on desired=true is intentionally durable. Genuine thread/worker
    // startup failures leave it On so restart/restoration can retry user intent.
    controller.start(id, desired).map_err(map_recording_error)
}

#[tauri::command]
fn recording_start(
    state: tauri::State<'_, Arc<DesktopState>>,
    camera_id: String,
) -> Result<RecordingStatus, DesktopErrorDto> {
    start_recording(&state, &camera_id)
}

fn stop_recording(state: &DesktopState) -> Result<RecordingStatus, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(state)?;
    // Persist Off before touching the runtime controller. A crash while the
    // worker is stopping must not resurrect a recording the user stopped.
    let desired_camera = lock(&state.camera_service)?
        .desired_recording_camera()
        .map_err(map_desired_state_error)?;
    if let Some(desired_camera) = desired_camera.as_ref() {
        lock(&state.camera_service)?
            .set_recording_enabled(desired_camera.as_str(), false)
            .map_err(map_desired_state_error)?;
        wake_tray_intent(state);
    }
    let mut controller = lock(&state.recording_controller)?;
    let status = controller.status().map_err(map_recording_error)?;
    if status.state.is_active() && status.state != RecordingState::Stopping {
        return controller.stop().map_err(map_recording_error);
    }
    if desired_camera.is_some() {
        // A persisted Desired=On may legitimately have no live worker after a
        // startup restoration failure. Turning the intent Off is still a
        // successful Stop operation; do not manufacture a NotRecording error.
        return Ok(status);
    }
    controller.stop().map_err(map_recording_error)
}

#[tauri::command]
fn recording_stop(
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<RecordingStatus, DesktopErrorDto> {
    stop_recording(&state)
}

#[tauri::command]
fn recording_status(
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<RecordingStatus, DesktopErrorDto> {
    lock(&state.recording_controller)?
        .status()
        .map_err(map_recording_error)
}

#[tauri::command]
fn recording_intent(
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<RecordingIntentDto, DesktopErrorDto> {
    let camera_id = lock(&state.camera_service)?
        .desired_recording_camera()
        .map_err(map_desired_state_error)?
        .map(|camera| camera.as_str().to_owned());
    Ok(RecordingIntentDto { camera_id })
}

#[tauri::command]
fn desktop_lifecycle_status(
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<DesktopLifecycleDto, DesktopErrorDto> {
    Ok(DesktopLifecycleDto {
        state: state.lifecycle.state().map_err(map_lifecycle_error)?,
        startup_error: lock(&state.startup_error)?.clone(),
    })
}

#[tauri::command]
fn recordings_refresh(state: tauri::State<'_, Arc<DesktopState>>) -> Result<(), DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    lock(&state.playback_controller)?
        .refresh_index()
        .map_err(map_playback_error)
}

#[tauri::command]
fn recording_days(
    state: tauri::State<'_, Arc<DesktopState>>,
    camera_id: String,
) -> Result<Vec<String>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    let camera_id = CameraId::parse(&camera_id)
        .map_err(|error| DesktopErrorDto::new("validation", error.to_string()))?;
    lock(&state.playback_controller)?
        .recording_days(&camera_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn recording_timeline(
    state: tauri::State<'_, Arc<DesktopState>>,
    camera_id: String,
    start: String,
    end: String,
) -> Result<Vec<RecordingDto>, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
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
    state: tauri::State<'_, Arc<DesktopState>>,
    recording_id: String,
) -> Result<PlaybackOpenDto, DesktopErrorDto> {
    admit_running(&state)?;
    lock(&state.playback_controller)?
        .open(&recording_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_close(
    state: tauri::State<'_, Arc<DesktopState>>,
    session_id: String,
) -> Result<(), DesktopErrorDto> {
    lock(&state.playback_controller)?
        .close(&session_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_keepalive(
    state: tauri::State<'_, Arc<DesktopState>>,
    session_id: String,
) -> Result<(), DesktopErrorDto> {
    lock(&state.playback_controller)?
        .keep_alive(&session_id)
        .map_err(map_playback_error)
}

#[tauri::command]
fn playback_status(
    state: tauri::State<'_, Arc<DesktopState>>,
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
    state: tauri::State<'_, Arc<DesktopState>>,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    lock(&state.camera_service)?
        .application_settings()
        .map_err(map_camera_error)
}

#[tauri::command]
fn settings_update(
    app: AppHandle,
    state: tauri::State<'_, Arc<DesktopState>>,
    settings: ApplicationSettingsDto,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    require_running(&state)?;
    let active = lock(&state.recording_controller)?
        .status()
        .map_err(map_recording_error)?
        .state
        .is_active();
    let mut camera_service = lock(&state.camera_service)?;
    let mut playback = lock(&state.playback_controller)?;
    let autostart = NativeAutostartService { app: &app };
    update_settings_transaction(
        &mut camera_service,
        &mut playback,
        &autostart,
        settings,
        active,
    )
}

fn update_settings_transaction(
    camera_service: &mut CameraService,
    playback: &mut PlaybackController,
    autostart: &dyn AutostartService,
    settings: ApplicationSettingsDto,
    recording_active: bool,
) -> Result<ApplicationSettingsDto, DesktopErrorDto> {
    let previous = camera_service
        .application_settings()
        .map_err(map_camera_error)?;
    let prepared_settings = camera_service
        .prepare_application_settings(settings, recording_active)
        .map_err(map_camera_error)?;
    let prepared_dto = prepared_settings.dto();
    let critical_settings_changed = recording_storage_settings_changed(&previous, &prepared_dto);
    let prepared_playback = if critical_settings_changed {
        let (storage_root, retention, quota) = playback_storage_config(&prepared_dto)?;
        Some(
            playback
                .prepare_storage(storage_root, retention, quota)
                .map_err(map_playback_error)?,
        )
    } else {
        None
    };

    let autostart_changed = previous.launch_at_login != prepared_dto.launch_at_login;
    if autostart_changed {
        autostart
            .set_enabled(prepared_dto.launch_at_login)
            .map_err(|_| {
                DesktopErrorDto::new(
                    "autostart_failed",
                    "launch-at-login registration could not be changed",
                )
            })?;
    }

    let saved = match camera_service.commit_application_settings(prepared_settings) {
        Ok(saved) => saved,
        Err(error) => {
            if autostart_changed {
                if autostart.set_enabled(previous.launch_at_login).is_err() {
                    return Err(DesktopErrorDto::new(
                        "autostart_failed",
                        "settings persistence failed and launch-at-login rollback also failed",
                    ));
                }
                return Err(DesktopErrorDto::new(
                    "autostart_failed",
                    "settings persistence failed; launch-at-login registration was rolled back",
                ));
            }
            return Err(map_camera_error(error));
        }
    };
    if let Some(prepared_playback) = prepared_playback {
        playback.commit_prepared_storage(prepared_playback);
    }
    Ok(saved)
}

fn recording_storage_settings_changed(
    previous: &ApplicationSettingsDto,
    next: &ApplicationSettingsDto,
) -> bool {
    previous.storage_root != next.storage_root
        || previous.segment_target_secs != next.segment_target_secs
        || previous.max_age_days != next.max_age_days
        || previous.max_storage_bytes != next.max_storage_bytes
        || previous.cleanup_target_bytes != next.cleanup_target_bytes
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
        CameraServiceError::DesiredStateInvariant => DesktopErrorDto::new(
            "desired_state_failed",
            "persisted recording intent violates the single-camera invariant",
        ),
    }
}

fn map_desired_state_error(error: CameraServiceError) -> DesktopErrorDto {
    let mapped = map_camera_error(error);
    DesktopErrorDto::new("desired_state_failed", mapped.message)
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
        PlaybackError::LifecycleBlocked => DesktopErrorDto::new(
            "suspending",
            "playback admission is blocked by desktop lifecycle",
        ),
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

fn startup_hidden_args<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|arg| arg.as_ref() == STARTUP_HIDDEN_ARG)
}

fn manual_activation_should_run_now(startup_complete: bool) -> bool {
    if startup_complete {
        return true;
    }
    PENDING_MANUAL_ACTIVATION.store(true, std::sync::atomic::Ordering::Release);
    false
}

fn reconcile_autostart(
    service: &dyn AutostartService,
    persisted_enabled: bool,
) -> Result<(), DesktopErrorDto> {
    let actual = service.is_enabled().map_err(|_| {
        DesktopErrorDto::new(
            "autostart_failed",
            "launch-at-login state could not be inspected",
        )
    })?;
    if actual != persisted_enabled {
        service.set_enabled(persisted_enabled).map_err(|_| {
            DesktopErrorDto::new(
                "autostart_failed",
                "launch-at-login registration could not be reconciled",
            )
        })?;
    }
    Ok(())
}

fn restoration_failure_category(error: &CameraServiceError) -> &'static str {
    match error {
        CameraServiceError::CredentialStore(_) => "credential_store",
        CameraServiceError::StorageNotConfigured => "storage_failed",
        CameraServiceError::CameraNotFound => "configuration",
        CameraServiceError::Validation(_) => "validation",
        CameraServiceError::DesiredStateInvariant => "desired_state_failed",
        _ => "lifecycle_failed",
    }
}

/// Caller owns `control_gate` or is still in single-threaded startup.
fn restore_desired_recording_locked(
    state: &DesktopState,
) -> Result<Option<RecordingStatus>, DesktopErrorDto> {
    require_running(state)?;
    let desired_camera = lock(&state.camera_service)?
        .desired_recording_camera()
        .map_err(map_desired_state_error)?;
    let Some(camera_id) = desired_camera else {
        return Ok(None);
    };

    let desired = match lock(&state.camera_service)?.prepare_recording(camera_id.as_str()) {
        Ok(desired) => desired,
        Err(error) => {
            let category = restoration_failure_category(&error);
            let mapped = map_camera_error(error);
            let _ = lock(&state.recording_controller)?
                .mark_failed(camera_id, category)
                .map_err(map_recording_error)?;
            return Err(mapped);
        }
    };
    lock(&state.recording_controller)?
        .start(camera_id, desired)
        .map(Some)
        .map_err(map_recording_error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrayProjection {
    status_text: String,
    stop_enabled: bool,
}

fn tray_projection(status: &RecordingStatus, desired_on: bool) -> TrayProjection {
    let label = match status.state {
        RecordingState::Stopped => "Stopped",
        RecordingState::Starting => "Starting",
        RecordingState::Recovering => "Recovering",
        RecordingState::Connecting => "Connecting",
        RecordingState::Recording => "Recording",
        RecordingState::Backoff => "Backoff",
        RecordingState::Stopping => "Stopping",
        RecordingState::Failed => "Failed",
    };
    TrayProjection {
        status_text: format!("Recording status: {label}"),
        stop_enabled: desired_on
            || (status.state.is_active() && status.state != RecordingState::Stopping),
    }
}

fn render_tray_status(app: &AppHandle, status: &RecordingStatus) {
    let Some(state) = app.try_state::<Arc<DesktopState>>() else {
        return;
    };
    let Some(tray) = app.try_state::<TrayUi>() else {
        return;
    };
    let desired_on = lock(&state.camera_service)
        .and_then(|service| {
            service
                .desired_recording_camera()
                .map_err(map_desired_state_error)
        })
        .ok()
        .flatten()
        .is_some();
    let projection = tray_projection(status, desired_on);
    let _ = tray.status.set_text(projection.status_text);
    let _ = tray.stop.set_enabled(projection.stop_enabled);
}

fn refresh_tray(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<DesktopState>>() else {
        return;
    };
    let status = match lock(&state.recording_controller)
        .and_then(|mut c| c.status().map_err(map_recording_error))
    {
        Ok(status) => status,
        Err(_) => return,
    };
    render_tray_status(app, &status);
}

fn run_tray_watch_loop<F>(rx: mpsc::Receiver<TrayWatchMessage>, mut refresh: F)
where
    F: FnMut(&RecordingStatus),
{
    let mut status = RecordingStatus::default();
    loop {
        match rx.recv() {
            Ok(TrayWatchMessage::Status(next)) => status = next,
            Ok(TrayWatchMessage::RefreshIntent) => {}
            Ok(TrayWatchMessage::Shutdown) | Err(_) => break,
        }
        refresh(&status);
    }
}

fn start_tray_watcher(
    app: &tauri::App,
    state: &Arc<DesktopState>,
    rx: mpsc::Receiver<TrayWatchMessage>,
) -> Result<(), DesktopErrorDto> {
    let app_handle = app.handle().clone();
    let thread = std::thread::Builder::new()
        .name("desktop-tray-status".to_owned())
        .spawn(move || {
            run_tray_watch_loop(rx, |status| render_tray_status(&app_handle, status));
        })
        .map_err(|_| {
            DesktopErrorDto::new("lifecycle_failed", "tray status worker could not start")
        })?;
    *lock(&state.tray_thread)? = Some(thread);
    Ok(())
}

fn wake_tray_intent(state: &DesktopState) {
    if let Ok(sender) = state.tray_watch_tx.lock()
        && let Some(sender) = sender.as_ref()
    {
        let _ = sender.send(TrayWatchMessage::RefreshIntent);
    }
}

fn shutdown_tray_watcher(state: &DesktopState) {
    if let Ok(mut sender) = state.tray_watch_tx.lock()
        && let Some(sender) = sender.take()
    {
        let _ = sender.send(TrayWatchMessage::Shutdown);
    }
    if let Ok(mut thread) = state.tray_thread.lock()
        && let Some(thread) = thread.take()
    {
        let _ = thread.join();
    }
}

fn install_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("tray-open", "Open Nian Vision").build(app)?;
    let status = MenuItemBuilder::with_id("tray-recording-status", "Recording status: Stopped")
        .enabled(false)
        .build(app)?;
    let stop = MenuItemBuilder::with_id("tray-stop-recording", "Stop recording")
        .enabled(false)
        .build(app)?;
    let quit = MenuItemBuilder::with_id("tray-quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open, &status, &stop, &quit])
        .build()?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("Nian Vision")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-open" => {
                let _ = activate_main_window(app);
            }
            "tray-stop-recording" => {
                if let Some(state) = app.try_state::<Arc<DesktopState>>() {
                    let _ = stop_recording(&state);
                    refresh_tray(app);
                }
            }
            "tray-quit" => {
                let _ = request_quit(app);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let activate = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if activate {
                let _ = activate_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    app.manage(TrayUi {
        _tray: tray,
        status,
        stop,
    });
    refresh_tray(app.handle());
    Ok(())
}

#[cfg(test)]
fn run_shutdown_sequence<R, P, Q, T, W, E>(
    recording: R,
    playback: P,
    probe: Q,
    tray: T,
    power: W,
    exit: E,
) where
    R: FnOnce(),
    P: FnOnce(),
    Q: FnOnce(),
    T: FnOnce(),
    W: FnOnce(),
    E: FnOnce(),
{
    recording();
    playback();
    probe();
    tray();
    power();
    exit();
}

fn shutdown_power_runtime(state: &DesktopState) {
    // Dispatcher shutdown is explicit and independent of callback ownership.
    if let Ok(mut control) = state.power_dispatch_tx.lock()
        && let Some(control) = control.take()
    {
        let _ = control.send(PowerDispatchMessage::Shutdown);
    }
    if let Ok(mut subscription) = state.power_subscription.lock() {
        subscription.take();
    }
    if let Ok(mut thread) = state.power_thread.lock()
        && let Some(thread) = thread.take()
    {
        let _ = thread.join();
    }
}

fn shutdown_runtime_resources(state: &DesktopState) -> Result<(), DesktopErrorDto> {
    let mut first_error = None;
    match state.recording_controller.lock() {
        Ok(mut controller) => {
            if let Err(error) = controller.shutdown() {
                capture_first_error(&mut first_error, map_recording_error(error));
            }
        }
        Err(_) => capture_first_error(
            &mut first_error,
            DesktopErrorDto::new("lifecycle_failed", "recording shutdown failed"),
        ),
    }
    match state.playback_controller.lock() {
        Ok(mut playback) => playback.shutdown(),
        Err(_) => capture_first_error(
            &mut first_error,
            DesktopErrorDto::new("lifecycle_failed", "playback shutdown failed"),
        ),
    }
    if let Err(error) = state.probe_controller.shutdown() {
        capture_first_error(&mut first_error, map_probe_error(error));
    }
    shutdown_tray_watcher(state);
    shutdown_power_runtime(state);
    first_error.map_or(Ok(()), Err)
}

fn begin_update_shutdown(state: &DesktopState) -> Result<(), DesktopErrorDto> {
    let mut admission_error = None;
    {
        let _gate = lock(&state.control_gate)?;
        if !state
            .update_installing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(DesktopErrorDto::new(
                "update_failed",
                "update handoff was not admitted",
            ));
        }
        if !state.lifecycle.begin_quit().map_err(map_lifecycle_error)? {
            return Err(DesktopErrorDto::new(
                "quitting",
                "application is already quitting",
            ));
        }
        state.probe_controller.stop_accepting_and_cancel();
        match state.playback_controller.lock() {
            Ok(mut playback) => playback.stop_accepting(),
            Err(_) => capture_first_error(
                &mut admission_error,
                DesktopErrorDto::new("lifecycle_failed", "playback update admission failed"),
            ),
        }
        match state.recording_controller.lock() {
            Ok(mut controller) => {
                if let Err(error) = controller.request_lifecycle_stop() {
                    capture_first_error(&mut admission_error, map_recording_error(error));
                }
            }
            Err(_) => capture_first_error(
                &mut admission_error,
                DesktopErrorDto::new("lifecycle_failed", "recording update admission failed"),
            ),
        }
    }
    if let Some(error) = admission_error {
        return Err(error);
    }
    shutdown_runtime_resources(state)
}

fn finish_quit(state: &DesktopState, app: &AppHandle) {
    let _ = shutdown_runtime_resources(state);
    app.exit(0);
}

fn request_quit(app: &AppHandle) -> Result<(), DesktopErrorDto> {
    let state = app.try_state::<Arc<DesktopState>>().ok_or_else(|| {
        DesktopErrorDto::new("lifecycle_failed", "application state is unavailable")
    })?;
    let mut admission_error = None;
    {
        let _gate = lock(&state.control_gate)?;
        if !state.lifecycle.begin_quit().map_err(map_lifecycle_error)? {
            return Ok(());
        }
        // Admission closes before any potentially blocking teardown begins.
        state.probe_controller.stop_accepting_and_cancel();
        match state.playback_controller.lock() {
            Ok(mut playback) => playback.stop_accepting(),
            Err(_) => capture_first_error(
                &mut admission_error,
                DesktopErrorDto::new("lifecycle_failed", "playback shutdown admission failed"),
            ),
        }
        match state.recording_controller.lock() {
            Ok(mut controller) => {
                if let Err(error) = controller.request_lifecycle_stop() {
                    capture_first_error(&mut admission_error, map_recording_error(error));
                }
            }
            Err(_) => capture_first_error(
                &mut admission_error,
                DesktopErrorDto::new("lifecycle_failed", "recording shutdown admission failed"),
            ),
        }
    }

    let thread_state = state.inner().clone();
    let thread_app = app.clone();
    let fallback_state = thread_state.clone();
    let fallback_app = thread_app.clone();
    if std::thread::Builder::new()
        .name("desktop-graceful-quit".to_owned())
        .spawn(move || finish_quit(&thread_state, &thread_app))
        .is_err()
    {
        // Thread creation failure must not strand an already-Quitting process.
        // Existing controller teardown paths are bounded, so synchronous
        // fallback is preferable to leaving the host permanently wedged.
        finish_quit(&fallback_state, &fallback_app);
    }
    if let Some(error) = admission_error {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn capture_first_error(first: &mut Option<DesktopErrorDto>, error: DesktopErrorDto) {
    if first.is_none() {
        *first = Some(error);
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn handle_power_event(
    state: &DesktopState,
    event: nian_platform_windows::PowerEvent,
) -> Result<(), DesktopErrorDto> {
    let _gate = lock(&state.control_gate)?;
    match event {
        nian_platform_windows::PowerEvent::Suspend => {
            if !state
                .lifecycle
                .begin_suspend()
                .map_err(map_lifecycle_error)?
            {
                return Ok(());
            }
            let mut first_error = None;
            state.probe_controller.stop_accepting_and_cancel();
            match state.playback_controller.lock() {
                Ok(mut playback) => playback.stop_accepting(),
                Err(_) => capture_first_error(
                    &mut first_error,
                    DesktopErrorDto::new("internal", "playback controller is unavailable"),
                ),
            }
            match state.recording_controller.lock() {
                Ok(mut controller) => {
                    if let Err(error) = controller.request_lifecycle_stop() {
                        capture_first_error(&mut first_error, map_recording_error(error));
                    }
                }
                Err(_) => capture_first_error(
                    &mut first_error,
                    DesktopErrorDto::new("internal", "recording controller is unavailable"),
                ),
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        nian_platform_windows::PowerEvent::Resume => {
            if !state.lifecycle.resume().map_err(map_lifecycle_error)? {
                return Ok(());
            }
            let mut first_error = None;
            match state.playback_controller.lock() {
                Ok(mut playback) => {
                    if let Err(error) = playback.resume_resync() {
                        capture_first_error(&mut first_error, map_playback_error(error));
                    }
                    // A failed index refresh must not wedge playback admission.
                    // Normal playback commands can surface/retry the real error.
                    playback.resume_accepting();
                }
                Err(_) => capture_first_error(
                    &mut first_error,
                    DesktopErrorDto::new("internal", "playback controller is unavailable"),
                ),
            }
            // Suspend requested cooperative interruption. Finish ownership of
            // that controller before any restoration, then reuse normal start.
            match state.recording_controller.lock() {
                Ok(mut controller) => {
                    if let Err(error) = controller.shutdown() {
                        capture_first_error(&mut first_error, map_recording_error(error));
                    }
                }
                Err(_) => capture_first_error(
                    &mut first_error,
                    DesktopErrorDto::new("internal", "recording controller is unavailable"),
                ),
            }
            let active = match state.recording_controller.lock() {
                Ok(mut controller) => match controller.active_camera() {
                    Ok(active) => active,
                    Err(error) => {
                        capture_first_error(&mut first_error, map_recording_error(error));
                        None
                    }
                },
                Err(_) => {
                    capture_first_error(
                        &mut first_error,
                        DesktopErrorDto::new("internal", "recording controller is unavailable"),
                    );
                    None
                }
            };
            if active.is_none()
                && let Err(error) = restore_desired_recording_locked(state)
            {
                capture_first_error(&mut first_error, error);
            }
            state.probe_controller.resume_accepting();
            if let Some(error) = first_error {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn subscribe_power_source(
    source: &dyn PowerEventSource,
    tx: mpsc::Sender<PowerDispatchMessage>,
) -> Result<Box<dyn PowerEventSubscription>, DesktopErrorDto> {
    source
        .subscribe(Arc::new(move |event| {
            let _ = tx.send(PowerDispatchMessage::Event(event));
        }))
        .map_err(|_| {
            DesktopErrorDto::new(
                "lifecycle_failed",
                "Windows power notifications could not be registered",
            )
        })
}

#[cfg(windows)]
fn prepare_power_events(
    state: &Arc<DesktopState>,
) -> Result<Option<mpsc::Receiver<PowerDispatchMessage>>, DesktopErrorDto> {
    let (tx, rx) = mpsc::channel();
    let source = NativePowerEventSource;
    let subscription = subscribe_power_source(&source, tx.clone())?;
    *lock(&state.power_subscription)? = Some(subscription);
    *lock(&state.power_dispatch_tx)? = Some(tx);
    Ok(Some(rx))
}

#[cfg(not(windows))]
fn prepare_power_events(
    _state: &Arc<DesktopState>,
) -> Result<Option<mpsc::Receiver<PowerDispatchMessage>>, DesktopErrorDto> {
    Ok(None)
}

fn run_power_dispatch_loop<F>(rx: mpsc::Receiver<PowerDispatchMessage>, mut handle_event: F)
where
    F: FnMut(nian_platform_windows::PowerEvent),
{
    while let Ok(PowerDispatchMessage::Event(event)) = rx.recv() {
        handle_event(event);
    }
}

fn start_power_dispatcher(
    app: &tauri::App,
    state: &Arc<DesktopState>,
    rx: Option<mpsc::Receiver<PowerDispatchMessage>>,
) -> Result<(), DesktopErrorDto> {
    let Some(rx) = rx else {
        return Ok(());
    };
    let state_for_thread = state.clone();
    let app_handle = app.handle().clone();
    let thread = std::thread::Builder::new()
        .name("desktop-power-events".to_owned())
        .spawn(move || {
            run_power_dispatch_loop(rx, |event| {
                if let Err(error) = handle_power_event(&state_for_thread, event) {
                    tracing::warn!(code = error.code, "power lifecycle handling failed");
                }
                refresh_tray(&app_handle);
            });
        })
        .map_err(|_| {
            DesktopErrorDto::new("lifecycle_failed", "power event worker could not start")
        })?;
    *lock(&state.power_thread)? = Some(thread);
    Ok(())
}

fn emit_startup_smoke_marker() -> std::io::Result<()> {
    if let Some(path) = std::env::var_os("NIAN_DESKTOP_STARTUP_SMOKE_FILE") {
        std::fs::write(path, b"desktop_startup_ready\n")?;
    }
    Ok(())
}

#[cfg(windows)]
fn emit_power_subscription_smoke_marker() -> std::io::Result<()> {
    if let Some(path) = std::env::var_os("NIAN_DESKTOP_POWER_SMOKE_FILE") {
        std::fs::write(path, b"windows_power_subscription_ready\n")?;
    }
    Ok(())
}

#[cfg(windows)]
fn start_containment_smoke_worker() -> std::io::Result<()> {
    let Some(marker) = std::env::var_os("NIAN_DESKTOP_CONTAINMENT_SMOKE_FILE") else {
        return Ok(());
    };
    let worker = std::env::current_exe()?.with_file_name("nian-media-worker.exe");
    let mut child = Command::new(worker)
        .arg("__containment-smoke")
        .env("NIAN_WORKER_CONTAINMENT_SMOKE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Err(error) = nian_platform_windows::contain_worker_process(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let pid = child.id();
    // This child ignores stdin, so hard-death disappearance proves containment.
    std::fs::write(marker, format!("{pid}\n"))?;
    Ok(())
}

/// Initializes logging and starts the Tauri runtime.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let startup_hidden = startup_hidden_args(std::env::args());
    let builder = tauri::Builder::default()
        // Tauri requires the single-instance plugin to be registered first so
        // a secondary process exits before backend resources are initialized.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // Background autostart duplication must not unexpectedly surface a
            // window. A manual second launch is an explicit activation signal.
            if !startup_hidden_args(&args) {
                let startup_complete = app.try_state::<Arc<DesktopState>>().is_some_and(|state| {
                    state
                        .startup_complete
                        .load(std::sync::atomic::Ordering::Acquire)
                });
                if manual_activation_should_run_now(startup_complete) {
                    let _ = activate_main_window(app);
                }
            }
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Nian Vision")
                .arg(STARTUP_HIDDEN_ARG)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && let Some(state) = window.app_handle().try_state::<Arc<DesktopState>>()
                && matches!(
                    close_action(&state.lifecycle),
                    Ok(CloseAction::HideAndPrevent)
                )
            {
                api.prevent_close();
                let _ = WindowActions::hide(window);
            }
        })
        .setup(move |app| {
            nian_platform_windows::initialize_worker_process_containment().map_err(|_| {
                std::io::Error::other("media worker process containment could not be initialized")
            })?;
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
            let (tray_watch_tx, tray_watch_rx) = mpsc::channel();
            let mut recording_controller =
                RecordingController::new(Arc::new(SupervisorRecordingRunner {
                    worker_program: worker_program.clone(),
                }));
            let tray_status_tx = tray_watch_tx.clone();
            recording_controller
                .set_status_observer(Arc::new(move |status| {
                    let _ = tray_status_tx.send(TrayWatchMessage::Status(status));
                }))
                .map_err(|_| {
                    std::io::Error::other("tray status observer could not be installed")
                })?;
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

            let autostart = NativeAutostartService { app: app.handle() };
            let startup_error = reconcile_autostart(&autostart, initial_settings.launch_at_login)
                .inspect_err(|error| {
                    tracing::warn!(code = error.code, "autostart reconciliation failed");
                })
                .err();

            let state = Arc::new(DesktopState {
                camera_service: Mutex::new(camera_service),
                recording_controller: Mutex::new(recording_controller),
                playback_controller: Mutex::new(playback_controller),
                probe_controller,
                lifecycle: DesktopLifecycle::new(),
                power_subscription: Mutex::new(None),
                power_dispatch_tx: Mutex::new(None),
                power_thread: Mutex::new(None),
                tray_watch_tx: Mutex::new(Some(tray_watch_tx)),
                tray_thread: Mutex::new(None),
                startup_error: Mutex::new(startup_error),
                update_installing: std::sync::atomic::AtomicBool::new(false),
                startup_complete: std::sync::atomic::AtomicBool::new(false),
                control_gate: Mutex::new(()),
            });
            app.manage(state.clone());

            // Subscribe before restoration so Windows cannot lose early power
            // events, but do not dispatch them concurrently with startup. The
            // receiver queues them until authoritative initialization finishes.
            let power_rx = match prepare_power_events(&state) {
                Ok(rx) => {
                    #[cfg(windows)]
                    emit_power_subscription_smoke_marker()?;
                    rx
                }
                Err(error) => {
                    tracing::warn!(code = error.code, "power event source is unavailable");
                    *lock(&state.startup_error)
                        .map_err(|error| std::io::Error::other(error.message))? = Some(error);
                    None
                }
            };
            install_tray(app)?;

            {
                start_tray_watcher(app, &state, tray_watch_rx)
                    .map_err(|error| std::io::Error::other(error.message))?;

                let _gate = lock(&state.control_gate)
                    .map_err(|error| std::io::Error::other(error.message))?;
                if let Err(error) = restore_desired_recording_locked(&state) {
                    tracing::warn!(code = error.code, "desired recording restoration failed");
                    if let Ok(mut startup_error) = state.startup_error.lock()
                        && startup_error.is_none()
                    {
                        *startup_error = Some(error);
                    }
                }
            }
            refresh_tray(app.handle());

            let pending_manual_activation =
                PENDING_MANUAL_ACTIVATION.swap(false, std::sync::atomic::Ordering::AcqRel);
            if startup_hidden && !pending_manual_activation {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = WindowActions::hide(&window);
                }
            } else if let Err(error) = activate_main_window(app.handle()) {
                tracing::warn!(code = error.code, "main window activation failed");
            }
            state
                .startup_complete
                .store(true, std::sync::atomic::Ordering::Release);
            // Close the race between consuming the startup latch above and
            // publishing startup_complete. A manual second launch in that tiny
            // window is latched by the callback and consumed here.
            if PENDING_MANUAL_ACTIVATION.swap(false, std::sync::atomic::Ordering::AcqRel)
                && let Err(error) = activate_main_window(app.handle())
            {
                tracing::warn!(code = error.code, "deferred main window activation failed");
            }
            if let Err(error) = start_power_dispatcher(app, &state, power_rx) {
                tracing::warn!(code = error.code, "power event dispatcher could not start");
                if let Ok(mut startup_error) = state.startup_error.lock()
                    && startup_error.is_none()
                {
                    *startup_error = Some(error);
                }
            }
            emit_startup_smoke_marker()?;
            #[cfg(windows)]
            start_containment_smoke_worker()?;
            tracing::info!(event = "desktop_startup_ready", "desktop startup ready");

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            update_check,
            update_install,
            camera_list,
            camera_create,
            camera_update,
            camera_delete,
            camera_probe,
            recording_start,
            recording_stop,
            recording_status,
            recording_intent,
            desktop_lifecycle_status,
            recordings_refresh,
            recording_days,
            recording_timeline,
            playback_open,
            playback_close,
            playback_keepalive,
            playback_status,
            settings_get,
            settings_update,
        ]);

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(error) => {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("nian-desktop: fatal: {error}");
            }
            std::process::exit(1);
        }
    };

    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event
            && let Some(state) = app.try_state::<Arc<DesktopState>>()
        {
            match state.lifecycle.state() {
                Ok(DesktopLifecycleState::Quitting) => {}
                Ok(DesktopLifecycleState::Running | DesktopLifecycleState::Suspending) => {
                    api.prevent_exit();
                    if let Err(error) = request_quit(app) {
                        tracing::warn!(code = error.code, "coordinated exit request failed");
                    }
                }
                Err(_) => api.prevent_exit(),
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Debug)]
    struct TestRepositoryState {
        settings: nian_settings::ApplicationSettings,
        fail_save: bool,
        fail_desired_write: bool,
        cameras: Vec<nian_domain::CameraConfig>,
        desired_camera: Option<String>,
        extra_desired_camera: Option<String>,
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
            Ok(self.state.lock().unwrap().cameras.clone())
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
                .cameras
                .iter()
                .find(|camera| camera.camera_id() == camera_id)
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

        fn recording_enabled_cameras(
            &self,
        ) -> Result<Vec<CameraId>, nian_application::SettingsRepositoryError> {
            let state = self.state.lock().unwrap();
            state
                .desired_camera
                .iter()
                .chain(state.extra_desired_camera.iter())
                .map(CameraId::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| nian_application::SettingsRepositoryError::Persistence)
        }

        fn set_recording_enabled(
            &mut self,
            camera_id: &CameraId,
            enabled: bool,
        ) -> Result<bool, nian_application::SettingsRepositoryError> {
            let mut state = self.state.lock().unwrap();
            if state.fail_desired_write {
                return Err(nian_application::SettingsRepositoryError::Persistence);
            }
            if !state
                .cameras
                .iter()
                .any(|camera| camera.camera_id() == camera_id)
            {
                return Ok(false);
            }
            state.desired_camera = enabled.then(|| camera_id.as_str().to_owned());
            Ok(true)
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
                launch_at_login: false,
            },
            fail_save: false,
            fail_desired_write: false,
            cameras: Vec::new(),
            desired_camera: None,
            extra_desired_camera: None,
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
            launch_at_login: false,
        }
    }

    #[derive(Default)]
    struct FakeAutostart {
        enabled: Mutex<bool>,
        fail_inspect: bool,
        fail_set: bool,
        fail_on_call: Option<usize>,
        calls: Mutex<Vec<bool>>,
    }

    impl AutostartService for FakeAutostart {
        fn is_enabled(&self) -> Result<bool, ()> {
            if self.fail_inspect {
                Err(())
            } else {
                Ok(*self.enabled.lock().unwrap())
            }
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), ()> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(enabled);
            let call_number = calls.len();
            drop(calls);
            if self.fail_set || self.fail_on_call == Some(call_number) {
                return Err(());
            }
            *self.enabled.lock().unwrap() = enabled;
            Ok(())
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

    #[derive(Default)]
    struct CountingRecordingRunner {
        starts: AtomicUsize,
    }

    impl nian_application::RecordingRunner for CountingRecordingRunner {
        fn run(
            &self,
            _desired: nian_application::DesiredRecording,
            stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<nian_application::WorkerEnd, nian_application::RecordingRunFailure> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(nian_application::WorkerEnd::RequestedShutdown)
        }
    }

    #[derive(Default)]
    struct ParkedStoppingRunner {
        starts: AtomicUsize,
        stop_seen: AtomicBool,
        release: AtomicBool,
    }

    impl nian_application::RecordingRunner for ParkedStoppingRunner {
        fn run(
            &self,
            _desired: nian_application::DesiredRecording,
            stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<nian_application::WorkerEnd, nian_application::RecordingRunFailure> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            self.stop_seen.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(nian_application::WorkerEnd::RequestedShutdown)
        }
    }

    struct ScriptedTransitionRunner;

    impl nian_application::RecordingRunner for ScriptedTransitionRunner {
        fn run(
            &self,
            _desired: nian_application::DesiredRecording,
            _stop: Arc<AtomicBool>,
            observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<nian_application::WorkerEnd, nian_application::RecordingRunFailure> {
            for state in ["recovering", "connecting", "recording", "backoff"] {
                observer(&serde_json::json!({"state": state}));
            }
            Err(nian_application::RecordingRunFailure::Permanent {
                failure_category: Some("source_open_failed".to_owned()),
            })
        }
    }

    struct FailingRecordingThreadSpawner;

    impl nian_application::RecordingThreadSpawner for FailingRecordingThreadSpawner {
        fn spawn(
            &self,
            _name: String,
            _task: Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<JoinHandle<()>> {
            Err(std::io::Error::other("injected recording thread failure"))
        }
    }

    struct FakePlaybackBackend;

    impl nian_application::PlaybackBackend for FakePlaybackBackend {
        fn prepare(
            &self,
            source_path: &std::path::Path,
            output_path: &std::path::Path,
        ) -> Result<nian_application::PlaybackInspectDto, PlaybackError> {
            let metadata = std::fs::symlink_metadata(source_path)
                .map_err(|_| PlaybackError::MediaUnreadable)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(PlaybackError::MediaUnreadable);
            }
            std::fs::write(output_path, b"prepared-media").map_err(|_| PlaybackError::Internal)?;
            Ok(nian_application::PlaybackInspectDto {
                duration_ms: Some(5_000),
                video_codec: "h264".to_owned(),
                width: Some(1920),
                height: Some(1080),
                audio_available: false,
                container_compatibility: "fragmented_mp4".to_owned(),
                seekable: true,
            })
        }
    }

    #[derive(Default)]
    struct ImmediateProbeRunner {
        calls: AtomicUsize,
    }

    impl nian_application::ProbeRunner for ImmediateProbeRunner {
        fn run(
            &self,
            _request: nian_application::PreparedProbe,
            cancel: Arc<AtomicBool>,
        ) -> Result<ProbeResult, ProbeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancel.load(Ordering::Acquire) {
                return Err(ProbeError::Cancelled);
            }
            Ok(ProbeResult {
                reachable: true,
                video_stream_found: true,
                codec: Some("h264".to_owned()),
                width: Some(1920),
                height: Some(1080),
                audio_stream_count: 1,
            })
        }
    }

    struct FakePowerEventSource {
        event: nian_platform_windows::PowerEvent,
        subscriptions: AtomicUsize,
    }

    impl PowerEventSource for FakePowerEventSource {
        fn subscribe(
            &self,
            callback: PowerEventCallback,
        ) -> Result<Box<dyn PowerEventSubscription>, ()> {
            self.subscriptions.fetch_add(1, Ordering::SeqCst);
            callback(self.event);
            Ok(Box::new(()))
        }
    }

    struct RetainedCallbackSubscription {
        callback: Option<PowerEventCallback>,
        leak_on_drop: bool,
        reclaimed: Arc<AtomicBool>,
    }

    impl Drop for RetainedCallbackSubscription {
        fn drop(&mut self) {
            let Some(callback) = self.callback.take() else {
                return;
            };
            if self.leak_on_drop {
                std::mem::forget(callback);
            } else {
                drop(callback);
                self.reclaimed.store(true, Ordering::Release);
            }
        }
    }

    struct RetainingPowerEventSource {
        leak_on_drop: bool,
        reclaimed: Arc<AtomicBool>,
    }

    impl PowerEventSource for RetainingPowerEventSource {
        fn subscribe(
            &self,
            callback: PowerEventCallback,
        ) -> Result<Box<dyn PowerEventSubscription>, ()> {
            Ok(Box::new(RetainedCallbackSubscription {
                callback: Some(callback),
                leak_on_drop: self.leak_on_drop,
                reclaimed: self.reclaimed.clone(),
            }))
        }
    }

    fn seed_recording_camera(
        repository: &Arc<Mutex<TestRepositoryState>>,
        credentials: &Arc<nian_application::MemoryCredentialStore>,
        desired: bool,
    ) {
        let credential_ref = CredentialRef::parse("desktop-lifecycle-test-ref").unwrap();
        credentials
            .put(&credential_ref, &Credentials::new("admin", "secret"))
            .unwrap();
        repository.lock().unwrap().cameras.push(
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
        repository.lock().unwrap().desired_camera = desired.then(|| "front-door".to_owned());
    }

    fn seed_second_recording_camera(repository: &Arc<Mutex<TestRepositoryState>>) {
        let credential_ref = CredentialRef::parse("desktop-lifecycle-test-ref").unwrap();
        repository.lock().unwrap().cameras.push(
            nian_domain::CameraConfig::new(
                CameraId::parse("back-door").unwrap(),
                "Back door",
                nian_domain::CameraSource::Rtsp(
                    nian_domain::CameraEndpoint::new(
                        nian_domain::Host::parse("192.168.1.51").unwrap(),
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
    }

    fn lifecycle_state(
        root: &std::path::Path,
        configure_playback_storage: bool,
    ) -> (
        DesktopState,
        Arc<Mutex<TestRepositoryState>>,
        Arc<CountingRecordingRunner>,
        Arc<ImmediateProbeRunner>,
    ) {
        let (service, repository, credentials) = test_service(root);
        seed_recording_camera(&repository, &credentials, true);
        let recording_runner = Arc::new(CountingRecordingRunner::default());
        let recording_controller = RecordingController::new(recording_runner.clone());
        let probe_runner = Arc::new(ImmediateProbeRunner::default());
        let probe_controller = ProbeController::new(probe_runner.clone());
        let cache_root = root.with_extension("playback-cache");
        let playback_controller = if configure_playback_storage {
            configured_playback(root, cache_root)
        } else {
            PlaybackController::new("unused-test-worker".to_owned(), cache_root).unwrap()
        };

        (
            DesktopState {
                camera_service: Mutex::new(service),
                recording_controller: Mutex::new(recording_controller),
                playback_controller: Mutex::new(playback_controller),
                probe_controller,
                lifecycle: DesktopLifecycle::new(),
                power_subscription: Mutex::new(None),
                power_dispatch_tx: Mutex::new(None),
                power_thread: Mutex::new(None),
                tray_watch_tx: Mutex::new(None),
                tray_thread: Mutex::new(None),
                startup_error: Mutex::new(None),
                update_installing: std::sync::atomic::AtomicBool::new(false),
                startup_complete: std::sync::atomic::AtomicBool::new(false),
                control_gate: Mutex::new(()),
            },
            repository,
            recording_runner,
            probe_runner,
        )
    }

    fn wait_for_starts(runner: &CountingRecordingRunner, expected: usize) {
        for _ in 0..200 {
            if runner.starts.load(Ordering::SeqCst) >= expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("recording runner did not reach expected start count");
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

    #[derive(Default)]
    struct FakeWindow {
        calls: Mutex<Vec<&'static str>>,
    }

    impl WindowActions for FakeWindow {
        fn show(&self) -> Result<(), ()> {
            self.calls.lock().unwrap().push("show");
            Ok(())
        }

        fn hide(&self) -> Result<(), ()> {
            self.calls.lock().unwrap().push("hide");
            Ok(())
        }

        fn unminimize(&self) -> Result<(), ()> {
            self.calls.lock().unwrap().push("unminimize");
            Ok(())
        }

        fn focus(&self) -> Result<(), ()> {
            self.calls.lock().unwrap().push("focus");
            Ok(())
        }
    }

    #[test]
    fn activation_reports_suspending_and_quitting_truthfully() {
        let lifecycle = DesktopLifecycle::new();
        let window = FakeWindow::default();

        activate_window(&lifecycle, &window).unwrap();
        assert_eq!(
            *window.calls.lock().unwrap(),
            vec!["show", "unminimize", "focus"]
        );

        lifecycle.begin_suspend().unwrap();
        let error = activate_window(&lifecycle, &window).unwrap_err();
        assert_eq!(error.code, "suspending");

        lifecycle.begin_quit().unwrap();
        let error = activate_window(&lifecycle, &window).unwrap_err();
        assert_eq!(error.code, "quitting");
        assert_eq!(
            *window.calls.lock().unwrap(),
            vec!["show", "unminimize", "focus"]
        );
    }

    #[test]
    fn close_hides_normally_but_allows_real_close_while_quitting() {
        let running = DesktopLifecycle::new();
        assert_eq!(close_action(&running).unwrap(), CloseAction::HideAndPrevent);
        running.begin_suspend().unwrap();
        assert_eq!(close_action(&running).unwrap(), CloseAction::HideAndPrevent);

        let quitting = DesktopLifecycle::new();
        quitting.begin_quit().unwrap();
        assert_eq!(close_action(&quitting).unwrap(), CloseAction::Allow);
    }

    #[test]
    fn update_admission_blocks_new_operations_before_teardown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, _runner, _probe) = lifecycle_state(&root, false);

        state.update_installing.store(true, Ordering::Release);
        let error = require_running(&state).unwrap_err();

        assert_eq!(error.code, "update_in_progress");
        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Running
        );
    }

    #[test]
    fn windows_update_install_is_supported_without_appimage() {
        validate_update_install_platform(UpdateInstallPlatform::WindowsNsis, false).unwrap();
        validate_update_install_platform(UpdateInstallPlatform::WindowsNsis, true).unwrap();
    }

    #[test]
    fn linux_update_install_requires_appimage() {
        let error = validate_update_install_platform(UpdateInstallPlatform::LinuxAppImage, false)
            .unwrap_err();
        assert_eq!(error.code, "update_unsupported");
        validate_update_install_platform(UpdateInstallPlatform::LinuxAppImage, true).unwrap();
    }

    #[test]
    fn unsupported_update_platform_is_rejected() {
        let error = validate_update_install_platform(UpdateInstallPlatform::Unsupported, false)
            .unwrap_err();
        assert_eq!(error.code, "update_unsupported");
    }

    #[test]
    fn windows_verified_update_tears_down_before_handoff_without_app_restart() {
        let events = std::cell::RefCell::new(vec!["verified"]);
        let restarts = std::cell::Cell::new(0usize);

        handoff_verified_update(
            UpdateInstallPlatform::WindowsNsis,
            VerifiedUpdateBytes(vec![1, 2, 3]),
            || {
                events.borrow_mut().push("teardown");
                Ok(())
            },
            |bytes| {
                assert!(!bytes.is_empty());
                events.borrow_mut().push("installer_handoff");
                Ok(())
            },
            || restarts.set(restarts.get() + 1),
        )
        .unwrap();

        assert_eq!(
            events.into_inner(),
            vec!["verified", "teardown", "installer_handoff"]
        );
        assert_eq!(restarts.get(), 0);
    }

    #[test]
    fn linux_verified_update_restarts_exactly_once_after_handoff() {
        let events = std::cell::RefCell::new(vec!["verified"]);
        let restarts = std::cell::Cell::new(0usize);

        handoff_verified_update(
            UpdateInstallPlatform::LinuxAppImage,
            VerifiedUpdateBytes(vec![1, 2, 3]),
            || {
                events.borrow_mut().push("teardown");
                Ok(())
            },
            |bytes| {
                assert!(!bytes.is_empty());
                events.borrow_mut().push("installer_handoff");
                Ok(())
            },
            || {
                restarts.set(restarts.get() + 1);
                events.borrow_mut().push("restart");
            },
        )
        .unwrap();

        assert_eq!(
            events.into_inner(),
            vec!["verified", "teardown", "installer_handoff", "restart"]
        );
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn update_shutdown_preserves_desired_recording_intent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, _runner, _probe) = lifecycle_state(&root, false);
        restore_desired_recording_locked(&state).unwrap();

        state.update_installing.store(true, Ordering::Release);
        begin_update_shutdown(&state).unwrap();

        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Quitting
        );
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );
        assert_eq!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .status()
                .unwrap()
                .state,
            RecordingState::Stopped,
        );
    }

    #[test]
    fn startup_hidden_marker_is_exact_and_distinguishes_manual_launch() {
        assert!(startup_hidden_args(["nian-vision.exe", STARTUP_HIDDEN_ARG]));
        assert!(!startup_hidden_args(["nian-vision.exe"]));
        assert!(!startup_hidden_args([
            "nian-vision.exe",
            "--startup-hidden-ish"
        ]));
    }

    #[test]
    fn manual_second_launch_during_startup_is_latched_until_state_is_ready() {
        PENDING_MANUAL_ACTIVATION.store(false, Ordering::Release);

        assert!(!manual_activation_should_run_now(false));
        assert!(PENDING_MANUAL_ACTIVATION.swap(false, Ordering::AcqRel));
        assert!(manual_activation_should_run_now(true));
        assert!(!PENDING_MANUAL_ACTIVATION.load(Ordering::Acquire));
    }

    #[test]
    fn power_event_source_seam_hands_events_to_the_desktop_dispatch_channel() {
        let source = FakePowerEventSource {
            event: nian_platform_windows::PowerEvent::Resume,
            subscriptions: AtomicUsize::new(0),
        };
        let (tx, rx) = std::sync::mpsc::channel();

        let _subscription = subscribe_power_source(&source, tx).unwrap();

        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            PowerDispatchMessage::Event(nian_platform_windows::PowerEvent::Resume)
        );
        assert_eq!(source.subscriptions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_power_unregistration_model_reclaims_callback_state() {
        let reclaimed = Arc::new(AtomicBool::new(false));
        let source = RetainingPowerEventSource {
            leak_on_drop: false,
            reclaimed: reclaimed.clone(),
        };
        let (tx, rx) = mpsc::channel();
        let subscription = subscribe_power_source(&source, tx).unwrap();

        drop(subscription);

        assert!(reclaimed.load(Ordering::Acquire));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn leaked_callback_sender_cannot_block_dispatcher_shutdown_or_final_exit() {
        let (control_tx, rx) = mpsc::channel();
        let reclaimed = Arc::new(AtomicBool::new(false));
        let source = RetainingPowerEventSource {
            leak_on_drop: true,
            reclaimed: reclaimed.clone(),
        };
        let subscription = subscribe_power_source(&source, control_tx.clone()).unwrap();
        drop(subscription);
        assert!(!reclaimed.load(Ordering::Acquire));
        let dispatcher = std::thread::spawn(move || {
            run_power_dispatch_loop(rx, |_| {});
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let power_events = events.clone();
        let exit_events = events.clone();

        run_shutdown_sequence(
            || {},
            || {},
            || {},
            || {},
            move || {
                control_tx.send(PowerDispatchMessage::Shutdown).unwrap();
                dispatcher.join().unwrap();
                power_events.lock().unwrap().push("power-joined");
            },
            move || exit_events.lock().unwrap().push("exit"),
        );

        assert_eq!(*events.lock().unwrap(), vec!["power-joined", "exit"]);
    }

    #[test]
    fn shutdown_sequence_is_deterministic_and_exit_is_last() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recording = events.clone();
        let playback = events.clone();
        let probe = events.clone();
        let tray = events.clone();
        let power = events.clone();
        let exit = events.clone();

        run_shutdown_sequence(
            move || recording.lock().unwrap().push("recording"),
            move || playback.lock().unwrap().push("playback"),
            move || probe.lock().unwrap().push("probe"),
            move || tray.lock().unwrap().push("tray"),
            move || power.lock().unwrap().push("power"),
            move || exit.lock().unwrap().push("exit"),
        );

        assert_eq!(
            *events.lock().unwrap(),
            vec!["recording", "playback", "probe", "tray", "power", "exit"]
        );
    }

    #[test]
    fn recording_status_observer_drives_tray_projection_without_frontend_polling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, _runner, _probe) = lifecycle_state(&root, true);
        let (tx, rx) = mpsc::channel();
        let projections = Arc::new(Mutex::new(Vec::new()));
        let captured = projections.clone();
        let watcher = std::thread::spawn(move || {
            run_tray_watch_loop(rx, |status| {
                captured.lock().unwrap().push(tray_projection(status, true));
            });
        });

        let mut controller = RecordingController::new(Arc::new(ScriptedTransitionRunner));
        let observer_tx = tx.clone();
        controller
            .set_status_observer(Arc::new(move |status| {
                let _ = observer_tx.send(TrayWatchMessage::Status(status));
            }))
            .unwrap();
        *state.recording_controller.lock().unwrap() = controller;

        start_recording(&state, "front-door").unwrap();
        for _ in 0..200 {
            let status = state.recording_controller.lock().unwrap().status().unwrap();
            if status.state == RecordingState::Failed {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        tx.send(TrayWatchMessage::Shutdown).unwrap();
        watcher.join().unwrap();

        let projections = projections.lock().unwrap();
        let labels = projections
            .iter()
            .map(|projection| projection.status_text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "Recording status: Stopped",
                "Recording status: Starting",
                "Recording status: Recovering",
                "Recording status: Connecting",
                "Recording status: Recording",
                "Recording status: Backoff",
                "Recording status: Failed",
            ]
        );
        assert!(projections.last().unwrap().stop_enabled);
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn tray_stop_clears_intent_before_publishing_stopping_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, _default_runner, _probe) = lifecycle_state(&root, true);
        repository.lock().unwrap().desired_camera = None;
        let runner = Arc::new(ParkedStoppingRunner::default());
        *state.recording_controller.lock().unwrap() = RecordingController::new(runner.clone());
        let (tx, rx) = mpsc::channel();
        *state.tray_watch_tx.lock().unwrap() = Some(tx.clone());
        let observer_tx = tx.clone();
        state
            .recording_controller
            .lock()
            .unwrap()
            .set_status_observer(Arc::new(move |status| {
                let _ = observer_tx.send(TrayWatchMessage::Status(status));
            }))
            .unwrap();

        start_recording(&state, "front-door").unwrap();
        for _ in 0..200 {
            if runner.starts.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        while rx.try_recv().is_ok() {}

        let stopped = stop_recording(&state).unwrap();
        assert_eq!(stopped.state, RecordingState::Stopping);
        assert!(repository.lock().unwrap().desired_camera.is_none());
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            TrayWatchMessage::RefreshIntent
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            TrayWatchMessage::Status(RecordingStatus {
                state: RecordingState::Stopping,
                ..
            })
        ));

        runner.release.store(true, Ordering::Release);
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn tray_stop_enablement_includes_persisted_intent_but_not_off_stopping() {
        assert!(!tray_projection(&RecordingStatus::default(), false).stop_enabled);
        let failed = RecordingStatus {
            state: RecordingState::Failed,
            ..RecordingStatus::default()
        };
        assert!(tray_projection(&failed, true).stop_enabled);
        let stopping = RecordingStatus {
            state: RecordingState::Stopping,
            ..RecordingStatus::default()
        };
        assert!(!tray_projection(&stopping, false).stop_enabled);
    }

    #[test]
    fn tray_watcher_shutdown_is_explicit_and_joined() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, _runner, _probe) = lifecycle_state(&root, true);
        let (tx, rx) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let thread_exited = exited.clone();
        let thread = std::thread::spawn(move || {
            run_tray_watch_loop(rx, |_| {});
            thread_exited.store(true, Ordering::Release);
        });
        *state.tray_watch_tx.lock().unwrap() = Some(tx);
        *state.tray_thread.lock().unwrap() = Some(thread);

        shutdown_tray_watcher(&state);

        assert!(exited.load(Ordering::Acquire));
        assert!(state.tray_thread.lock().unwrap().is_none());
    }

    #[test]
    fn desired_recording_restores_through_normal_controller_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, runner, _probe) = lifecycle_state(&root, true);

        let restored = restore_desired_recording_locked(&state).unwrap().unwrap();
        assert_eq!(restored.camera_id.as_deref(), Some("front-door"));
        wait_for_starts(&runner, 1);
        assert_eq!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .active_camera()
                .unwrap()
                .as_ref()
                .map(CameraId::as_str),
            Some("front-door")
        );
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );

        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn start_runtime_failure_keeps_persisted_desired_intent_on() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, runner, _probe) = lifecycle_state(&root, true);
        repository.lock().unwrap().desired_camera = None;
        *state.recording_controller.lock().unwrap() = RecordingController::with_spawner(
            runner.clone(),
            Arc::new(FailingRecordingThreadSpawner),
        );

        let error = start_recording(&state, "front-door").unwrap_err();

        assert_eq!(error.code, "worker_unavailable");
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
        let status = state.recording_controller.lock().unwrap().status().unwrap();
        assert_eq!(status.state, RecordingState::Failed);
    }

    #[test]
    fn rejected_second_camera_start_preserves_intent_until_prior_runner_is_reaped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, _default_runner, _probe) = lifecycle_state(&root, true);
        seed_second_recording_camera(&repository);
        repository.lock().unwrap().desired_camera = None;
        let runner = Arc::new(ParkedStoppingRunner::default());
        *state.recording_controller.lock().unwrap() = RecordingController::new(runner.clone());

        start_recording(&state, "front-door").unwrap();
        for _ in 0..200 {
            if runner.starts.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );

        let error = start_recording(&state, "back-door").unwrap_err();
        assert_eq!(error.code, "already_recording");
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );
        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);

        let stopped = stop_recording(&state).unwrap();
        assert_eq!(stopped.state, RecordingState::Stopping);
        assert!(repository.lock().unwrap().desired_camera.is_none());
        for _ in 0..200 {
            if runner.stop_seen.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(runner.stop_seen.load(Ordering::Acquire));

        let error = start_recording(&state, "back-door").unwrap_err();
        assert_eq!(error.code, "already_recording");
        assert!(repository.lock().unwrap().desired_camera.is_none());
        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);

        runner.release.store(true, Ordering::Release);
        for _ in 0..200 {
            if state
                .recording_controller
                .lock()
                .unwrap()
                .ensure_startable()
                .is_ok()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        start_recording(&state, "back-door").unwrap();
        for _ in 0..200 {
            if runner.starts.load(Ordering::SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(runner.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("back-door")
        );
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn multiple_persisted_desired_cameras_fail_closed_without_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, runner, _probe) = lifecycle_state(&root, true);
        repository.lock().unwrap().extra_desired_camera = Some("back-door".to_owned());

        let error = restore_desired_recording_locked(&state).unwrap_err();

        assert_eq!(error.code, "desired_state_failed");
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
        assert!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .active_camera()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restoration_failure_keeps_desired_on_and_surfaces_failed_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, _runner, _probe) = lifecycle_state(&root, true);
        repository.lock().unwrap().settings.storage_root = None;

        let error = restore_desired_recording_locked(&state).unwrap_err();

        assert_eq!(error.code, "storage_failed");
        let status = state.recording_controller.lock().unwrap().status().unwrap();
        assert_eq!(status.state, RecordingState::Failed);
        assert_eq!(status.camera_id.as_deref(), Some("front-door"));
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );
    }

    #[test]
    fn stop_turns_failed_desired_intent_off_without_not_recording_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, _runner, _probe) = lifecycle_state(&root, true);
        repository.lock().unwrap().settings.storage_root = None;
        restore_desired_recording_locked(&state).unwrap_err();

        let status = stop_recording(&state).unwrap();

        assert_eq!(status.state, RecordingState::Failed);
        assert!(repository.lock().unwrap().desired_camera.is_none());
        assert!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .active_camera()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stop_persistence_failure_leaves_live_runtime_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, runner, _probe) = lifecycle_state(&root, true);
        restore_desired_recording_locked(&state).unwrap();
        wait_for_starts(&runner, 1);
        repository.lock().unwrap().fail_desired_write = true;

        let error = stop_recording(&state).unwrap_err();

        assert_eq!(error.code, "desired_state_failed");
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );
        let mut controller = state.recording_controller.lock().unwrap();
        let status = controller.status().unwrap();
        assert_ne!(status.state, RecordingState::Stopping);
        assert_eq!(
            controller
                .active_camera()
                .unwrap()
                .as_ref()
                .map(CameraId::as_str),
            Some("front-door")
        );
        drop(controller);
        repository.lock().unwrap().fail_desired_write = false;
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn suspend_resume_restores_once_and_duplicate_resume_does_not_spawn_again() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, repository, runner, _probe) = lifecycle_state(&root, true);
        restore_desired_recording_locked(&state).unwrap();
        wait_for_starts(&runner, 1);

        handle_power_event(&state, nian_platform_windows::PowerEvent::Suspend).unwrap();
        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Suspending
        );
        handle_power_event(&state, nian_platform_windows::PowerEvent::Resume).unwrap();
        wait_for_starts(&runner, 2);
        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Running
        );
        handle_power_event(&state, nian_platform_windows::PowerEvent::Resume).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(runner.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            repository.lock().unwrap().desired_camera.as_deref(),
            Some("front-door")
        );

        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn startup_restore_refuses_to_start_after_lifecycle_is_suspending() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, runner, _probe) = lifecycle_state(&root, true);
        state.lifecycle.begin_suspend().unwrap();

        let error = restore_desired_recording_locked(&state).unwrap_err();

        assert_eq!(error.code, "suspending");
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
        state.lifecycle.begin_quit().unwrap();
        let error = restore_desired_recording_locked(&state).unwrap_err();
        assert_eq!(error.code, "quitting");
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_suspend_during_startup_is_processed_only_after_initial_restore() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, runner, _probe) = lifecycle_state(&root, true);
        let (tx, rx) = mpsc::channel();
        tx.send(PowerDispatchMessage::Event(
            nian_platform_windows::PowerEvent::Suspend,
        ))
        .unwrap();
        tx.send(PowerDispatchMessage::Shutdown).unwrap();

        {
            let _gate = state.control_gate.lock().unwrap();
            restore_desired_recording_locked(&state).unwrap();
            wait_for_starts(&runner, 1);
            assert_eq!(
                state.lifecycle.state().unwrap(),
                DesktopLifecycleState::Running
            );
        }

        run_power_dispatch_loop(rx, |event| handle_power_event(&state, event).unwrap());
        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Suspending
        );
        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn queued_suspend_resume_during_startup_converges_to_one_active_owner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, runner, _probe) = lifecycle_state(&root, true);
        let (tx, rx) = mpsc::channel();
        tx.send(PowerDispatchMessage::Event(
            nian_platform_windows::PowerEvent::Suspend,
        ))
        .unwrap();
        tx.send(PowerDispatchMessage::Event(
            nian_platform_windows::PowerEvent::Resume,
        ))
        .unwrap();
        tx.send(PowerDispatchMessage::Shutdown).unwrap();

        {
            let _gate = state.control_gate.lock().unwrap();
            restore_desired_recording_locked(&state).unwrap();
            wait_for_starts(&runner, 1);
        }
        run_power_dispatch_loop(rx, |event| handle_power_event(&state, event).unwrap());
        wait_for_starts(&runner, 2);

        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Running
        );
        assert_eq!(runner.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .active_camera()
                .unwrap(),
            Some(CameraId::parse("front-door").unwrap())
        );
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn resume_error_does_not_wedge_recording_playback_or_probe_admission() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, runner, probe) = lifecycle_state(&root, false);
        restore_desired_recording_locked(&state).unwrap();
        wait_for_starts(&runner, 1);

        handle_power_event(&state, nian_platform_windows::PowerEvent::Suspend).unwrap();
        let error =
            handle_power_event(&state, nian_platform_windows::PowerEvent::Resume).unwrap_err();

        assert_eq!(error.code, "internal");
        wait_for_starts(&runner, 2);
        assert_eq!(
            state.lifecycle.state().unwrap(),
            DesktopLifecycleState::Running
        );
        assert!(matches!(
            state
                .playback_controller
                .lock()
                .unwrap()
                .open("missing-recording"),
            Err(PlaybackError::Internal)
        ));
        state
            .probe_controller
            .probe(nian_application::PreparedProbe {
                camera_id: "front-door".to_owned(),
                source_json: serde_json::json!({"kind": "test"}),
                timeout_ms: 50,
            })
            .unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
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
    fn launch_only_update_succeeds_while_recording_and_leaves_runner_owned() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let (state, _repository, runner, _probe) = lifecycle_state(&root, true);
        restore_desired_recording_locked(&state).unwrap();
        wait_for_starts(&runner, 1);
        let autostart = FakeAutostart::default();

        {
            let mut service = state.camera_service.lock().unwrap();
            let mut playback = state.playback_controller.lock().unwrap();
            let mut next = service.application_settings().unwrap();
            next.launch_at_login = true;
            let saved =
                update_settings_transaction(&mut service, &mut playback, &autostart, next, true)
                    .unwrap();
            assert!(saved.launch_at_login);
        }

        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            state
                .recording_controller
                .lock()
                .unwrap()
                .active_camera()
                .unwrap(),
            Some(CameraId::parse("front-door").unwrap())
        );
        state
            .recording_controller
            .lock()
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn launch_only_update_keeps_active_playback_session_open_and_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let relative = "front-door/2026/08/29/08-30-00.mkv";
        let source = root.join(relative);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"source-media").unwrap();
        let (mut service, _state, _credentials) = test_service(&root);
        let mut playback = PlaybackController::with_backend(
            Arc::new(FakePlaybackBackend),
            temp.path().join("cache"),
        )
        .unwrap();
        playback
            .configure_storage(Some(root.clone()), RetentionPolicy::default(), None)
            .unwrap();
        let opened = playback.open(relative).unwrap();
        assert!(playback.session_active(&opened.session_id).unwrap());

        let previous = service.application_settings().unwrap();
        let mut next = previous.clone();
        next.launch_at_login = true;
        assert!(!recording_storage_settings_changed(&previous, &next));
        let autostart = FakeAutostart::default();

        let saved =
            update_settings_transaction(&mut service, &mut playback, &autostart, next, true)
                .unwrap();

        assert!(saved.launch_at_login);
        assert_eq!(playback.configured_storage_root(), Some(root.as_path()));
        assert!(playback.session_active(&opened.session_id).unwrap());
        playback.close(&opened.session_id).unwrap();
    }

    #[test]
    fn recording_critical_settings_change_remains_blocked_while_active() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, _state, _credentials) = test_service(&root_a);
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));
        let next = settings_for(&root_b);
        assert!(recording_storage_settings_changed(
            &service.application_settings().unwrap(),
            &next
        ));

        let error = update_settings_transaction(
            &mut service,
            &mut playback,
            &FakeAutostart::default(),
            next,
            true,
        )
        .unwrap_err();

        assert_eq!(error.code, "camera_busy");
        assert_eq!(playback.configured_storage_root(), Some(root_a.as_path()));
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

        let autostart = FakeAutostart::default();
        assert!(
            update_settings_transaction(
                &mut service,
                &mut playback,
                &autostart,
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

        let autostart = FakeAutostart::default();
        assert!(
            update_settings_transaction(
                &mut service,
                &mut playback,
                &autostart,
                settings_for(&root_b),
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
    fn startup_autostart_reconciliation_repairs_os_drift_without_rewriting_preference() {
        let autostart = FakeAutostart::default();
        *autostart.enabled.lock().unwrap() = true;

        reconcile_autostart(&autostart, false).unwrap();

        assert!(!*autostart.enabled.lock().unwrap());
        assert_eq!(*autostart.calls.lock().unwrap(), vec![false]);
    }

    #[test]
    fn autostart_os_failure_prevents_settings_commit_and_playback_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, _state, _credentials) = test_service(&root_a);
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));
        let mut next = settings_for(&root_b);
        next.launch_at_login = true;
        let autostart = FakeAutostart {
            fail_set: true,
            ..Default::default()
        };

        let error =
            update_settings_transaction(&mut service, &mut playback, &autostart, next, false)
                .unwrap_err();

        assert_eq!(error.code, "autostart_failed");
        assert!(!service.application_settings().unwrap().launch_at_login);
        assert_eq!(playback.configured_storage_root(), Some(root_a.as_path()));
        assert_eq!(*autostart.calls.lock().unwrap(), vec![true]);
    }

    #[test]
    fn settings_failure_rolls_back_autostart_registration() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, state, _credentials) = test_service(&root_a);
        state.lock().unwrap().fail_save = true;
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));
        let mut next = settings_for(&root_b);
        next.launch_at_login = true;
        let autostart = FakeAutostart::default();

        let error =
            update_settings_transaction(&mut service, &mut playback, &autostart, next, false)
                .unwrap_err();

        assert_eq!(error.code, "autostart_failed");
        assert!(!*autostart.enabled.lock().unwrap());
        assert_eq!(*autostart.calls.lock().unwrap(), vec![true, false]);
        assert!(!service.application_settings().unwrap().launch_at_login);
        assert_eq!(playback.configured_storage_root(), Some(root_a.as_path()));
    }

    #[test]
    fn autostart_rollback_failure_is_typed_and_does_not_claim_convergence() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("recordings-a");
        let root_b = temp.path().join("recordings-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let (mut service, state, _credentials) = test_service(&root_a);
        state.lock().unwrap().fail_save = true;
        let mut playback = configured_playback(&root_a, temp.path().join("cache"));
        let mut next = settings_for(&root_b);
        next.launch_at_login = true;
        let autostart = FakeAutostart {
            fail_on_call: Some(2),
            ..Default::default()
        };

        let error =
            update_settings_transaction(&mut service, &mut playback, &autostart, next, false)
                .unwrap_err();

        assert_eq!(error.code, "autostart_failed");
        assert!(error.message.contains("rollback also failed"));
        assert!(*autostart.enabled.lock().unwrap());
        assert_eq!(*autostart.calls.lock().unwrap(), vec![true, false]);
        assert!(!service.application_settings().unwrap().launch_at_login);
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
        state.lock().unwrap().cameras.push(
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

        let autostart = FakeAutostart::default();
        let saved = update_settings_transaction(
            &mut service,
            &mut playback,
            &autostart,
            settings_for(&root_b),
            false,
        )
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
