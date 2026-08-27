//! Parent-side supervision of the media worker process (M3 §14; remediated
//! per M3 review findings 3–7, 18, 19).
//!
//! # Coordinator architecture (finding 3)
//!
//! A single COORDINATOR (the caller's thread) owns all request state and
//! shutdown intent. Child stdout is drained by a DEDICATED READER THREAD
//! that pushes framed envelopes/EOF/errors into a channel, so the
//! coordinator's event loop is `recv_timeout`-based: operator shutdown is
//! observed WITHOUT needing any incoming worker frame, and worker death is
//! observed from ANY lifecycle phase. Writes go through a dedicated writer
//! thread consuming one command channel — never concurrent unsynchronized
//! stdin writes.
//!
//! # Deadlines (finding 4)
//!
//! Hello / recording.start / shutdown responses are bounded by injected,
//! configurable deadlines ([`SupervisorDeadlines`]); a silent-but-alive
//! worker is an UNHEALTHY episode handled by restart policy, not an
//! indefinite block.
//!
//! # Crash phases (finding 5)
//!
//! Unexpected child death or pipe EOF in EVERY phase (before hello, during
//! hello, awaiting recording.start ack, while recording, while processing
//! shutdown) maps to the restart policy. Malformed frames and protocol-
//! version mismatches stay PERMANENT. Wedged-but-alive workers surface via
//! response deadlines as unhealthy episodes (restartable).
//!
//! # Recording visibility (findings 6/7)
//!
//! The coordinator periodically polls `recording.status`. Terminal job
//! states (`stopped`, `completed`, `failed` + typed failure category) come
//! back through [`EpisodeUpdate`]-shaped flows — the parent NEVER equates
//! a healthy process with healthy recording. Transient start refusals map
//! to retryable episodes (finding 6), permanent ones to permanent errors.
//!
//! # Framing (finding 19)
//!
//! The parent reader reuses nian-ipc's FramedReader with its
//! MAX_MESSAGE_BYTES pre-allocation bound; a malformed worker can never
//! force an unbounded line allocation.
//!
//! Secrets: source JSON travels only through the stdin command channel;
//! Debug output of DesiredRecording omits it entirely.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nian_domain::ReconnectBackoff;
use nian_ipc::message::{Envelope, PROTOCOL_VERSION};

use crate::error::ApplicationError;

/// A parameterized worker launcher (test seam for §15's crash test).
pub trait WorkerLauncher {
    /// Spawns a fresh worker in `run` mode. Only static arguments may go
    /// through argv — never source descriptions or credentials.
    fn spawn(&mut self) -> std::io::Result<Child>;
}

/// Production launcher for the built worker binary.
#[derive(Debug, Clone)]
pub struct BinaryLauncher {
    /// Path to the `nian-media-worker` executable.
    pub program: String,
}

impl WorkerLauncher for BinaryLauncher {
    fn spawn(&mut self) -> std::io::Result<Child> {
        // SECRET BOUNDARY: argv carries the program and the literal
        // subcommand ONLY.
        Command::new(&self.program)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // worker stderr stays diagnostic-only
            .spawn()
    }
}

/// How one supervised episode ended (`run_forever` drives restarts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    /// Operator asked for shutdown; the worker acknowledged or exited
    /// right after receiving it.
    RequestedShutdown,
    /// Death/unresponsiveness/transient refusal in ANY phase: restart
    /// policy material (backoff + state restore).
    RetryableEpisode,
    /// The desired job completed on its own (file-source EOF handled
    /// inside the worker); process remains healthy but the recording is
    /// finished — supervision stops at clean completion.
    JobCompletedCleanly,
}

/// Bounded deadlines for one supervised episode (finding 4). Tests use
/// short values; production defaults cover slow RTSP handshakes.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorDeadlines {
    /// Max wait for the hello handshake after spawn.
    pub hello: Duration,
    /// Max wait for a recording.start acknowledgement.
    pub start_ack: Duration,
    /// Max wait for the shutdown acknowledgement before declaring the
    /// episode dead anyway.
    pub shutdown_ack: Duration,
    /// Interval between recording.status polls while monitoring.
    pub status_poll: Duration,
}

impl Default for SupervisorDeadlines {
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(20),
            start_ack: Duration::from_secs(15),
            shutdown_ack: Duration::from_secs(15),
            status_poll: Duration::from_secs(1),
        }
    }
}

/// The recording state a restart must restore verbatim.
///
/// Source data travels only toward the worker's stdin; Debug output stays
/// secret-free via the manual implementation.
#[derive(Clone)]
pub struct DesiredRecording {
    /// Camera id accepted by the worker (`cam-…`).
    pub camera: String,
    /// Storage root path (operator-known value).
    pub storage_root: String,
    /// Source payload (`{"kind": "...", ...}`) serialized into the
    /// `recording.start` params; NEVER logged nor Debug-printed.
    pub source_json: serde_json::Value,
    /// Segment target seconds.
    pub segment_target_secs: u64,
}

impl std::fmt::Debug for DesiredRecording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesiredRecording")
            .field("camera", &self.camera)
            .field("storage_root", &self.storage_root)
            .field("segment_target_secs", &self.segment_target_secs)
            .finish_non_exhaustive() // source_json deliberately omitted
    }
}

impl DesiredRecording {
    fn start_params(&self) -> serde_json::Value {
        serde_json::json!({
            "camera": self.camera,
            "storage": self.storage_root,
            "source": self.source_json,
            "segment_target_secs": self.segment_target_secs,
        })
    }
}

// ---- Episode plumbing -------------------------------------------------------

enum ReaderMessage {
    Frame(Box<Envelope>),
    Eof,
    DecodeError(String),
}

enum WriterCommand {
    Send(Box<Envelope>),
    Close,
}

/// Terminal recording-job state observed from status polls (§7): lets the
/// parent see FAILED/STOPPED jobs while the PROCESS stays alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobTerminal {
    /// Operator stop reached the job; everything finalized gracefully.
    Stopped,
    /// Finite source completed naturally.
    Completed,
    /// Permanent recording failure INSIDE the worker (typed category).
    Failed(String),
}

impl JobTerminal {
    /// Parses a `recording.status` result payload; `None` while running.
    fn parse(status: &serde_json::Value) -> Option<Self> {
        if !status.get("finished")?.as_bool()? {
            return None;
        }
        let end_kind = status.get("end_kind").and_then(serde_json::Value::as_str)?;
        match end_kind {
            "stopped" => Some(Self::Stopped),
            "completed" => Some(Self::Completed),
            "failed" => {
                let category = status
                    .get("failure_category")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown")
                    .to_owned();
                Some(Self::Failed(category))
            }
            _ => None,
        }
    }
}

/// One live episode's plumbing owned exclusively by the coordinator loop.
struct EpisodePipes {
    reader_rx: mpsc::Receiver<ReaderMessage>,
    stdin_tx: mpsc::Sender<WriterCommand>,
    child: Child,
}

/// Minimum lifetime before an episode resets restart economics (mirror of
/// the camera supervisor's stability rule).
const STABLE_EPISODE: Duration = Duration::from_secs(60);

/// Fast deaths allowed before supervision stops permanently (finding 5:
/// repeated instant deaths are a build/config problem).
const MAX_CONSECUTIVE_FAST_DEATHS: u32 = 5;

/// Internal episode outcome (before exit-status bookkeeping). Some
/// payload fields exist for logs/tests via Debug rendering only.
#[allow(dead_code)] // payloads are diagnostic-only
enum Flow {
    ShutdownAcked,
    EofAfterShutdownRequest,
    /// File-source recording completed inside a live worker.
    JobCompleted,
    /// Recording job reached a terminal FAILED/STOPPED state while the
    /// process stayed alive (finding 7): episode-level signal so the
    /// restart policy and hosts observe it; category accessible via
    /// `last_job_terminal()`.
    JobFailedObservation,
    ChildDied,
    Unresponsive {
        phase: &'static str,
    },
    TransientStartRefusal(String),
    PermanentStartRefusal(String),
    PermanentProtocol(String),
}

fn classify_start_refusal(code: String) -> Flow {
    const PERMANENT_PREFIXES: [&str; 3] = ["invalid_params", "job_already_active", "unsupported"];
    if PERMANENT_PREFIXES
        .iter()
        .any(|prefix| code.starts_with(prefix))
    {
        Flow::PermanentStartRefusal(code)
    } else {
        // finding 6: transient refusals retry under backoff — explicitly
        // NOT a shutdown, never an infinite loop for configuration rows.
        Flow::TransientStartRefusal(code)
    }
}

/// Supervises worker episodes with crash-restart + state restoration.
pub struct WorkerSupervisor<L: WorkerLauncher> {
    launcher: L,
    deadlines: SupervisorDeadlines,
    backoff: ReconnectBackoff,
    desired_recording: Option<DesiredRecording>,
    consecutive_fast_deaths: u32,
    /// Latest observed terminal job state within the current/last episode
    /// (§7 observability seam for hosts).
    last_job_terminal: Option<JobTerminal>,
}

impl<L: WorkerLauncher> WorkerSupervisor<L> {
    /// Wires a supervisor around a launcher with production deadlines.
    pub fn new(launcher: L) -> Self {
        Self::with_deadlines(launcher, SupervisorDeadlines::default())
    }

    /// Wires a supervisor with EXPLICIT deadlines (tests use milliseconds).
    pub fn with_deadlines(launcher: L, deadlines: SupervisorDeadlines) -> Self {
        Self {
            launcher,
            deadlines,
            backoff: ReconnectBackoff::default(),
            desired_recording: None,
            consecutive_fast_deaths: 0,
            last_job_terminal: None,
        }
    }

    /// Sets/updates the enforced recording state; resets episode economics.
    pub fn set_desired_recording(&mut self, desired: DesiredRecording) {
        self.desired_recording = Some(desired);
        self.backoff.reset();
        self.consecutive_fast_deaths = 0;
        self.last_job_terminal = None;
    }

    /// Clears desired state (restarts stop restoring anything).
    pub fn clear_desired_recording(&mut self) {
        self.desired_recording = None;
    }

    /// Consecutive fast deaths so far (observability/test seam).
    pub fn consecutive_fast_deaths(&self) -> u32 {
        self.consecutive_fast_deaths
    }

    /// Latest observed terminal recording-job state, if any (§7).
    pub fn last_job_terminal(&self) -> Option<&JobTerminal> {
        self.last_job_terminal.as_ref()
    }

    /// Runs ONE supervised episode. Death-in-any-phase paths return
    /// `RetryableEpisode`; only clean shutdown/completion or permanent
    /// classifications differ.
    #[allow(clippy::too_many_lines)] // one linear lifecycle per §14 bullet
    pub fn run_one_episode(
        &mut self,
        shutdown_requested: &dyn Fn() -> bool,
    ) -> Result<WorkerEnd, ApplicationError> {
        let started_at = Instant::now();
        let mut pipes = self.spawn_episode()?;
        let flow = self.coordinate(&mut pipes, shutdown_requested);

        // Flows whose WORKER MAY STILL BE ALIVE must not reach child.wait():
        // that would block this thread forever on a wedged process (finding
        // 5/7: unhealthy ≠ dead). Signal them to end (stdin close), force-
        // kill after courtesy is moot for tests, and reap whatever remains.
        if matches!(
            flow,
            Ok(Flow::Unresponsive { .. }
                | Flow::JobFailedObservation
                | Flow::TransientStartRefusal(_)
                | Flow::PermanentStartRefusal(_)
                | Flow::PermanentProtocol(_))
        ) {
            let _ = pipes.stdin_tx.send(WriterCommand::Close);
            let _ = pipes.child.kill();
        }

        // Reap + classify exit status no matter how coordination concluded.
        let exited_cleanly = pipes.child.wait().map(|s| s.success()).unwrap_or(false);
        let _ = exited_cleanly;
        drop(pipes);

        let stable_episode = started_at.elapsed() >= STABLE_EPISODE;
        match flow? {
            Flow::ShutdownAcked | Flow::EofAfterShutdownRequest => {
                self.backoff.reset();
                self.consecutive_fast_deaths = 0;
                Ok(WorkerEnd::RequestedShutdown)
            }
            Flow::JobCompleted => {
                self.backoff.reset();
                self.consecutive_fast_deaths = 0;
                Ok(WorkerEnd::JobCompletedCleanly)
            }
            Flow::ChildDied
            | Flow::Unresponsive { .. }
            | Flow::TransientStartRefusal(_)
            | Flow::JobFailedObservation => {
                if stable_episode {
                    self.backoff.reset();
                    self.consecutive_fast_deaths = 0;
                } else {
                    self.consecutive_fast_deaths = self.consecutive_fast_deaths.saturating_add(1);
                    if self.consecutive_fast_deaths >= MAX_CONSECUTIVE_FAST_DEATHS {
                        return Err(ApplicationError::PermanentRecordingConfig(format!(
                            "media worker died/went unhealthy \
                             {MAX_CONSECUTIVE_FAST_DEATHS} times without running stably; \
                             supervision stopped pending operator review"
                        )));
                    }
                }
                Ok(WorkerEnd::RetryableEpisode)
            }
            Flow::PermanentProtocol(message) => Err(ApplicationError::WorkerProtocol(message)),
            Flow::PermanentStartRefusal(code) => Err(ApplicationError::PermanentRecordingConfig(
                format!("worker refuses recording.start permanently: {code}"),
            )),
        }
    }

    /// Full service loop: episodes until operator shutdown, permanent
    /// failure, or clean job completion. Backoff waits are STOP-AWARE
    /// (finding 18): sliced sleeping checking the closure, mirroring the
    /// camera supervisor.
    pub fn run_forever(
        &mut self,
        shutdown_requested: &dyn Fn() -> bool,
        sleep: &dyn Fn(Duration),
    ) -> Result<WorkerEnd, ApplicationError> {
        loop {
            match self.run_one_episode(shutdown_requested)? {
                WorkerEnd::RequestedShutdown => return Ok(WorkerEnd::RequestedShutdown),
                WorkerEnd::JobCompletedCleanly => return Ok(WorkerEnd::JobCompletedCleanly),
                WorkerEnd::RetryableEpisode => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        delay_ms = delay.as_millis() as u64,
                        "worker episode failed/unhealthy; restarting"
                    );
                    let mut remaining = delay;
                    let slice = Duration::from_millis(100);
                    while remaining > Duration::ZERO && !shutdown_requested() {
                        let step = remaining.min(slice);
                        sleep(step);
                        remaining -= step;
                    }
                    if shutdown_requested() {
                        return Ok(WorkerEnd::RequestedShutdown);
                    }
                }
            }
        }
    }

    fn spawn_episode(&mut self) -> Result<EpisodePipes, ApplicationError> {
        let mut child = self.launcher.spawn().map_err(|error| {
            ApplicationError::WorkerLaunch(format!("cannot spawn media worker: {error}"))
        })?;
        let stdin_pipe = child.stdin.take().ok_or_else(|| {
            ApplicationError::WorkerProtocol("worker spawned without stdin".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ApplicationError::WorkerProtocol("worker spawned without stdout".to_owned())
        })?;

        // Dedicated READER thread (§3) over BOUNDED framing (§19).
        let (reader_tx, reader_rx) = mpsc::channel::<ReaderMessage>();
        let reader_name = format!("worker-stdout-{}", std::process::id());
        std::thread::Builder::new()
            .name(reader_name)
            .spawn(move || {
                let mut framed = nian_ipc::FramedReader::new(std::io::BufReader::new(stdout));
                loop {
                    match framed.next_message() {
                        Ok(Some(envelope)) => {
                            if reader_tx
                                .send(ReaderMessage::Frame(Box::new(envelope)))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ = reader_tx.send(ReaderMessage::Eof);
                            return;
                        }
                        Err(error) => {
                            let _ = reader_tx.send(ReaderMessage::DecodeError(error.to_string()));
                            return;
                        }
                    }
                }
            })
            .map_err(|error| {
                ApplicationError::WorkerLaunch(format!("reader thread failed: {error}"))
            })?;

        // Dedicated WRITER thread: sole owner of stdin (§3 discipline).
        let (stdin_tx, stdin_rx) = mpsc::channel::<WriterCommand>();
        let writer_name = format!("worker-stdin-{}", std::process::id());
        std::thread::Builder::new()
            .name(writer_name)
            .spawn(move || {
                let mut sink = stdin_pipe;
                loop {
                    match stdin_rx.recv() {
                        Ok(WriterCommand::Send(envelope)) => {
                            if write_frame(&mut sink, &envelope).is_err() {
                                return;
                            }
                        }
                        Ok(WriterCommand::Close) | Err(_) => return,
                    }
                }
            })
            .map_err(|error| {
                ApplicationError::WorkerLaunch(format!("writer thread failed: {error}"))
            })?;

        Ok(EpisodePipes {
            reader_rx,
            stdin_tx,
            child,
        })
    }

    #[allow(clippy::too_many_lines)] // one linear lifecycle per §14 bullet
    fn coordinate(
        &mut self,
        pipes: &mut EpisodePipes,
        shutdown_requested: &dyn Fn() -> bool,
    ) -> Result<Flow, ApplicationError> {
        // ---- Handshake with deadline (§4/§5) --------------------------------
        let hello_deadline = Instant::now() + self.deadlines.hello;
        loop {
            if shutdown_requested() {
                // Operator gave up during handshake: deliver shutdown and
                // classify whatever answer comes (or death) accordingly.
                return self.request_shutdown(pipes, shutdown_requested);
            }
            match recv_until(&pipes.reader_rx, hello_deadline)? {
                Recv::Frame(envelope) => match verify_hello(&envelope) {
                    Ok(hello) => {
                        let _ = hello;
                        break;
                    }
                    Err(message) => return Ok(Flow::PermanentProtocol(message)),
                },
                Recv::Eof => return Ok(Flow::ChildDied),
                Recv::DecodeError(text) => {
                    return Ok(Flow::PermanentProtocol(format!(
                        "malformed frame during hello: {text}"
                    )));
                }
                Recv::Tick => {}
            }
            if Instant::now() >= hello_deadline {
                let _ = pipes.stdin_tx.send(WriterCommand::Close);
                return Ok(Flow::Unresponsive { phase: "hello" });
            }
        }

        // ---- Restore desired recording state -------------------------------
        if let Some(desired) = self.desired_recording.clone() {
            send_request(
                &pipes.stdin_tx,
                1,
                "recording.start",
                desired.start_params(),
            )
            .map_err(|_| ApplicationError::WorkerProtocol("worker stdin closed".to_owned()))?;
            let ack_deadline = Instant::now() + self.deadlines.start_ack;
            loop {
                if shutdown_requested() {
                    return self.request_shutdown(pipes, shutdown_requested);
                }
                match recv_until(&pipes.reader_rx, ack_deadline)? {
                    Recv::Frame(envelope) => {
                        if let Envelope::Response {
                            id: reply_id,
                            ok,
                            error_code,
                            ..
                        } = *envelope
                            && reply_id == 1
                        {
                            if ok {
                                break;
                            }
                            let code = error_code.unwrap_or_else(|| "unknown".to_owned());
                            return Ok(classify_start_refusal(code));
                        }
                        // Stray other-id replies/events: keep waiting.
                    }
                    Recv::Eof => return Ok(Flow::ChildDied),
                    Recv::DecodeError(text) => {
                        return Ok(Flow::PermanentProtocol(format!(
                            "malformed frame awaiting start ack: {text}"
                        )));
                    }
                    Recv::Tick => {}
                }
                if Instant::now() >= ack_deadline {
                    let _ = pipes.stdin_tx.send(WriterCommand::Close);
                    return Ok(Flow::Unresponsive {
                        phase: "recording.start",
                    });
                }
            }
        }

        // ---- Monitor: poll-driven, responsive to shutdown (§3/§7) ----------
        loop {
            if shutdown_requested() {
                return self.request_shutdown(pipes, shutdown_requested);
            }

            if send_request(
                &pipes.stdin_tx,
                3,
                "recording.status",
                serde_json::Value::Null,
            )
            .is_err()
            {
                // stdin closed: worker died — crash semantics in this phase.
                return Ok(Flow::ChildDied);
            }

            let poll_end = Instant::now() + self.deadlines.status_poll;
            let mut poll_done = false;
            while !poll_done {
                if shutdown_requested() {
                    return self.request_shutdown(pipes, shutdown_requested);
                }
                match recv_until(&pipes.reader_rx, poll_end)? {
                    Recv::Frame(envelope) => {
                        if let Envelope::Response {
                            id: reply_id,
                            ok: true,
                            result,
                            ..
                        } = *envelope
                            && reply_id == 3
                        {
                            if let Some(terminal) = JobTerminal::parse(&result) {
                                self.mark_job_terminal(terminal.clone());
                                return Ok(match terminal {
                                    JobTerminal::Completed => Flow::JobCompleted,
                                    JobTerminal::Stopped => Flow::ShutdownAcked,
                                    JobTerminal::Failed(_) => Flow::JobFailedObservation,
                                });
                            }
                            poll_done = true; // running normally; next poll soon
                        }
                    }
                    Recv::Eof => return Ok(Flow::ChildDied),
                    Recv::DecodeError(text) => {
                        return Ok(Flow::PermanentProtocol(format!(
                            "malformed frame while monitoring: {text}"
                        )));
                    }
                    Recv::Tick => {}
                }
                if Instant::now() >= poll_end && !poll_done {
                    poll_done = true; // unanswered poll retried next round
                }
            }

            // finding 7 rows: a FAILED recording job inside a healthy-looking
            // process ends this episode so restart/permanent classification
            // happens ABOVE ("worker alive ≠ recording healthy").
            if matches!(self.last_job_terminal, Some(JobTerminal::Failed(_))) {
                return Ok(Flow::JobFailedObservation);
            }
        }
    }

    fn mark_job_terminal(&mut self, terminal: JobTerminal) {
        self.last_job_terminal = Some(terminal);
    }

    /// Delivers shutdown and awaits its ack with the configured deadline.
    /// A lost stdin (child already dead) classifies as CHILD DIED — crash
    /// semantics in every phase (§5), not a successful stop.
    fn request_shutdown(
        &self,
        pipes: &mut EpisodePipes,
        _shutdown_requested: &dyn Fn() -> bool,
    ) -> Result<Flow, ApplicationError> {
        let delivered = send_request(&pipes.stdin_tx, 2, "shutdown", serde_json::Value::Null);
        if delivered.is_err() {
            // stdin already gone: worker died mid-phase — crash semantics.
            return Ok(Flow::ChildDied);
        }
        let deadline = Instant::now() + self.deadlines.shutdown_ack;
        loop {
            match recv_until(&pipes.reader_rx, deadline)? {
                Recv::Frame(envelope) => {
                    if let Envelope::Response { id: reply_id, .. } = *envelope
                        && reply_id == 2
                    {
                        return Ok(Flow::ShutdownAcked);
                    }
                }
                Recv::Eof => return Ok(Flow::EofAfterShutdownRequest),
                Recv::DecodeError(text) => {
                    return Ok(Flow::PermanentProtocol(format!(
                        "malformed frame awaiting shutdown ack: {text}"
                    )));
                }
                Recv::Tick => {}
            }
            if Instant::now() >= deadline {
                return Ok(Flow::Unresponsive { phase: "shutdown" });
            }
        }
    }
}

// ---- Wire helpers -----------------------------------------------------------

/// Envelope variants seen inside `coordinate`; mirrors the concrete match
/// arms above for documentation.
#[allow(dead_code)]
enum ObservedEnvelope {}

/// Bounded receive with tick-based deadline checks: yields `Recv::Tick`
/// between short waits so callers can interleave shutdown polls; maps a
/// closed reader channel onto the protocol error type.
enum Recv {
    Frame(Box<Envelope>),
    Eof,
    DecodeError(String),
    Tick,
}

fn recv_until(
    rx: &mpsc::Receiver<ReaderMessage>,
    deadline: Instant,
) -> Result<Recv, ApplicationError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let wait = remaining.min(Duration::from_millis(50));
    match rx.recv_timeout(wait) {
        Ok(ReaderMessage::Frame(envelope)) => Ok(Recv::Frame(envelope)),
        Ok(ReaderMessage::Eof) => Ok(Recv::Eof),
        Ok(ReaderMessage::DecodeError(text)) => Ok(Recv::DecodeError(text)),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(Recv::Tick),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ApplicationError::WorkerProtocol(
            "worker stdout channel closed".to_owned(),
        )),
    }
}

fn write_frame(sink: &mut ChildStdin, envelope: &Envelope) -> Result<(), ()> {
    let line = serde_json::to_vec(envelope).map_err(|_| ())?;
    if line.len() > nian_ipc::MAX_MESSAGE_BYTES {
        return Err(());
    }
    sink.write_all(&line).map_err(|_| ())?;
    sink.write_all(b"\n").map_err(|_| ())?;
    sink.flush().map_err(|_| ())
}

/// Queues one REQUEST envelope with its parameters for the writer thread.
fn send_request(
    tx: &mpsc::Sender<WriterCommand>,
    id: u64,
    method: &'static str,
    params: serde_json::Value,
) -> Result<(), ()> {
    tx.send(WriterCommand::Send(Box::new(Envelope::Request {
        v: PROTOCOL_VERSION,
        id,
        method: method.to_owned(),
        params,
    })))
    .map_err(|_| ())
}

fn verify_hello(envelope: &Envelope) -> Result<&serde_json::Value, String> {
    let Envelope::Event { name, data, .. } = envelope else {
        // Anything else before hello is tolerated in moderation; hello must
        // eventually arrive — enforced by the surrounding deadline loop.
        return Err("first frame was not the hello event".to_owned());
    };
    if name != "hello" {
        return Err("first event was not hello".to_owned());
    }
    let protocol = data
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if protocol != u64::from(PROTOCOL_VERSION) {
        return Err(format!(
            "protocol mismatch: worker speaks {protocol}, we speak {PROTOCOL_VERSION}"
        ));
    }
    if data.get("ffmpeg").is_none() {
        return Err("hello missing ffmpeg capability report".to_owned());
    }
    Ok(data)
}
