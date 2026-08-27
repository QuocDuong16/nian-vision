//! Worker-side recording job lifecycle over NDJSON IPC (M3 §13).
//!
//! ONE recording job per worker process: `recording.start` claims the
//! single job slot, spawns a dedicated thread running the
//! [`CameraRecordingSupervisor`] above real [`RecordingSession`]s, and
//! publishes a typed [`JobStatus`] snapshot that `recording.status` serves.
//! `recording.stop` requests graceful shutdown of the whole supervised run;
//! pressing it again escalates to forced cancellation of blocking I/O in
//! whichever session attempt is live.
//!
//! # Threading model
//!
//! `MediaInput` is `!Send`, but sessions never cross threads here: every
//! connection is opened INSIDE the job thread by the factories below; only
//! plain data (paths, config, URL string) moves through `.spawn`. Stdout
//! stays single-owner — the serve-loop thread writes every frame; the job
//! thread only updates its snapshot slot.
//!
//! Stop wiring: the supervisor's run-level [`StopFlag`] is a shared atomic;
//! the manager captures a clone BEFORE the supervisor moves into the job
//! thread and requests stops through it. Forced-escape works through the
//! `latest_interrupt` slot: every `open_session` deposits the fresh handle
//! it built, and a second stop press cancels whatever handle is current,
//! aborting a blocked read/write exactly like the manual CLI's Ctrl+C
//! escalation (M2 review §9 semantics, hosted form).
//!
//! # Secrets contract (master spec §4/§7)
//!
//! The RTSP URL arrives inside the request payload over the private stdin
//! channel — never argv. It lives in [`SecretUrl`], whose Debug/Display
//! output is `<redacted>`; nothing in this module logs or echoes source
//! strings, and supervisor event payloads cannot carry them by construction.

// Mutex poisoning cannot wedge a worker whose lock guards are plain JSON
// snapshots: every guard closure below is panic-free, so lock().unwrap() is
// an invariant, not a guess. Tests are absent here (logic lives in the
// recorder/ipc crates), hence the unconditional scoped allow.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nian_domain::CameraId;
use nian_media_ffmpeg::InterruptHandle;
use nian_recorder::{
    CameraRecordingSupervisor, RecorderConfig, RecordingError, RecordingSession, SeededJitter,
    SleepWaiter, SourceKind, StopFlag, SupervisorConfig, SupervisorEvent,
};

/// Stable IPC error codes for the recording namespace.
pub mod code {
    /// A start request arrived while this worker already has/had its job.
    pub const JOB_ALREADY_ACTIVE: &str = "job_already_active";
    /// Stop with nothing running.
    pub const NO_ACTIVE_JOB: &str = "no_active_job";
    /// Start parameters failed validation (bad camera id/storage/source).
    pub const INVALID_PARAMS: &str = "invalid_params";
}

/// Default served when the request omits `segment_target_secs` (5 minutes).
pub const DEFAULT_SEGMENT_TARGET_SECS: u64 = 300;

/// Newtype around a credential-bearing URL refusing to leak through
/// formatting traits.
#[derive(Clone)]
pub struct SecretUrl(String);

impl SecretUrl {
    /// Wraps the raw URL string.
    pub fn new(raw: String) -> Self {
        Self(raw)
    }

    /// The single accessor handing out the raw value, for opening media.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretUrl(<redacted>)")
    }
}

impl std::fmt::Display for SecretUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where the supervised recording reads from — parsed ONCE from the request
/// and kept secret-safe (`Rtsp`'s Debug shows no URL).
#[derive(Clone)]
pub enum JobSource {
    /// Local file path (test/manual sources).
    File(PathBuf),
    /// Live network stream.
    Rtsp(SecretUrl),
}

impl std::fmt::Debug for JobSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            // Deliberately opaque: paths are safe to print, URLs are not.
            Self::Rtsp(url) => f.debug_tuple("Rtsp").field(&url.expose().len()).finish(),
        }
    }
}

/// Everything `recording.start` needs for one supervised camera.
#[derive(Debug, Clone)]
pub struct JobSpec {
    /// Camera identity (drives the recordings directory layout).
    pub camera: CameraId,
    /// Storage root.
    pub storage_root: PathBuf,
    /// Where the bytes come from (secret-safe Debug).
    pub source: JobSource,
    /// Segment target duration.
    pub segment_target: Duration,
    /// Whether audio streams are copied alongside video.
    pub copy_audio: bool,
}

impl JobSpec {
    /// Parses the wire payload into a validated spec. Accepted shape:
    ///
    /// `{ "camera": "...", "storage": "...",
    ///    "source": { "kind": "file", "path": "..." }
    ///              | { "kind": "rtsp", "url": "..." },
    ///    "segment_target_secs": 300, "copy_audio": true }`
    ///
    /// Validation failures map to stable error code `invalid_params`.
    pub fn from_params(params: &serde_json::Value) -> Result<Self, &'static str> {
        let camera_text = params
            .get("camera")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing 'camera'")?;
        let camera = CameraId::parse(camera_text).map_err(|_| "invalid 'camera' id")?;

        let storage = params
            .get("storage")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing 'storage'")?;
        if nian_storage::RecordingsLayout::new(storage).is_err() {
            return Err("invalid 'storage' root");
        }

        let source_value = params.get("source").ok_or("missing 'source'")?;
        let kind = source_value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing 'source.kind'")?;
        let source = match kind {
            "file" => {
                let path = source_value
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing 'source.path'")?;
                JobSource::File(PathBuf::from(path))
            }
            "rtsp" => {
                let url = source_value
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing 'source.url'")?;
                JobSource::Rtsp(SecretUrl::new(url.to_owned()))
            }
            _other => return Err("unknown 'source.kind' (file|rtsp)"),
        };

        let segment_target_secs = params
            .get("segment_target_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_SEGMENT_TARGET_SECS);
        if segment_target_secs == 0 {
            return Err("'segment_target_secs' must be greater than zero");
        }

        let copy_audio = params
            .get("copy_audio")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        Ok(Self {
            camera,
            storage_root: PathBuf::from(storage),
            source,
            segment_target: Duration::from_secs(segment_target_secs),
            copy_audio,
        })
    }
}

/// Live observability snapshot of the supervised job (served by
/// `recording.status`). Secret-free by construction: only labels, counters
/// and the camera id appear.
#[derive(Debug, Clone)]
pub struct JobStatus {
    /// Camera under supervision (empty when none yet).
    pub camera_id: String,
    /// Coarse lifecycle word (`connecting | backoff | recording | stopping |
    /// stopped | failed | idle`), lowercase.
    pub state: String,
    /// Consecutive retry attempt currently armed (0 once connected).
    pub retry_attempt: u32,
    /// Total segments published across all attempts so far.
    pub finalized_segments: usize,
    /// Whether the job reached a terminal state.
    pub finished: bool,
}

impl JobStatus {
    fn initial() -> Self {
        Self {
            camera_id: String::new(),
            state: "idle".to_owned(),
            retry_attempt: 0,
            finalized_segments: 0,
            finished: false,
        }
    }

    /// Renders into the JSON shape served over IPC.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "camera_id": self.camera_id,
            "state": self.state,
            "retry_attempt": self.retry_attempt,
            "finalized_segments": self.finalized_segments,
            "finished": self.finished,
        })
    }
}

/// State shared between the manager (serve-loop thread) and the job thread.
struct SharedState {
    status: Mutex<JobStatus>,
    /// Set by the job thread right before it exits.
    done: AtomicBool,
}

/// Deposits each connection attempt's fresh interrupt handle so the manager
/// can force-cancel whatever attempt is live (second stop press).
type LatestInterruptSlot = Arc<Mutex<Option<InterruptHandle>>>;

/// Opens one fresh local-file session per attempt (fresh input + interrupt
/// handle on EVERY call — M3 §3 reconnect rule, structurally enforced by
/// consuming the previous session).
struct FileFactory {
    path: PathBuf,
    layout: nian_storage::RecordingsLayout,
    config: RecorderConfig,
    latest_interrupt: LatestInterruptSlot,
}

impl nian_recorder::SessionFactory for FileFactory {
    fn open_session(
        &mut self,
    ) -> Result<Box<dyn nian_recorder::supervisor::ActiveSession>, RecordingError> {
        let interrupt = InterruptHandle::new();
        *self.latest_interrupt.lock().unwrap() = Some(interrupt.clone());
        let input = nian_media_ffmpeg::MediaInput::open(
            &nian_media::MediaSource::File(self.path.clone()),
            &interrupt,
        )?;
        Ok(Box::new(SessionShell(RecordingSession::from_input(
            input,
            self.layout.clone(),
            self.config.clone(),
        )?)))
    }
}

/// Opens one fresh RTSP session per attempt; the URL string exists only
/// inside the job thread after spawn.
struct RtspFactory {
    url: String,
    layout: nian_storage::RecordingsLayout,
    config: RecorderConfig,
    latest_interrupt: LatestInterruptSlot,
}

impl nian_recorder::SessionFactory for RtspFactory {
    fn open_session(
        &mut self,
    ) -> Result<Box<dyn nian_recorder::supervisor::ActiveSession>, RecordingError> {
        let interrupt = InterruptHandle::new();
        *self.latest_interrupt.lock().unwrap() = Some(interrupt.clone());
        let input = nian_media_ffmpeg::MediaInput::open(
            &nian_media::MediaSource::Rtsp {
                url: nian_media::RtspUrl::new(self.url.clone()),
            },
            &interrupt,
        )?;
        Ok(Box::new(SessionShell(RecordingSession::from_input(
            input,
            self.layout.clone(),
            self.config.clone(),
        )?)))
    }
}

/// Bridges a concrete [`RecordingSession`] to the supervisor's object seam.
struct SessionShell(RecordingSession);

impl nian_recorder::supervisor::ActiveSession for SessionShell {
    fn run(
        self: Box<Self>,
        events: &mut dyn FnMut(nian_recorder::RecordingEvent),
    ) -> Result<nian_recorder::RecordingSummary, RecordingError> {
        self.0.run(events)
    }

    fn stop_flag(&self) -> StopFlag {
        self.0.stop_flag()
    }
}

enum BoxedSupervisor {
    File(CameraRecordingSupervisor<FileFactory, SleepWaiter, SeededJitter>),
    Rtsp(CameraRecordingSupervisor<RtspFactory, SleepWaiter, SeededJitter>),
}

/// The single-job lifecycle container. Not `Clone`: one job per worker.
pub struct RecordingJobManager {
    shared: Arc<SharedState>,
    /// Clone of the supervisor's run-level flag captured at `start()` time;
    /// `stop()` requests go here and are honored by the supervised loop.
    run_stop: Option<StopFlag>,
    /// Where every attempt deposits its fresh interrupt handle; the second
    /// stop press cancels the current one.
    latest_interrupt: LatestInterruptSlot,
    active: Option<std::thread::JoinHandle<()>>,
    started_once: bool,
}

impl Default for RecordingJobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingJobManager {
    /// Creates an idle manager.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(SharedState {
                status: Mutex::new(JobStatus::initial()),
                done: AtomicBool::new(false),
            }),
            run_stop: None,
            latest_interrupt: Arc::new(Mutex::new(None)),
            active: None,
            started_once: false,
        }
    }

    /// Current status snapshot (thread-safe read of the job's progress).
    pub fn status(&self) -> JobStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Whether a job was started AND already reached a terminal state.
    pub fn is_finished(&self) -> bool {
        self.started_once && self.shared.done.load(Ordering::SeqCst)
    }

    /// Starts a supervised job from a validated spec. Fails with
    /// `job_already_active` when this worker already has/had a job (one job
    /// per worker lifetime keeps recovery boundaries clean).
    pub fn start(&mut self, spec: JobSpec) -> Result<(), &'static str> {
        if self.started_once || self.active.is_some() {
            return Err(code::JOB_ALREADY_ACTIVE);
        }

        *self.shared.status.lock().unwrap() = JobStatus {
            camera_id: spec.camera.as_str().to_owned(),
            state: "connecting".to_owned(),
            retry_attempt: 0,
            finalized_segments: 0,
            finished: false,
        };

        let layout = nian_storage::RecordingsLayout::new(spec.storage_root.clone())
            .map_err(|_| code::INVALID_PARAMS)?;
        let mut config =
            RecorderConfig::new(spec.camera.clone()).with_segment_target(spec.segment_target);
        if !spec.copy_audio {
            config = config.with_audio(nian_recorder::AudioPolicy::Exclude);
        }

        enum ThreadSource {
            File(PathBuf),
            Rtsp(String),
        }
        let thread_source = match &spec.source {
            JobSource::File(path) => ThreadSource::File(path.clone()),
            JobSource::Rtsp(url) => ThreadSource::Rtsp(url.expose().to_owned()),
        };

        // Only plain data crosses `.spawn`; MediaInput is !Send, which is why
        // connections are constructed per-attempt inside the factories.
        let boxed = match thread_source {
            ThreadSource::File(path) => BoxedSupervisor::File(CameraRecordingSupervisor::new(
                spec.camera.clone(),
                SourceKind::File,
                SupervisorConfig::default(),
                FileFactory {
                    path,
                    layout,
                    config,
                    latest_interrupt: Arc::clone(&self.latest_interrupt),
                },
                SleepWaiter,
                SeededJitter::new(job_seed()),
            )),
            ThreadSource::Rtsp(url) => BoxedSupervisor::Rtsp(CameraRecordingSupervisor::new(
                spec.camera.clone(),
                SourceKind::Rtsp,
                SupervisorConfig::default(),
                RtspFactory {
                    url,
                    layout,
                    config,
                    latest_interrupt: Arc::clone(&self.latest_interrupt),
                },
                SleepWaiter,
                SeededJitter::new(job_seed()),
            )),
        };

        // Capture the run-level stop flag BEFORE the supervisor moves into
        // the job thread; both clones share one Arc'd atomic.
        let run_stop = match &boxed {
            BoxedSupervisor::File(supervisor) => supervisor.stop_flag(),
            BoxedSupervisor::Rtsp(supervisor) => supervisor.stop_flag(),
        };

        let shared_for_run = Arc::clone(&self.shared);
        let join = std::thread::Builder::new()
            .name("recording-job".to_owned())
            .spawn(move || match boxed {
                BoxedSupervisor::File(supervisor) => {
                    supervise_and_fold(supervisor, shared_for_run);
                }
                BoxedSupervisor::Rtsp(supervisor) => {
                    supervise_and_fold(supervisor, shared_for_run);
                }
            })
            .map_err(|_| "thread spawn failed")?;

        self.run_stop = Some(run_stop);
        self.active = Some(join);
        self.started_once = true;
        Ok(())
    }

    /// Requests a stop of the running job, M2-review-style escalation:
    ///
    /// * press 1: graceful — the supervised loop finalizes/publishes the
    ///   active segment, no reconnect happens afterwards;
    /// * press 2+: force-cancel the LIVE attempt's blocking I/O through its
    ///   deposited interrupt handle (the active partial follows M2 abandon
    ///   semantics); the run then ends as operator stop.
    ///
    /// Returns presses so far, or an error code when nothing runs.
    pub fn stop(&mut self) -> Result<u32, &'static str> {
        let Some(stop_flag) = &self.run_stop else {
            return Err(code::NO_ACTIVE_JOB);
        };
        stop_flag.request();
        *shared_stop_presses().lock().unwrap() += 1;
        let presses = *shared_stop_presses().lock().unwrap();
        if presses >= 2
            && let Some(interrupt) = self.latest_interrupt.lock().unwrap().as_ref()
        {
            interrupt.cancel();
        }
        Ok(presses)
    }
}

fn shared_stop_presses() -> &'static Mutex<u32> {
    static PRESSES: std::sync::OnceLock<Mutex<u32>> = std::sync::OnceLock::new();
    PRESSES.get_or_init(|| Mutex::new(0))
}

fn job_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 ^ u64::from(std::process::id()) << 32)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// Runs the supervisor to completion on the job thread, folding events into
/// the shared status snapshot; marks the slot done when finished.
fn supervise_and_fold<
    F: nian_recorder::SessionFactory,
    W: nian_recorder::Waiter,
    J: nian_recorder::Jitter,
>(
    supervisor: CameraRecordingSupervisor<F, W, J>,
    shared: Arc<SharedState>,
) {
    let fold = |event: SupervisorEvent| {
        let mut slot = shared.status.lock().unwrap();
        match event {
            SupervisorEvent::StateChanged { to, .. } => {
                slot.state = format!("{to:?}").to_lowercase();
            }
            SupervisorEvent::ReconnectScheduled { retry_attempt, .. } => {
                slot.state = "backoff".to_owned();
                slot.retry_attempt = retry_attempt;
            }
            SupervisorEvent::ConnectionEstablished { .. } => {
                slot.state = "recording".to_owned();
                slot.retry_attempt = 0;
            }
            // Segment totals arrive authoritatively via Finished; per-attempt
            // progress increments below keep the counter live meanwhile.
            SupervisorEvent::SessionEnded { outcome, .. } => match outcome {
                nian_recorder::AttemptOutcome::GracefulStop { finalized_segments }
                | nian_recorder::AttemptOutcome::Eof {
                    finalized_segments, ..
                } => {
                    slot.finalized_segments += finalized_segments;
                }
                nian_recorder::AttemptOutcome::Failure { .. } => {}
            },
            SupervisorEvent::Finished {
                finalized_segments, ..
            } => {
                slot.finalized_segments = finalized_segments;
                slot.finished = true;
            }
        }
    };

    let _outcome = supervisor.run_until_end(&mut |event| fold(event));
    shared.done.store(true, Ordering::SeqCst);
}
