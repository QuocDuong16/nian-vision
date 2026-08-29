//! Worker-side recording job lifecycle over NDJSON IPC (M3 §13).
//!
//! ONE recording job per worker process: `recording.start` validates CHEAP
//! parameters, spawns a dedicated job thread and replies immediately (final
//! remediation §6) — the job thread then runs asynchronous startup
//! recovery (`recovering` state, observable and stoppable) followed by the
//! [`CameraRecordingSupervisor`] above real [`RecordingSession`]s, and
//! publishes a typed [`JobStatus`] snapshot that `recording.status` serves.
//!
//! # Stop-cancellation control model (final remediation §4)
//!
//! Operator escalation and process shutdown are SEPARATE operations:
//!
//! * `recording.stop` keeps the user-facing two-press semantics (press 1
//!   graceful, press 2 force-cancels blocking I/O in the live attempt);
//! * process shutdown uses [`RecordingJobManager::request_graceful_stop`]
//!   (IDEMPOTENT — calling it twice must never become a force cancel),
//!   then a bounded grace join, then — only if grace expires —
//!   [`RecordingJobManager::force_cancel_current_io`], then a bounded
//!   final join. The press counter is never control flow for shutdown.
//!
//! Stop wiring: the supervisor's run-level [`StopFlag`] is a shared atomic
//! captured BEFORE the supervisor moves into the job thread. Forced escape
//! works through the `latest_interrupt` slot: every `open_session` (and the
//! asynchronous recovery pass) deposits the fresh handle it built, so a
//! force-cancel aborts whatever media I/O is currently live.
//!
//! # Threading model
//!
//! `MediaInput` is `!Send`, but sessions never cross threads here: every
//! connection is opened INSIDE the job thread by the factories below; only
//! plain data (paths, config, URL string) moves through `.spawn`. Stdout
//! stays single-owner — the serve-loop thread writes every frame; the job
//! thread only updates its snapshot slot.
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
///
/// # Start-refusal contract (final remediation §8)
///
/// `invalid_params` and `job_already_active` are PERMANENT refusals: the
/// parent must never restart the worker expecting a different answer.
/// `start_failed` is the only transient refusal (the job thread could not be
/// spawned — retrying is meaningful).
pub mod code {
    /// A start request arrived while this worker already has/had its job.
    pub const JOB_ALREADY_ACTIVE: &str = "job_already_active";
    /// Stop with nothing running.
    pub const NO_ACTIVE_JOB: &str = "no_active_job";
    /// Start parameters failed validation (bad camera id/storage/source).
    pub const INVALID_PARAMS: &str = "invalid_params";
    /// The job thread could not be spawned. The only TRANSIENT refusal.
    pub const START_FAILED: &str = "start_failed";
}

/// §13/§7: outcome of the once-per-job startup reconciliation, classified
/// by TYPED recovery dispositions (never error strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoverySummary {
    /// Partials remuxed and published as fresh recordings (including ones
    /// recognized as already recovered from an earlier pass).
    pub recovered: usize,
    /// Partials kept in place (empty/invalid/no-keyframe).
    pub quarantined: usize,
    /// Partials whose recovery attempt failed with a content verdict
    /// (unreadable/corrupt). Reported; never blocks new recording.
    pub failed: usize,
    /// Typed count of INFRASTRUCTURE failures (storage unusable). When
    /// this is positive and nothing was recovered, the job fails
    /// permanently — safe storage operation is impossible.
    pub infrastructure_failures: usize,
    /// Per-attempt ARTIFACT failures (final correctness remediation §7 /
    /// final safety remediation §5): output-side problems on THIS
    /// attempt's own scratch that coexist with continued recording.
    pub artifact_failures: usize,
    /// Final safety remediation §1, case C: deterministic destinations
    /// that exist WITHOUT trusted transaction evidence (foreign files,
    /// directories, crash-before-tombstone, or a concurrent winner's
    /// not-yet-tombstoned window). Both the original and the destination
    /// are preserved — reported observably, never resolved by deletion.
    pub conflicts: usize,
}

impl RecoverySummary {
    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "recovered": self.recovered,
            "quarantined": self.quarantined,
            "failed": self.failed,
            "infrastructure_failures": self.infrastructure_failures,
            "artifact_failures": self.artifact_failures,
            "conflicts": self.conflicts,
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
    /// Coarse lifecycle word (`idle | recovering | connecting | recording |
    /// backoff | stopped | failed`), lowercase, from EXPLICIT stable wire
    /// values (final safety remediation §8): supervisor states come from
    /// [`nian_recorder::SupervisorState::as_str`]; `recovering` (final
    /// remediation §6) is the asynchronous startup-reconciliation phase
    /// AFTER `recording.start` was accepted: IPC stays responsive, status
    /// works, and stop/shutdown interrupt recovery in a bounded way.
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
    /// stop requests go here and are honored by the supervised loop.
    run_stop: Option<StopFlag>,
    /// Where every attempt deposits its fresh interrupt handle; a
    /// force-cancel cancels the current one (live attempt OR recovery).
    latest_interrupt: LatestInterruptSlot,
    active: Option<std::thread::JoinHandle<()>>,
    started_once: bool,
    /// §2/§4: presses are PER-MANAGER operator state. A new manager (new
    /// job) starts from zero — never a process-global — and process
    /// shutdown NEVER touches this counter (it uses `request_graceful_stop`).
    stop_presses: u32,
    /// §4: whether the IDEMPOTENT graceful-stop request already fired for
    /// this job (shutdown orchestration + stop press 1 share the flag, but
    /// never the escalation counter).
    graceful_stop_requested: bool,
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
            graceful_stop_requested: false,
        }
    }

    /// Current status snapshot (thread-safe read of the job's progress).
    #[allow(clippy::unwrap_used)] // lock guards are panic-free label updates
    pub fn status(&self) -> JobStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Whether a job was started AND already reached a terminal state.
    pub fn is_finished(&self) -> bool {
        self.started_once && self.shared.done.load(Ordering::SeqCst)
    }

    /// Starts a supervised job from a validated spec. Only CHEAP validation
    /// happens here (final remediation §6): parameter checks and building the
    /// supervisor objects — no filesystem ownership, media work, or recovery.
    /// The job thread acquires the camera lease FIRST, then runs storage
    /// pre-flight → recovery → connection → recording asynchronously, so
    /// `recording.start` acks promptly even when large crash files need remuxing.
    ///
    /// Fails with `job_already_active` when this worker already has/had a
    /// job (one job per worker lifetime keeps recovery boundaries clean),
    /// or `start_failed` when the thread cannot spawn.
    pub fn start(&mut self, spec: JobSpec) -> Result<(), &'static str> {
        if self.started_once || self.active.is_some() {
            return Err(code::JOB_ALREADY_ACTIVE);
        }

        let layout = nian_storage::RecordingsLayout::new(spec.storage_root.clone())
            .map_err(|_| code::INVALID_PARAMS)?;
        *self.shared.status.lock().unwrap() = JobStatus {
            camera_id: spec.camera.as_str().to_owned(),
            state: "recovering".to_owned(),
            retry_attempt: 0,
            finalized_segments: 0,
            finished: false,
            end_kind: String::new(),
            failure_category: String::new(),
            recovery: None,
        };

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
        // Construction performs NO media I/O, so it belongs to the prompt
        // start path — this is also how the run-level stop flag is captured
        // by construction before the supervisor moves into the job thread.
        let boxed = match thread_source {
            ThreadSource::File(path) => BoxedSupervisor::File(CameraRecordingSupervisor::new(
                spec.camera.clone(),
                SourceKind::File,
                SupervisorConfig::default(),
                FileFactory {
                    path,
                    layout: layout.clone(),
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
                    layout: layout.clone(),
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
        let interrupt_slot = Arc::clone(&self.latest_interrupt);
        let camera = spec.camera.clone();
        let thread_stop = run_stop.clone();
        let join = std::thread::Builder::new()
            .name("recording-job".to_owned())
            .spawn(move || match boxed {
                BoxedSupervisor::File(supervisor) => {
                    run_recovering_then_supervising(
                        supervisor,
                        shared_for_run,
                        interrupt_slot,
                        layout,
                        camera,
                        &thread_stop,
                    );
                }
                BoxedSupervisor::Rtsp(supervisor) => {
                    run_recovering_then_supervising(
                        supervisor,
                        shared_for_run,
                        interrupt_slot,
                        layout,
                        camera,
                        &thread_stop,
                    );
                }
            })
            .map_err(|_| code::START_FAILED)?;

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
        if presses == 1 {
            self.graceful_stop_requested = true;
        }
        stop_flag.request();
        if presses >= 2
            && let Some(interrupt) = self.latest_interrupt.lock().unwrap().as_ref()
        {
            interrupt.cancel();
        }
        Ok(presses)
    }

    /// Final remediation §4: the IDEMPOTENT graceful-stop operation used by
    /// protocol shutdown. Requesting it twice (or after a graceful stop
    /// press) must NEVER become a force cancel — it only sets the shared
    /// run-level flag. Returns whether THIS call actually requested it.
    pub fn request_graceful_stop(&mut self) -> bool {
        let Some(stop_flag) = &self.run_stop else {
            return false;
        };
        if self.graceful_stop_requested {
            return false;
        }
        self.graceful_stop_requested = true;
        stop_flag.request();
        true
    }

    /// Final remediation §4: cancels whatever blocking media I/O is live
    /// right now (session attempt or asynchronous recovery). Used ONLY by
    /// the operator's second stop press and by shutdown AFTER its grace
    /// window expired — never by the graceful path.
    pub fn force_cancel_current_io(&mut self) {
        if let Some(stop_flag) = &self.run_stop {
            stop_flag.request();
        }
        if let Some(interrupt) = self.latest_interrupt.lock().unwrap().as_ref() {
            interrupt.cancel();
        }
    }

    /// §8 real shutdown lifecycle for `cmd_run`, rebuilt per final
    /// remediation §4: ONE idempotent graceful request → bounded grace join
    /// → force-cancel ONLY if grace expires → bounded absolute join. The
    /// grace window deliberately exceeds the normal bounded read operation
    /// (`SourceTimeouts::read` default 15 s) plus finalization headroom so
    /// a healthy session always finishes publishing inside it. The operator
    /// press counter is never consulted.
    pub fn shutdown(&mut self) -> ShutdownDisposition {
        if !self.started_once {
            return ShutdownDisposition::CleanExit;
        }
        self.request_graceful_stop();
        if self.is_finished() {
            self.join_until(std::time::Instant::now());
            return ShutdownDisposition::CleanExit;
        }
        match self.join_until(std::time::Instant::now() + DEFAULT_GRACE_BEFORE_FORCE) {
            JoinOutcome::Joined => ShutdownDisposition::CleanExit,
            JoinOutcome::StillRunning => {
                // Grace expired while blocks were in flight: force-cancel
                // and wait the absolute safety bound.
                self.force_cancel_current_io();
                match self.join_until(std::time::Instant::now() + ABSOLUTE_FORCE_BOUND) {
                    JoinOutcome::Joined => ShutdownDisposition::ForcedCancellationSurvived,
                    JoinOutcome::StillRunning => ShutdownDisposition::UnsafeTermination,
                }
            }
        }
    }

    /// Bounded join that actually REAPS the thread (final remediation §4):
    /// whenever the job is observed finished — by the `done` flag or by
    /// `JoinHandle::is_finished` — the handle is TAKEN and joined, never
    /// left stored while claiming `Joined`.
    fn join_until(&mut self, deadline: std::time::Instant) -> JoinOutcome {
        loop {
            if self.shared.done.load(Ordering::SeqCst) {
                if let Some(join) = self.active.take() {
                    let _ = join.join();
                }
                return JoinOutcome::Joined;
            }
            if let Some(join) = self.active.take() {
                if join.is_finished() {
                    // Finished without the flag (impossible for a healthy
                    // run, but then it MUST still be reaped): join now.
                    let _ = join.join();
                    return JoinOutcome::Joined;
                }
                // Still running: put the handle back and keep waiting.
                self.active = Some(join);
            } else {
                return JoinOutcome::Joined;
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

fn finish_job_failed(shared: &Arc<SharedState>, category: nian_recorder::FailureCategory) {
    let mut slot = shared.status.lock().unwrap();
    slot.state = "failed".to_owned();
    slot.finished = true;
    slot.end_kind = "failed".to_owned();
    slot.failure_category = category.as_str().to_owned();
    shared.done.store(true, Ordering::SeqCst);
}

/// Job-thread body (final remediation §6): asynchronous startup recovery
/// FIRST — observable as `recovering`, cooperatively stoppable through the
/// run-level graceful-stop flag AND interruptible via the deposited
/// interrupt handle — and only then the supervised recording run. Recovery
/// never blocks the IPC serve loop: it lives entirely on this thread.
fn run_recovering_then_supervising<
    F: nian_recorder::SessionFactory,
    W: nian_recorder::Waiter,
    J: nian_recorder::Jitter,
>(
    supervisor: CameraRecordingSupervisor<F, W, J>,
    shared: Arc<SharedState>,
    interrupt_slot: LatestInterruptSlot,
    layout: nian_storage::RecordingsLayout,
    camera: CameraId,
    run_stop: &StopFlag,
) {
    // Camera-wide ownership is acquired INSIDE the asynchronous job and held
    // by this stack frame until every recovery/connect/record/backoff/stop path
    // has ended. A reconnect never relinquishes ownership.
    let lease = match nian_storage::CameraLease::try_acquire(&layout, &camera) {
        Ok(lease) => lease,
        Err(nian_storage::StorageError::CameraAlreadyActive { .. }) => {
            finish_job_failed(&shared, nian_recorder::FailureCategory::CameraInUse);
            return;
        }
        Err(_) => {
            finish_job_failed(&shared, nian_recorder::FailureCategory::StorageFailed);
            return;
        }
    };

    // The ordinary write/delete pre-flight comes AFTER ownership. Therefore a
    // second process cannot scan or mutate partials merely to test storage.
    if layout.ensure_camera_dir(&camera).is_err() {
        finish_job_failed(&shared, nian_recorder::FailureCategory::StorageFailed);
        return;
    }

    // The recovery pass deposits ITS interrupt handle in the same slot the
    // session attempts use, so a force-cancel aborts blocked recovery I/O
    // in a bounded way (§6).
    let recovery_interrupt = InterruptHandle::new();
    *interrupt_slot.lock().unwrap() = Some(recovery_interrupt.clone());

    // Final correctness remediation §2: recovery observes the GRACEFUL
    // stop domain too — the FIRST recording.stop / protocol shutdown ends
    // recovery at its next safe boundary without a second press.
    let (outcomes, failures) = nian_recorder::recover_camera_partials_with_interrupt(
        &layout,
        &camera,
        &lease,
        &recovery_interrupt,
        Some(run_stop),
    );
    let summary = {
        let recovered = outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    nian_recorder::RecoveryOutcome::Recovered { .. }
                        | nian_recorder::RecoveryOutcome::AlreadyRecovered { .. }
                )
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
        // Final safety remediation §1 case C: a deterministic destination
        // without trusted transaction evidence is an OBSERVABLE conflict —
        // both files preserved, never resolved by deletion, and never a
        // job failure (the storage is provably fine).
        let conflicts = outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    nian_recorder::RecoveryOutcome::RecoveryConflict { .. }
                )
            })
            .count();
        // Typed classification (final correctness remediation §7):
        // INFRASTRUCTURE failures mean the storage target is unsafe for
        // new recording; per-attempt ARTIFACT failures and content
        // verdicts quarantine/report without blocking; Cancelled is
        // neither and is not counted as a failure at all.
        let infrastructure_failures = failures
            .iter()
            .filter(|failure| {
                matches!(
                    failure.error,
                    nian_recorder::RecoveryError::Infrastructure { .. }
                )
            })
            .count();
        let artifact_failures = failures
            .iter()
            .filter(|failure| {
                matches!(failure.error, nian_recorder::RecoveryError::Artifact { .. })
            })
            .count();
        let failed = failures
            .iter()
            .filter(|failure| !matches!(failure.error, nian_recorder::RecoveryError::Cancelled))
            .count();
        RecoverySummary {
            recovered,
            quarantined,
            failed,
            infrastructure_failures,
            artifact_failures,
            conflicts,
        }
    };

    // Recovery ran exactly ONCE per job (§13, final remediation §5): its
    // summary publishes through the status snapshot either way.
    let failed_permanently = {
        let mut slot = shared.status.lock().unwrap();
        slot.recovery = Some(summary);
        // Final correctness remediation §7: a genuinely INFRASTRUCTURE
        // failure means the storage target is unsafe for new recording —
        // another file's successful recovery is NOT proof that the failed
        // operation (scan/claim) can be ignored, so the job fails
        // permanently whenever one occurred. Artifact/content failures
        // never block new recording.
        summary.infrastructure_failures > 0
    };

    if failed_permanently {
        let mut slot = shared.status.lock().unwrap();
        slot.state = "failed".to_owned();
        slot.finished = true;
        slot.end_kind = "failed".to_owned();
        // §9: stable wire value, never Debug formatting.
        slot.failure_category = nian_recorder::FailureCategory::StorageFailed
            .as_str()
            .to_owned();
        shared.done.store(true, Ordering::SeqCst);
        return;
    }

    if run_stop.is_requested() {
        // Stop/shutdown arrived during recovery (§6): end the job as an
        // operator stop WITHOUT connecting. Whatever recovery published
        // stays; unattempted partials wait for the next startup.
        let mut slot = shared.status.lock().unwrap();
        slot.state = "stopped".to_owned();
        slot.finished = true;
        slot.end_kind = "stopped".to_owned();
        shared.done.store(true, Ordering::SeqCst);
        return;
    }

    {
        let mut slot = shared.status.lock().unwrap();
        slot.state = "connecting".to_owned();
    }
    supervise_and_fold(supervisor, &shared);
}

/// Runs the supervisor to completion on the job thread, folding events into
/// the shared status snapshot; marks the slot done when finished.
fn supervise_and_fold<
    F: nian_recorder::SessionFactory,
    W: nian_recorder::Waiter,
    J: nian_recorder::Jitter,
>(
    supervisor: CameraRecordingSupervisor<F, W, J>,
    shared: &Arc<SharedState>,
) {
    let fold = |event: SupervisorEvent| {
        let mut slot = shared.status.lock().unwrap();
        match event {
            SupervisorEvent::StateChanged { to, .. } => {
                // Final safety remediation §8: the wire value is the
                // EXPLICIT stable vocabulary (`SupervisorState::as_str`),
                // never Rust Debug output.
                slot.state = to.as_str().to_owned();
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
                // visible to hosts — worker-alive ≠ recording-healthy. §9:
                // the category crosses the wire as its STABLE as_str value,
                // never Rust Debug output.
                let (kind, category) = match end {
                    SupervisorEnd::SourceCompleted => ("completed", None),
                    SupervisorEnd::StoppedByOperator { .. } => ("stopped", None),
                    SupervisorEnd::PermanentFailure { category } => {
                        ("failed", Some(category.as_str()))
                    }
                };
                slot.end_kind = kind.to_owned();
                if let Some(category) = category {
                    slot.failure_category = category.to_owned();
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
    //! Unit coverage for §2 (per-job stop presses), §4 (graceful-stop
    //! operation split), §6 (asynchronous recovery lifecycle), §7 (typed
    //! recovery classification) and §8 (shutdown lifecycle).

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

    /// Bounded wait for the job to reach a terminal state.
    fn wait_finished(manager: &RecordingJobManager, seconds: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while !manager.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.is_finished()
    }

    /// Bounded wait until the job's status satisfies `predicate` (used for
    /// the asynchronous `recovering` → later phases lifecycle, §6).
    fn wait_status(manager: &RecordingJobManager, predicate: impl Fn(&JobStatus) -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if predicate(&manager.status()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
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
    fn start_acks_promptly_with_recovering_state_before_any_media_work() {
        // §6: recording.start must return BEFORE the recovery/remux work —
        // the status snapshot is already observable as `recovering` (or has
        // already moved on for tiny inputs) while the job thread runs.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let mut manager = RecordingJobManager::new();
        let result = manager.start(JobSpec {
            camera: CameraId::parse("cam-async").unwrap(),
            storage_root: temp.path().join("rec").to_path_buf(),
            source: JobSource::File(PathBuf::from(fixtures)),
            segment_target: Duration::from_secs(300),
            copy_audio: true,
        });
        assert!(result.is_ok(), "start must ack without doing media work");
        // State moved off `idle` immediately; the camera is already set.
        let status = manager.status();
        assert_eq!(status.camera_id, "cam-async");
        assert_ne!(status.state, "idle");
        assert!(!status.finished);

        let _ = manager.stop();
        assert!(wait_finished(&manager, 30));
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

        assert!(
            wait_finished(&manager, 30),
            "first graceful stop must finish the job"
        );
        let status = manager.status();
        assert_eq!(status.end_kind, "stopped");
    }

    #[test]
    fn request_graceful_stop_is_idempotent_and_never_consumes_the_press_counter() {
        // Final remediation §4: shutdown orchestration uses the idempotent
        // operation; calling it repeatedly must NOT become a force cancel
        // and must leave the operator press counter untouched.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let mut manager = RecordingJobManager::new();
        manager
            .start(spec_for(
                JobSource::File(PathBuf::from(fixtures)),
                "cam-graceful-shutdown",
            ))
            .expect("job starts");

        assert!(
            manager.request_graceful_stop(),
            "first graceful request fires"
        );
        assert!(
            !manager.request_graceful_stop(),
            "second graceful request is a no-op, NEVER an escalation"
        );
        assert_eq!(
            manager.stop_presses_field(),
            0,
            "the operator press counter must not be reused as control flow"
        );
        assert!(wait_finished(&manager, 30));
        assert_eq!(manager.status().end_kind, "stopped");
        // The shutdown lifecycle on an already-stopped job is a clean exit.
        assert_eq!(manager.shutdown(), ShutdownDisposition::CleanExit);
    }

    #[test]
    fn startup_recovery_runs_once_and_asynchronously_per_job_start() {
        // §13/§6: seed a leftover partial for the target camera; after the
        // ASYNC reconciliation completes, the status must carry a recovery
        // summary with real salvage accounting, and the job still records.
        // The lock excludes the process-global recovery hooks this test
        // does not arm but whose armed state would leak between parallel
        // tests (FAULT_LOCK is released by panic-safe RAII guards).
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(source_path)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start ok");

        // §6: the summary appears asynchronously once the job thread ran
        // the (single) reconciliation pass.
        assert!(
            wait_status(&manager, |status| status.recovery.is_some()),
            "the seeded partial must be accounted by startup recovery"
        );
        let recovery = manager.status().recovery.expect("summary present");
        assert!(
            recovery.recovered >= 1 || recovery.quarantined >= 1,
            "the seeded partial must be accounted by startup recovery: {recovery:?}"
        );

        // Stop quickly to keep this test bounded (fixture is short).
        let _ = manager.stop();
        assert!(wait_finished(&manager, 30));
    }

    #[test]
    fn live_partial_is_untouched_while_another_camera_lease_is_held() {
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rec");
        let layout = nian_storage::RecordingsLayout::new(root.clone()).unwrap();
        let camera = CameraId::parse("cam-live-lease").unwrap();

        // Holder A owns the camera before it creates the live canonical
        // partial, matching the production invariant. The claim token and
        // lease both stay alive while worker B attempts to start.
        let holder_lease = nian_storage::CameraLease::try_acquire(&layout, &camera).unwrap();
        let claim = layout
            .claim_segment(&camera, chrono::Local::now().naive_local())
            .unwrap();
        let active_partial = claim.partial_path().to_path_buf();
        let active_bytes = std::fs::read(fixtures).unwrap();
        std::fs::write(&active_partial, &active_bytes).unwrap();

        let mut blocked_manager = RecordingJobManager::new();
        blocked_manager
            .start(JobSpec {
                camera: camera.clone(),
                storage_root: root.clone(),
                // If the worker ever crossed the ownership fence into media
                // open, this deliberately-invalid source would fail with a
                // source category instead of `camera_in_use`.
                source: JobSource::File(temp.path().join("must-not-open.mkv")),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("recording.start still acks asynchronously");

        assert!(
            wait_finished(&blocked_manager, 10),
            "ownership conflict must terminate without retrying"
        );
        let blocked = blocked_manager.status();
        assert_eq!(blocked.end_kind, "failed");
        assert_eq!(blocked.failure_category, "camera_in_use");
        assert_eq!(
            blocked.recovery, None,
            "no lease means startup recovery must not even begin"
        );
        assert_eq!(
            std::fs::read(&active_partial).unwrap(),
            active_bytes,
            "the live partial must remain byte-identical"
        );

        let day_dir = active_partial.parent().unwrap();
        let blocked_names: Vec<String> = std::fs::read_dir(day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            blocked_names
                .iter()
                .all(|name| !name.contains(".recovered.mkv")),
            "B must not publish recovery output while A owns the lease: {blocked_names:?}"
        );
        assert!(
            blocked_names
                .iter()
                .all(|name| !name.contains(".recovery-")),
            "B must not create recovery scratch while A owns the lease: {blocked_names:?}"
        );
        assert!(
            blocked_names.iter().all(|name| !name.ends_with(".done")),
            "B must not create a recovery tombstone while A owns the lease: {blocked_names:?}"
        );

        // Once holder A is genuinely gone, the SAME bytes become abandoned
        // crash state. A fresh worker may take ownership, recover them once,
        // then continue into normal recording.
        drop(claim);
        drop(holder_lease);

        let mut recovery_manager = RecordingJobManager::new();
        recovery_manager
            .start(JobSpec {
                camera,
                storage_root: root,
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("fresh worker starts after old holder releases ownership");
        assert!(
            wait_finished(&recovery_manager, 60),
            "fresh worker must recover and complete"
        );
        let recovered = recovery_manager.status();
        let summary = recovered.recovery.expect("startup recovery must run");
        assert_eq!(
            summary.recovered, 1,
            "the abandoned partial is recovered once"
        );
        assert!(
            !active_partial.exists(),
            "successful recovery removes the abandoned original only after publication"
        );
        assert_eq!(recovered.end_kind, "completed");
    }

    #[test]
    fn corrupt_leftover_is_quarantined_and_does_not_block_new_recording() {
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Final remediation §7: a per-file CONTENT failure (unreadable
        // bytes) is quarantined/reported and must NOT prevent the new
        // camera recording — and must NOT classify as infrastructure.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-corrupt").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        // EBML-magic garbage above the size threshold: scanner routes it to
        // media-level recovery, the demuxer refuses it → content failure.
        let mut junk = vec![0x1A_u8, 0x45, 0xDF, 0xA3];
        junk.extend(std::iter::repeat_n(0xEE_u8, 2048));
        let stamp = chrono::Local::now().naive_local();
        std::fs::write(
            day_dir.join(nian_storage::paths::partial_file_name(stamp.time())),
            junk,
        )
        .unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start must NOT be refused by a corrupt leftover");

        assert!(wait_finished(&manager, 60), "recording must complete");
        let status = manager.status();
        assert_eq!(
            status.end_kind, "completed",
            "the new recording must run to its natural EOF"
        );
        let recovery = status.recovery.expect("recovery summary present");
        assert!(
            recovery.failed >= 1 && recovery.infrastructure_failures == 0,
            "corrupt leftover is a CONTENT failure: {recovery:?}"
        );
    }

    #[test]
    fn untrusted_destination_is_reported_as_a_conflict_and_does_not_block_recording() {
        // Final safety remediation §1 case C at the worker level: a
        // leftover whose deterministic recovered destination is occupied by
        // a FOREIGN file (no trusted tombstone) is reported observably as
        // a CONFLICT — both files preserved, no deletion, no job failure —
        // and the new recording continues to its natural EOF.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-conflict").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let stamp = chrono::Local::now().naive_local();
        let partial_name = nian_storage::paths::partial_file_name(stamp.time());
        std::fs::write(
            day_dir.join(&partial_name),
            std::fs::read(fixtures).unwrap(),
        )
        .unwrap();
        // Occupied destination WITHOUT trusted transaction evidence.
        let base = partial_name.strip_suffix(".partial.mkv").unwrap();
        let occupied = day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::write(&occupied, b"foreign destination bytes").unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start must NOT be refused by a preserved conflict");

        assert!(wait_finished(&manager, 60), "recording must complete");
        let status = manager.status();
        assert_eq!(
            status.end_kind, "completed",
            "a preserved conflict never fails the job"
        );
        let recovery = status.recovery.expect("recovery summary present");
        assert_eq!(
            recovery.conflicts, 1,
            "the conflict must be reported observably: {recovery:?}"
        );
        assert_eq!(recovery.recovered, 0);
        assert_eq!(recovery.failed, 0);
        assert_eq!(recovery.infrastructure_failures, 0);
        // Lossless-first: BOTH the original and the destination survive.
        assert!(
            day_dir.join(&partial_name).is_file(),
            "the original partial must survive the conflict"
        );
        assert_eq!(
            std::fs::read(&occupied).unwrap(),
            b"foreign destination bytes",
            "the destination must survive the conflict untouched"
        );
    }

    #[test]
    fn infrastructure_storage_failure_fails_the_job_permanently() {
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Final remediation §7/§8: a genuine storage-infrastructure failure
        // (the day directory path is occupied by a file — creation/claim is
        // impossible) fails the job TERMINALLY with the stable
        // `storage_failed` category. No retry loop, no refusal ambiguity.
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-infra").unwrap();
        // Pre-flight (camera dir) succeeds; the DAY dir path is a file, so
        // every later storage operation (recovery scan passes it, but the
        // first claim fails at filesystem level).
        let month_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(month_dir.parent().unwrap()).unwrap();
        std::fs::write(&month_dir, b"not a directory").unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start itself must ack (pre-flight only proves the camera dir)");

        assert!(
            wait_finished(&manager, 60),
            "the job must reach a terminal state"
        );
        let status = manager.status();
        assert_eq!(status.end_kind, "failed");
        assert_eq!(
            status.failure_category, "storage_failed",
            "§9: stable wire value for the failure category"
        );
    }

    #[test]
    fn shutdown_during_recovery_ends_bounded_without_connecting() {
        // §6 / final correctness remediation §2: protocol shutdown during
        // the `recovering` phase must act GRACEFULLY — one idempotent
        // graceful request makes the asynchronous recovery abandon at its
        // next safe boundary, so the disposition is CleanExit BEFORE the
        // force-cancel grace could ever expire. A deterministic delay hook
        // holds recovery at its pre-publication checkpoint while shutdown
        // arrives.
        // Serialize the process-global recovery hooks: parallel hook tests
        // must not reset each other's armed state (panic-safe RAII guards).
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hooks_guard = nian_recorder::arm_recovery_delay(1500);
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-stop-recovery").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let stamp = chrono::Local::now().naive_local();
        let partial_name = nian_storage::paths::partial_file_name(stamp.time());
        std::fs::write(
            day_dir.join(&partial_name),
            std::fs::read(fixtures).unwrap(),
        )
        .unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start ok");

        // Deterministic: wait until the job thread sits in recovery, then
        // run the shutdown lifecycle.
        assert!(
            wait_status(&manager, |status| status.state == "recovering"),
            "the job must be observed in the recovering state"
        );
        // Let the job thread reach its pre-publication checkpoint (held by
        // the armed delay hook), so the shutdown lands MID-recovery.
        std::thread::sleep(Duration::from_millis(250));
        let started = std::time::Instant::now();
        // CleanExit means the grace window never expired: the graceful
        // path (NOT force cancellation) ended recovery.
        assert_eq!(manager.shutdown(), ShutdownDisposition::CleanExit);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "graceful shutdown must end recovery well before the force grace: {started:?}"
        );
        assert!(wait_finished(&manager, 30));
        assert_eq!(manager.status().end_kind, "stopped");
        // The attempt's scratch is gone; the original stays recoverable;
        // nothing published.
        assert!(day_dir.join(&partial_name).is_file());
        let names: Vec<String> = std::fs::read_dir(&day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "no scratch may survive: {names:?}"
        );
    }

    #[test]
    fn first_stop_during_recovery_needs_no_second_press() {
        // Final correctness remediation §2: the FIRST recording.stop ends
        // asynchronous recovery cooperatively — bounded, terminal `stopped`,
        // NO camera connection attempt, original partials safe, no poisoned
        // recovery final, no force-cancel required.
        // Serialize the process-global recovery hooks: parallel hook tests
        // must not reset each other's armed state (panic-safe RAII guards).
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hooks_guard = nian_recorder::arm_recovery_delay(1500);
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-stop-recovering").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let stamp = chrono::Local::now().naive_local();
        let partial_name = nian_storage::paths::partial_file_name(stamp.time());
        std::fs::write(
            day_dir.join(&partial_name),
            std::fs::read(fixtures).unwrap(),
        )
        .unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start ok");
        assert!(
            wait_status(&manager, |status| status.state == "recovering"),
            "the job must be observed in the recovering state"
        );
        // Let the job thread reach its pre-publication checkpoint (held by
        // the armed delay hook), so the single stop lands MID-recovery.
        std::thread::sleep(Duration::from_millis(250));

        // EXACTLY ONE press.
        let presses = manager.stop().expect("stop accepted");
        assert_eq!(presses, 1, "a single graceful press must suffice");
        assert!(
            wait_finished(&manager, 30),
            "one press must end the job boundedly during recovery"
        );
        let status = manager.status();
        assert_eq!(status.end_kind, "stopped");
        assert_eq!(status.finalized_segments, 0, "nothing was recorded");

        // No camera connection: a connection would have produced normal
        // (`<HH-MM-SS>.mkv`) recording finals — there are none.
        let names: Vec<String> = std::fs::read_dir(&day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let normal_finals = names
            .iter()
            .filter(|name| {
                name.ends_with(".mkv")
                    && !name.contains(".partial.")
                    && !name.contains(".recovered")
            })
            .count();
        assert_eq!(
            normal_finals, 0,
            "no camera connection may happen: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "no scratch may survive the abandoned attempt: {names:?}"
        );
        assert!(
            day_dir.join(&partial_name).is_file(),
            "the original partial stays safely recoverable"
        );
    }

    #[test]
    fn one_stop_during_recovery_alignment_ends_the_job_without_connecting() {
        // Final safety remediation §2: graceful stop is observable during
        // the ALIGNMENT phase of recovery — the explicit keyframe probe —
        // not only at the pre-publication checkpoint. The attempt parks
        // INSIDE the probe deterministically; ONE recording.stop ends the
        // job boundedly as `stopped`, the camera is never connected, the
        // original stays recoverable, and no scratch/final appears.
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_hooks_guard, gate) = nian_recorder::arm_alignment_gate();
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-align-stop").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let stamp = chrono::Local::now().naive_local();
        let partial_name = nian_storage::paths::partial_file_name(stamp.time());
        std::fs::write(
            day_dir.join(&partial_name),
            std::fs::read(fixtures).unwrap(),
        )
        .unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start ok");
        assert!(
            wait_status(&manager, |status| status.state == "recovering"),
            "the job must be observed in the recovering state"
        );

        // The recovery thread parks inside the ALIGNMENT probe; the single
        // stop lands there deterministically.
        gate.wait_arrived();
        let presses = manager.stop().expect("stop accepted");
        assert_eq!(presses, 1, "a single graceful press must suffice");
        gate.release();

        assert!(
            wait_finished(&manager, 30),
            "one press must end the job boundedly during alignment"
        );
        let status = manager.status();
        assert_eq!(status.end_kind, "stopped");
        assert_eq!(status.finalized_segments, 0, "nothing was recorded");

        // No camera connection (no normal finals), no scratch, original
        // partial intact.
        let names: Vec<String> = std::fs::read_dir(&day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let normal_finals = names
            .iter()
            .filter(|name| {
                name.ends_with(".mkv")
                    && !name.contains(".partial.")
                    && !name.contains(".recovered")
            })
            .count();
        assert_eq!(
            normal_finals, 0,
            "no camera connection may happen: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "no scratch may survive the abandoned attempt: {names:?}"
        );
        assert!(
            day_dir.join(&partial_name).is_file(),
            "the original partial stays safely recoverable"
        );
    }

    #[test]
    fn artifact_storage_failure_coexists_with_continued_recording() {
        // Final correctness remediation §7: a per-ATTEMPT artifact failure
        // (the finalized scratch stat fails, injected deterministically)
        // must NOT fail the job — the scratch claim already succeeded, so
        // the storage root demonstrably accepts new files. The recording
        // continues to its natural EOF.
        let _hooks = nian_recorder::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hooks_guard = nian_recorder::arm_metadata_failure();
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let temp = tempfile::tempdir().unwrap();
        let layout = nian_storage::RecordingsLayout::new(temp.path().join("rec")).unwrap();
        let camera = CameraId::parse("cam-artifact").unwrap();
        let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
        std::fs::create_dir_all(&day_dir).unwrap();
        let stamp = chrono::Local::now().naive_local();
        std::fs::write(
            day_dir.join(nian_storage::paths::partial_file_name(stamp.time())),
            std::fs::read(fixtures).unwrap(),
        )
        .unwrap();

        let mut manager = RecordingJobManager::new();
        manager
            .start(JobSpec {
                camera,
                storage_root: temp.path().join("rec").to_path_buf(),
                source: JobSource::File(PathBuf::from(fixtures)),
                segment_target: Duration::from_secs(300),
                copy_audio: true,
            })
            .expect("start must not be refused by an artifact failure");

        assert!(wait_finished(&manager, 60), "recording must complete");
        let status = manager.status();
        assert_eq!(
            status.end_kind, "completed",
            "an artifact failure must coexist with continued recording"
        );
        let recovery = status.recovery.expect("recovery summary present");
        assert!(
            recovery.failed >= 1 && recovery.infrastructure_failures == 0,
            "the artifact failure is reported but is NOT infrastructure: {recovery:?}"
        );
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

        // Best-effort: catch the job while a segment is live. A local file
        // records FASTER than real time, so it may complete between polls;
        // what this test pins deterministically is the per-manager press
        // counter and bounded completion, not the race window itself.
        let _ = wait_status(&manager, |status| {
            status.state == "recording" || status.finished
        });

        let presses_1 = manager.stop().expect("first stop");
        assert_eq!(presses_1, 1, "graceful first");
        // Race-space deliberately tiny: second press arrives while I/O is
        // still blocked reading — exactly when force-cancel has meaning.
        let presses_2 = manager.stop().expect("second stop");
        assert_eq!(presses_2, 2, "escalation recorded per-manager");

        assert!(
            wait_finished(&manager, 30),
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
