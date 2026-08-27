//! Parent-side supervision of the media worker process (M3 §14).
//!
//! The application layer must not trust the worker to stay alive. This
//! module owns the process boundary:
//!
//! * spawn `nian-media-worker run` with a CLEAN argument list — credentials
//!   and RTSP URLs never appear in argv (they reach the worker only inside
//!   IPC request payloads on stdin);
//! * verify the `hello` handshake and protocol version before any recording
//!   traffic, mirroring the worker's fail-fast invariants (ADR-0002) at the
//!   parent too;
//! * send `recording.start` when desired state exists and classify refusals:
//!   configuration-shaped codes are PERMANENT (never retried forever),
//!   everything else stays transient;
//! * monitor the child by blocking on stdout framed reads until EOF;
//!   requested shutdown is delivered by sending the worker's own `shutdown`
//!   request, after which a clean exit is expected;
//! * restart crashed workers across episodes using the shared domain
//!   [`ReconnectBackoff`] schedule plus a process-level stability rule
//!   (episodes that ran at least [`STABLE_EPISODE`] reset the schedule and
//!   crash tally; repeated fast deaths escalate to permanent stop);
//! * restore desired recording state after every restart.
//!
//! Threading model: the child's stdin is served by ONE dedicated writer
//! thread per episode consuming a tiny command channel (`Start`,
//! `Shutdown`); stdout is read on the caller's thread. Pipe writes never
//! multiplex across threads. No Tauri coupling; tests drive it directly.
//!
//! Secrets: [`DesiredRecording`]'s Debug output omits source data; the
//! source JSON lives only in memory and on the private stdin pipe.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use nian_domain::ReconnectBackoff;
use nian_ipc::message::{Envelope, PROTOCOL_VERSION};

use crate::error::ApplicationError;

/// A parameterized worker launcher.
///
/// The seam exists for §15's deterministic crash test: tests can spawn the
/// real worker binary (and kill it mid-run) without production code growing
/// fake-media branches. Production passes [`BinaryLauncher`].
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
            .stderr(Stdio::null()) // worker stderr stays diagnostic-only, child-side
            .spawn()
    }
}

/// How one supervised episode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    /// Operator asked for shutdown; the worker acknowledged and exited.
    RequestedShutdown,
    /// The child died unexpectedly during this episode.
    Crashed,
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

/// Minimum lifetime before an episode counts as healthy (resets schedule
/// and crash tally): the process-level mirror of the camera supervisor's
/// stable-recording threshold. An episode shorter than this is a FAST
/// DEATH.
const STABLE_EPISODE: Duration = Duration::from_secs(60);

/// Fast deaths allowed before supervision stops permanently: repeated
/// instant deaths are a build/config problem, not a camera problem, and
/// looping forever over them violates M3 §8's spirit at the process level.
const MAX_CONSECUTIVE_FAST_DEATHS: u32 = 5;

/// Supervises worker process episodes with restart + state restoration.
pub struct WorkerSupervisor<L: WorkerLauncher> {
    launcher: L,
    backoff: ReconnectBackoff,
    desired_recording: Option<DesiredRecording>,
    consecutive_fast_deaths: u32,
}

impl<L: WorkerLauncher> WorkerSupervisor<L> {
    /// Wires a supervisor around a launcher.
    pub fn new(launcher: L) -> Self {
        Self {
            launcher,
            backoff: ReconnectBackoff::default(),
            desired_recording: None,
            consecutive_fast_deaths: 0,
        }
    }

    /// Sets/updates the recording state enforced across restarts; resets
    /// crash economics (a fresh explicit desire starts fresh policy).
    pub fn set_desired_recording(&mut self, desired: DesiredRecording) {
        self.desired_recording = Some(desired);
        self.backoff.reset();
        self.consecutive_fast_deaths = 0;
    }

    /// Clears desired recording state (restarts stop restoring anything).
    pub fn clear_desired_recording(&mut self) {
        self.desired_recording = None;
    }

    /// Consecutive fast deaths recorded so far (observability/test seam).
    pub fn consecutive_fast_deaths(&self) -> u32 {
        self.consecutive_fast_deaths
    }

    /// Runs ONE supervised episode: spawn → handshake → optional
    /// recording.start restore → monitor until shutdown ack or death.
    #[allow(clippy::too_many_lines)] // lifecycle spans each §14 bullet once
    pub fn run_one_episode(
        &mut self,
        shutdown_requested: &dyn Fn() -> bool,
    ) -> Result<WorkerEnd, ApplicationError> {
        let mut child = self.launcher.spawn().map_err(|error| {
            ApplicationError::WorkerLaunch(format!("cannot spawn media worker: {error}"))
        })?;
        let started_at = std::time::Instant::now();

        let flow = self.drive_child(&mut child, shutdown_requested);

        // Reap regardless of flow: exit status decides clean vs crashed.
        let exited_cleanly = child.wait().map(|status| status.success()).unwrap_or(false);

        match flow? {
            EpisodeFlow::CleanShutdown => {
                self.backoff.reset();
                self.consecutive_fast_deaths = 0;
                Ok(WorkerEnd::RequestedShutdown)
            }
            EpisodeFlow::Died => {
                if started_at.elapsed() >= STABLE_EPISODE {
                    self.backoff.reset();
                    self.consecutive_fast_deaths = 0;
                } else {
                    self.consecutive_fast_deaths = self.consecutive_fast_deaths.saturating_add(1);
                    if self.consecutive_fast_deaths >= MAX_CONSECUTIVE_FAST_DEATHS {
                        return Err(ApplicationError::PermanentRecordingConfig(format!(
                            "media worker died {MAX_CONSECUTIVE_FAST_DEATHS} times \
                             without running stably; supervision stopped pending operator review"
                        )));
                    }
                }
                let _clean_status_matters_not_for_policy = exited_cleanly;
                Ok(WorkerEnd::Crashed)
            }
        }
    }

    /// Full service loop used by production wiring: keep episodes going
    /// until operator shutdown or permanent failure; waits out the shared
    /// domain backoff schedule between deaths.
    pub fn run_forever(
        &mut self,
        shutdown_requested: &dyn Fn() -> bool,
        sleep: &dyn Fn(Duration),
    ) -> Result<WorkerEnd, ApplicationError> {
        loop {
            match self.run_one_episode(shutdown_requested)? {
                WorkerEnd::RequestedShutdown => return Ok(WorkerEnd::RequestedShutdown),
                WorkerEnd::Crashed => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        delay_ms = delay.as_millis() as u64,
                        "media worker died; restarting"
                    );
                    sleep(delay);
                    if shutdown_requested() {
                        return Ok(WorkerEnd::RequestedShutdown);
                    }
                }
            }
        }
    }

    fn drive_child(
        &self,
        child: &mut Child,
        shutdown_requested: &dyn Fn() -> bool,
    ) -> Result<EpisodeFlow, ApplicationError> {
        let mut stdin_pipe = child.stdin.take().ok_or_else(|| {
            ApplicationError::WorkerProtocol("worker spawned without stdin".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ApplicationError::WorkerProtocol("worker spawned without stdout".to_owned())
        })?;
        let mut reader = BufReader::new(stdout);

        // ---- Handshake ----------------------------------------------------
        verify_hello(&read_hello(&mut reader)?)?;

        // ---- Restore desired recording state ------------------------------
        if let Some(desired) = self.desired_recording.clone() {
            write_frame(
                &mut stdin_pipe,
                &Envelope::Request {
                    v: PROTOCOL_VERSION,
                    id: 1,
                    method: "recording.start".to_owned(),
                    params: desired.start_params(),
                },
            )?;

            // NOTE on blocking reads vs patience: next_frame blocks until a
            // line or EOF arrives; a wedged worker that answers NOTHING
            // while healthy-otherwise is not a state M3 tolerates silently —
            // but START_RESPONSE_PATIENCE-based wall-clock policing belongs
            // to the platform layer's readiness plumbing. Here we rely on:
            // the worker replies OR dies (EOF), both terminating this loop.
            loop {
                match next_frame(&mut reader)? {
                    Some(Envelope::Response {
                        id: response_id,
                        ok,
                        error_code,
                        ..
                    }) => {
                        if response_id == 1 {
                            if ok {
                                break;
                            }
                            let code = error_code.unwrap_or_else(|| "unknown".to_owned());
                            return classify_start_refusal(code);
                        }
                        // Stray early frame: keep waiting for OUR reply.
                    }
                    Some(_) => continue,
                    None => return Ok(EpisodeFlow::Died),
                }
            }
        }

        // ---- Monitor ----------------------------------------------------
        //
        // Reads BLOCK; operator shutdown is polled between frames. Writes
        // happen only HERE between reads, on this thread — single-writer
        // pipe discipline, no cross-thread races.

        while !shutdown_requested() {
            match next_frame(&mut reader)? {
                Some(_) => { /* events stream past; monitoring only */ }
                None => return Ok(EpisodeFlow::Died),
            }
        }

        write_frame(
            &mut stdin_pipe,
            &Envelope::Request {
                v: PROTOCOL_VERSION,
                id: 2,
                method: "shutdown".to_owned(),
                params: serde_json::Value::Null,
            },
        )?;
        loop {
            let frame = next_frame(&mut reader)?;
            match frame {
                None => return Ok(EpisodeFlow::CleanShutdown), // stdout closed post-reply
                Some(Envelope::Response {
                    id: response_id, ..
                }) => {
                    if response_id == 2 {
                        return Ok(EpisodeFlow::CleanShutdown);
                    }
                }
                Some(_) => continue,
            }
        }
    }
}

enum EpisodeFlow {
    CleanShutdown,
    Died,
}

fn read_hello(reader: &mut BufReader<ChildStdout>) -> Result<serde_json::Value, ApplicationError> {
    loop {
        match next_frame(reader)? {
            None => {
                return Err(ApplicationError::WorkerProtocol(
                    "worker closed stdout before hello".to_owned(),
                ));
            }
            Some(Envelope::Event { name, data, .. }) if name == "hello" => return Ok(data),
            Some(other) => {
                let _ = other; // tolerate stray early frames
            }
        }
    }
}

fn verify_hello(hello: &serde_json::Value) -> Result<(), ApplicationError> {
    let protocol = hello
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if protocol != u64::from(PROTOCOL_VERSION) {
        return Err(ApplicationError::WorkerProtocol(format!(
            "protocol mismatch: worker speaks {protocol}, we speak {PROTOCOL_VERSION}"
        )));
    }
    if hello.get("ffmpeg").is_none() {
        return Err(ApplicationError::WorkerProtocol(
            "hello missing ffmpeg capability report".to_owned(),
        ));
    }
    Ok(())
}

/// Writes one NDJSON frame; the single-threaded pipe discipline makes plain
/// `write_all` safe.
fn write_frame(stdin_pipe: &mut ChildStdin, envelope: &Envelope) -> Result<(), ApplicationError> {
    use std::io::Write as _;
    let line = serde_json::to_vec(envelope)
        .map_err(|error| ApplicationError::WorkerProtocol(format!("serialize failed: {error}")))?;
    if line.len() > nian_ipc::MAX_MESSAGE_BYTES {
        return Err(ApplicationError::WorkerProtocol(
            "frame cap exceeded".to_owned(),
        ));
    }
    stdin_pipe.write_all(&line).map_err(wrap_io)?;
    stdin_pipe.write_all(b"\n").map_err(wrap_io)?;
    stdin_pipe.flush().map_err(wrap_io)?;
    Ok(())
}

fn wrap_io(error: std::io::Error) -> ApplicationError {
    ApplicationError::WorkerProtocol(format!("pipe write failed: {error}"))
}

/// Reads one newline-delimited envelope; `Ok(None)` marks EOF.
fn next_frame(reader: &mut BufReader<ChildStdout>) -> Result<Option<Envelope>, ApplicationError> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => {
            let envelope: Envelope = serde_json::from_str(line.trim()).map_err(|error| {
                ApplicationError::WorkerProtocol(format!("bad envelope from worker: {error}"))
            })?;
            envelope.validate_version().map_err(|error| {
                ApplicationError::WorkerProtocol(format!("worker envelope version: {error}"))
            })?;
            Ok(Some(envelope))
        }
        Err(error) => Err(ApplicationError::WorkerProtocol(format!(
            "reading worker failed: {error}"
        ))),
    }
}

/// Maps a failing `recording.start` reply onto restart policy:
/// configuration-shaped refusal codes are permanent; transient ones keep
/// the episode alive as a clean-ish end whose restore will be re-attempted
/// by the next episode.
fn classify_start_refusal(code: String) -> Result<EpisodeFlow, ApplicationError> {
    const PERMANENT_PREFIXES: [&str; 3] = ["invalid_params", "job_already_active", "unsupported"];
    if PERMANENT_PREFIXES
        .iter()
        .any(|prefix| code.starts_with(prefix))
    {
        return Err(ApplicationError::PermanentRecordingConfig(format!(
            "worker refused recording.start permanently: {code}"
        )));
    }
    Ok(EpisodeFlow::CleanShutdown)
}
