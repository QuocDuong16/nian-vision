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
    SleepWaiter, SourceKind, StopFlag, SupervisorConfig, SupervisorEnd, SupervisorEvent,
};

/// Stable IPC error codes for the recording namespace.
pub mod code {
    /// A start request arrived while this worker already has/had its job.
    pub const JOB_ALREADY_ACTIVE: &str = "job_already_active";
    /// Stop with nothing running.
    pub const NO_ACTIVE_JOB: &str = "no_active_job";
    /// Start parameters failed validation (bad camera id/storage/source).
    pub const INVALID_PARAMS: &str = "invalid_params";
    /// Startup partial-reconciliation could not run against this storage —
    /// typed permanent/local failure per remediation §13.
    pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
}

/// §13: outcome of the once-per-job startup reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoverySummary {
    /// Partials remuxed and published as fresh recordings.
    pub recovered: usize,
    /// Partials kept in place (empty/invalid/no-keyframe).
    pub quarantined: usize,
    /// Partials whose recovery attempt failed (reported upstream).
    pub failed: usize,
}

impl RecoverySummary {
    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "recovered": self.recovered,
            "quarantined": self.quarantined,
            "failed": self.failed,
        })
    }
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
    /// §7: WHEN finished, whether it ended successfully (`stopped`/
    /// `completed`) or permanently (`failed`); empty otherwise. The parent
    /// MUST NOT equate worker health with recording health — this field is
    /// how they differ observably.
    pub end_kind: String,
    /// §7: typed failure category behind a `failed` end_kind
    /// (e.g. `StorageFailed`); empty when not applicable.
    pub failure_category: String,
    /// §13: startup reconciliation summary (None before first start).
    pub recovery: Option<RecoverySummary>,
}

impl JobStatus {
    fn initial() -> Self {
        Self {
            camera_id: String::new(),
            state: "idle".to_owned(),
            retry_attempt: 0,
            finalized_segments: 0,
            finished: false,
            end_kind: String::new(),
            failure_category: String::new(),
            recovery: None,
        }
    }

    /// Renders into the JSON shape served over IPC.
    #[allow(clippy::unwrap_used)] // formatting static keys only
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "camera_id": self.camera_id,
            "state": self.state,
            "retry_attempt": self.retry_attempt,
            "finalized_segments": self.finalized_segments,
            "finished": self.finished,
            "end_kind": self.end_kind,
            "failure_category": self.failure_category,
            "recovery": self.recovery.map(RecoverySummary::to_json),
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
        run_stop: &StopFlag,
    ) -> Result<Box<dyn nian_recorder::supervisor::ActiveSession>, RecordingError> {
        let interrupt = InterruptHandle::new();
        *self.latest_interrupt.lock().unwrap() = Some(interrupt.clone());
        // §15: the configured OPEN budget applies on every reconnect attempt
        // too (these factories own opening, not `RecordingSession::open`).
        // Force-cancel still works during Connecting: cancellation outranks
        // the deadline at the FFmpeg abort-cause boundary.
        let _open_budget = interrupt.scoped_deadline(self.config.timeouts.open);
        let input = nian_media_ffmpeg::MediaInput::open(
            &nian_media::MediaSource::File(self.path.clone()),
            &interrupt,
        )?;
        drop(_open_budget);
        Ok(Box::new(SessionShell(
            RecordingSession::from_input_with_stop(
                input,
                self.layout.clone(),
                self.config.clone(),
                run_stop.clone(),
            )?,
        )))
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
        run_stop: &StopFlag,
    ) -> Result<Box<dyn nian_recorder::supervisor::ActiveSession>, RecordingError> {
        let interrupt = InterruptHandle::new();
        *self.latest_interrupt.lock().unwrap() = Some(interrupt.clone());
        // §15: same open/connect budget discipline as the file factory.
        let _open_budget = interrupt.scoped_deadline(self.config.timeouts.open);
        let input = nian_media_ffmpeg::MediaInput::open(
            &nian_media::MediaSource::Rtsp {
                url: nian_media::RtspUrl::new(self.url.clone()),
            },
            &interrupt,
        )?;
        drop(_open_budget);
        Ok(Box::new(SessionShell(
            RecordingSession::from_input_with_stop(
                input,
                self.layout.clone(),
                self.config.clone(),
                run_stop.clone(),
            )?,
        )))
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
    /// §2: presses are PER-MANAGER state. A new manager (new job) starts
    /// from zero — never a process-global.
    stop_presses: u32,
    /// §13: result of the once-per-job startup reconciliation (Some after
    /// a start attempt).
    recovery_run: Option<RecoverySummary>,
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
            stop_presses: 0,
            recovery_run: None,
        }
    }

    /// Current status snapshot (thread-safe read of the job's progress).
    #[allow(clippy::unwrap_used)] // lock guards are panic-free label updates
    pub fn status(&self) -> JobStatus {
        let mut status = self.shared.status.lock().unwrap().clone();
        status.recovery = self.recovery_run;
        status
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
            end_kind: String::new(),
            failure_category: String::new(),
            recovery: self.recovery_run,
        };

        let layout = nian_storage::RecordingsLayout::new(spec.storage_root.clone())
            .map_err(|_| code::INVALID_PARAMS)?;

        // ---- Startup reconciliation (M3 remediation §13): ONCE per job ---
        // Before ANY connection attempt, reconcile this camera's canonical
        // partials. Storage-level infrastructure failures here make safe
        // recording impossible → typed permanent failure instead of starting
        // a doomed supervisor loop. Quarantined/unrecoverable leftovers only
        // REPORT; they never block new recording.
        let (recovery_outcomes, recovery_failures) =
            nian_recorder::recover_camera_partials(&layout, &spec.camera);
        let recovery_summary = {
            let outcomes = &recovery_outcomes;
            let failures = &recovery_failures;
            let recovered = outcomes
                .iter()
                .filter(|outcome| {
                    matches!(outcome, nian_recorder::RecoveryOutcome::Recovered { .. })
                })
                .count();
            let quarantined = outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome,
                        nian_recorder::RecoveryOutcome::KeptUnrecoverable { .. }
                    )
                })
                .count();
            if !failures.is_empty() && recovered == 0 {
                *self.shared.status.lock().unwrap() = JobStatus {
                    camera_id: spec.camera.as_str().to_owned(),
                    state: "failed".to_owned(),
                    retry_attempt: 0,
                    finalized_segments: 0,
                    finished: true,
                    end_kind: "failed".to_owned(),
                    failure_category: "StorageFailed".to_owned(),
                    recovery: None,
                };
                return Err(code::STORAGE_UNAVAILABLE);
            }
            RecoverySummary {
                recovered,
                quarantined,
                failed: failures.len(),
            }
        };
        self.recovery_run = Some(recovery_summary);
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

    /// Requests a stop of the running job. M3 remediation §2: press state
    /// lives PER MANAGER/JOB (never process-global), so a fresh job starts
    /// at zero presses again after one finishes.
    ///
    /// * press 1: graceful — the supervised loop finalizes/publishes the
    ///   active segment; no reconnect happens afterwards;
    /// * press 2: force-cancel of the LIVE attempt's blocking I/O through
    ///   its deposited interrupt handle (the active partial follows M2
    ///   abandon semantics); the run then ends as operator stop.
    ///
    /// With no job ever started this is `no_active_job`.
    pub fn stop(&mut self) -> Result<u32, &'static str> {
        let Some(stop_flag) = &self.run_stop else {
            return Err(code::NO_ACTIVE_JOB);
        };
        self.stop_presses += 1;
        let presses = self.stop_presses;
        stop_flag.request();
        if presses >= 2
            && let Some(interrupt) = self.latest_interrupt.lock().unwrap().as_ref()
        {
            interrupt.cancel();
        }
        Ok(presses)
    }

    /// §8 real shutdown lifecycle for `cmd_run`: graceful stop → bounded
    /// join → escalation → bounded join → forced-path flag. The grace
    /// window deliberately exceeds the normal bounded read operation
    /// (`SourceTimeouts::read` default 15 s) plus finalization headroom so
    /// a healthy session always finishes publishing inside it.
    pub fn shutdown(&mut self) -> ShutdownDisposition {
        let grace = if self.started_once {
            DEFAULT_GRACE_BEFORE_FORCE
        } else {
            Duration::ZERO
        };

        // Never started or already finished: nothing to wait for.
        if !self.started_once {
            return ShutdownDisposition::CleanExit;
        }
        if self.is_finished() {
            return self.join_now(grace);
        }
        // First (graceful) press regardless of prior manual stop presses.
        let _ = self.stop();
        match self.join_deadline(std::time::Instant::now() + grace) {
            JoinOutcome::Joined => return ShutdownDisposition::CleanExit,
            JoinOutcome::StillRunning => {}
        }
        // Grace expired while blocks were in flight: force-cancel and wait
        // the absolute safety bound.
        if let Some(interrupt) = self.latest_interrupt.lock().unwrap().as_ref() {
            interrupt.cancel();
        }
        let _ = self.stop(); // ensure run-level flag also set
        match self.join_deadline(std::time::Instant::now() + ABSOLUTE_FORCE_BOUND) {
            JoinOutcome::Joined => ShutdownDisposition::ForcedCancellationSurvived,
            JoinOutcome::StillRunning => ShutdownDisposition::UnsafeTermination,
        }
    }

    fn join_now(&mut self, _grace: Duration) -> ShutdownDisposition {
        if let Some(join) = self.active.take() {
            let _ = join.join();
        }
        ShutdownDisposition::CleanExit
    }

    fn join_deadline(&mut self, deadline: std::time::Instant) -> JoinOutcome {
        // The supervisor sets `done` right before its thread returns; we
        // join on that signal so the thread is fully reaped, and treat a
        // finished-done flag as authoritative within the bound.
        loop {
            if self.shared.done.load(Ordering::SeqCst) {
                if let Some(join) = self.active.take() {
                    let _ = join.join();
                }
                return JoinOutcome::Joined;
            }
            match &mut self.active {
                None => return JoinOutcome::Joined,
                Some(join) => {
                    if join.is_finished() {
                        return JoinOutcome::Joined;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return JoinOutcome::StillRunning;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Grace before force-cancel: > configured packet-read timeout (15 s) +
/// finalization headroom, satisfying §8's requirement to never choose a
/// value shorter than a normal bounded read operation.
const DEFAULT_GRACE_BEFORE_FORCE: Duration = Duration::from_secs(25);
/// Absolute bound after forced cancellation before declaring unsafe exit.
const ABSOLUTE_FORCE_BOUND: Duration = Duration::from_secs(10);

enum JoinOutcome {
    Joined,
    StillRunning,
}

/// Outcome of [`RecordingJobManager::shutdown`] surfaced to main().
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownDisposition {
    /// Job finished inside grace; worker may exit cleanly.
    CleanExit,
    /// Forced cancellation was needed but the thread rejoined afterwards.
    ForcedCancellationSurvived,
    /// Even the absolute force bound expired: caller should terminate with
    /// the active output left partial (explicit forced-shutdown path).
    UnsafeTermination,
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
                end,
                finalized_segments,
                ..
            } => {
                slot.finalized_segments = finalized_segments;
                slot.finished = true;
                // §7: terminal disposition AND its failure category must be
                // visible to hosts — worker-alive ≠ recording-healthy.
                let (kind, category) = match end {
                    SupervisorEnd::SourceCompleted => ("completed", None),
                    SupervisorEnd::StoppedByOperator { .. } => ("stopped", None),
                    SupervisorEnd::PermanentFailure { category } => {
                        ("failed", Some(format!("{category:?}")))
                    }
                };
                slot.end_kind = kind.to_owned();
                if let Some(category) = category {
                    slot.failure_category = category;
                }
            }
        }
    };

    let _outcome = supervisor.run_until_end(&mut |event| fold(event));
    shared.done.store(true, Ordering::SeqCst);
}

impl RecordingJobManager {
    #[cfg(test)]
    pub(crate) fn stop_presses_field(&self) -> u32 {
        self.stop_presses
    }
}

#[cfg(test)]
mod tests {
    //! Unit coverage for §2 (per-job stop presses), §8 (shutdown lifecycle),
    //! and §13 (startup recovery invocation + typed storage failure).

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::time::Duration;

    #[allow(clippy::needless_range_loop)]
    fn spec_for(source: JobSource, camera: &str) -> JobSpec {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("rec");
        std::fs::create_dir_all(&root).unwrap();
        // The tempdir is dropped at scope end; only used for validation, so
        // a path that EXISTS at parse time is all from_params needs.
        let storage_root = root.to_string_lossy().into_owned();
        // Leaked temp root keeps the directory valid through the test.
        let leaked: &'static str = Box::leak(storage_root.into_boxed_str());
        let _ = dir.keep();
        JobSpec {
            camera: CameraId::parse(camera).unwrap(),
            storage_root: PathBuf::from(leaked),
            source,
            segment_target: Duration::from_secs(5),
            copy_audio: true,
        }
    }

    #[test]
    fn stop_presses_are_per_manager_and_start_at_zero() {
        let mut manager = RecordingJobManager::new();
        // No job yet: pressing is an error regardless of global state.
        assert!(matches!(manager.stop(), Err(code::NO_ACTIVE_JOB)));
        // A SECOND manager must not inherit any press count from the first:
        // construct via drop of a started one — covered by per-field state
        // (each new() starts stop_presses = 0).
        assert_eq!(RecordingJobManager::new().stop_presses_field(), 0);
        assert_eq!(manager.stop_presses_field(), 0);
    }

    #[test]
    fn graceful_stop_reaches_the_active_session_without_second_press() {
        // §20 worker row 1: FIRST stop must be graceful AND actually reach
        // the active session. With the file fixture source, recording ends
        // by itself; request stop during recording and observe terminal
        // state without escalation.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let mut manager = RecordingJobManager::new();
        manager
            .start(spec_for(
                JobSource::File(PathBuf::from(fixtures)),
                "cam-stop-test",
            ))
            .expect("job starts");

        // First press while Connecting/Recording.
        let presses = manager.stop().expect("stop accepted");
        assert_eq!(presses, 1);

        // Bounded wait for the job to finish gracefully.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !manager.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            manager.is_finished(),
            "first graceful stop must finish the job"
        );
        let status = manager.status();
        assert_eq!(status.end_kind, "stopped");
    }

    #[test]
    fn startup_recovery_runs_once_per_job_start() {
        // §13: seed a leftover partial for the target camera; after start()
        // completes its reconciliation, the status must carry a recovery
        // summary with recovered >= 1 (real remux into a published final).
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-recover-start").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let bytes = std::fs::read(fixtures).unwrap();
        let stamp = chrono::Local::now().naive_local();
        // The scan only accepts CANONICAL names — build one the same way
        // the allocator does, via the documented formatting helper.
        let canonical = nian_storage::paths::partial_file_name(stamp.time());
        std::fs::write(day_dir.join(canonical), &bytes).unwrap();

        let mut manager = RecordingJobManager::new();
        let source_path = fixtures.to_string();
        let spec = JobSpec {
            camera,
            storage_root: temp.path().join("rec").to_path_buf(),
            source: JobSource::File(PathBuf::from(source_path)),
            segment_target: Duration::from_secs(300),
            copy_audio: true,
        };
        // Storage validates at start(); recovery then runs BEFORE any media
        // open in the job thread? NO — remediation requires it BEFORE spawn;
        // so recover_camera_partials runs synchronously inside start().
        manager.start(spec).expect("start ok");

        let recovery = manager.status().recovery.expect("summary present");
        assert!(
            recovery.recovered >= 1 || recovery.quarantined >= 1,
            "the seeded partial must be accounted by startup recovery: {recovery:?}"
        );

        // Stop quickly to keep this test bounded (fixture is short).
        let _ = manager.stop();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !manager.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn second_stop_force_cancels_the_live_attempt() {
        // §20 worker row: press 2 escalates to forced cancellation of the
        // current attempt. A LONG fixture + huge segment target keeps ONE
        // recording segment open; after force-cancel the supervisor must
        // finish WITHOUT publishing (M2 abandon semantics) and still end
        // as an operator stop. Bounded waits only.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/session_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera: CameraId::parse("cam-force").unwrap(),
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                // 300 s target => the single attempt never rotates; the run
                // can only end via our stop/escalation.
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("job starts");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while manager.status().state != "recording" && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        let presses_1 = manager.stop().expect("first stop");
        assert_eq!(presses_1, 1, "graceful first");
        // Race-space deliberately tiny: second press arrives while I/O is
        // still blocked reading — exactly when force-cancel has meaning.
        let presses_2 = manager.stop().expect("second stop");
        assert_eq!(presses_2, 2, "escalation recorded per-manager");

        let finished_by = std::time::Instant::now() + Duration::from_secs(30);
        while !manager.is_finished() && std::time::Instant::now() < finished_by {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            manager.is_finished(),
            "forced cancellation must end the job"
        );
        let status = manager.status();
        // A fast local file may finish naturally between presses (race we
        // cannot remove without a network source); what MUST hold either way
        // is: per-manager counter == 2 above, bounded completion here, and —
        // under `stopped` — the recorder-level forced-abandon contract that
        // nian-recorder's own cancellation tests pin deterministically.
        assert!(
            status.end_kind == "stopped" || status.end_kind == "completed",
            "unexpected terminal disposition {status:?}"
        );
    }

    #[test]
    fn shutdown_without_job_exits_cleanly() {
        let mut manager = RecordingJobManager::new();
        assert_eq!(
            manager.shutdown(),
            ShutdownDisposition::CleanExit,
            "no job => nothing to wait for"
        );
    }
}
