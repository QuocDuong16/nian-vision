//! Deterministic stub-worker tests for parent-side supervision policy rows
//! (M3 remediation findings 4-7, 18; final remediation §1-§3, §8): each
//! launcher spawns a TINY SHELL SCRIPT through the real WorkerSupervisor
//! coordinator - no sleeps beyond injected millisecond deadlines, every
//! outcome row pinned.
//!
//! Every stub fixture uses the REAL worker protocol exactly: `recording.
//! status` results ARE the worker's JobStatus object (canonical shape,
//! final remediation §1) with STABLE snake_case failure categories (§9);
//! start acks are `{"started":true}`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use nian_application::{
    ApplicationError, DesiredRecording, JobTerminal, SupervisorDeadlines, WorkerEnd,
    WorkerLauncher, WorkerSupervisor,
};

fn test_deadlines() -> SupervisorDeadlines {
    SupervisorDeadlines {
        hello: Duration::from_millis(300),
        start_ack: Duration::from_millis(300),
        shutdown_ack: Duration::from_millis(200),
        status_poll: Duration::from_millis(20),
        status_response: Duration::from_millis(120),
    }
}

/// Deadlines for tests where a bash STUB must get its scripted flow through
/// under FULL-SUITE CPU contention: bash startup alone can exceed the tight
/// default handshake windows when many tests run in parallel, which would
/// turn every fixture into a spurious `Unresponsive{phase:"hello"}`. The
/// windows here are still bounded and stay milliseconds-scale in practice;
/// tests that ASSERT an elapsed bound (wedged hello, monitor silence) keep
/// the tight [`test_deadlines`] instead.
fn fixture_deadlines() -> SupervisorDeadlines {
    SupervisorDeadlines {
        hello: Duration::from_secs(5),
        start_ack: Duration::from_secs(5),
        shutdown_ack: Duration::from_secs(1),
        status_poll: Duration::from_millis(20),
        status_response: Duration::from_secs(2),
    }
}

fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(format!("{name}.sh"));
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    // Executable-bit setup is POSIX-only; these bash-stub tests are never
    // RUN on Windows, but the target must still compile (`cargo check
    // --all-targets --target x86_64-pc-windows-msvc`).
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    path.to_string_lossy().into_owned()
}

struct ScriptLauncher {
    program: String,
}

impl WorkerLauncher for ScriptLauncher {
    fn spawn(&mut self) -> std::io::Result<Child> {
        Command::new("bash")
            .arg("-c")
            .arg(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }
}

/// Spawn counter: proves (final remediation §2) that a permanent job
/// failure never spawns a second worker.
struct CountingLauncher {
    inner: ScriptLauncher,
    spawns: Arc<AtomicUsize>,
}

impl WorkerLauncher for CountingLauncher {
    fn spawn(&mut self) -> std::io::Result<Child> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn()
    }
}

const HELLO_OK: &str = r#"{"type":"event","v":1,"name":"hello","data":{"worker":"nian-media-worker","protocol":1,"ffmpeg":{"libavformat_major":62,"libavcodec_major":62,"libavutil_major":60}}}"#;

/// Start ack EXACTLY as the real worker answers `recording.start`.
const START_ACK: &str = r#"{"type":"response","v":1,"id":1,"ok":true,"result":{"started":true}}"#;

/// Shutdown ack EXACTLY as the real worker answers `shutdown`: the request
/// id (always 2 from the coordinator) echoed with `{"bye":true}`.
const SHUTDOWN_ACK: &str = r#"{"type":"response","v":1,"id":2,"ok":true,"result":{"bye":true}}"#;

/// Canonical terminal FAILED JobStatus with the STABLE wire category (§9).
fn failed_status_json(category: &str) -> String {
    format!(
        r#"{{"camera_id":"cam-z","state":"failed","retry_attempt":0,"finalized_segments":2,"finished":true,"end_kind":"failed","failure_category":"{category}","recovery":{{"recovered":0,"quarantined":0,"failed":1,"infrastructure_failures":1}}}}"#
    )
}

/// Canonical terminal JobStatus for ANY end kind (final correctness
/// remediation §3 fixtures: completed / stopped / failed).
fn terminal_status_json(end_kind: &str, category: &str) -> String {
    format!(
        r#"{{"camera_id":"cam-z","state":"{end_kind}","retry_attempt":0,"finalized_segments":2,"finished":true,"end_kind":"{end_kind}","failure_category":"{category}","recovery":null}}"#
    )
}

fn response(id: u64, result_json: &str) -> String {
    format!(r#"{{"type":"response","v":1,"id":{id},"ok":true,"result":{result_json}}}"#)
}

fn desired(camera: &str) -> DesiredRecording {
    DesiredRecording {
        camera: camera.to_owned(),
        storage_root: std::env::temp_dir()
            .join("nian-worker-supervisor-test")
            .to_string_lossy()
            .into_owned(),
        source_json: serde_json::json!({
            "kind": "file",
            "path": std::env::temp_dir().join("nian-worker-supervisor-fixture.mkv")
        }),
        segment_target_secs: 300,
        copy_audio: true,
    }
}

#[test]
fn crash_before_hello_is_a_retryable_episode() {
    let temp = tempfile::tempdir().unwrap();
    let prog = script(temp.path(), "instant-death", "exit 1");
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("expected retryable episode, got {other:?}"),
    }
    assert_eq!(supervisor.consecutive_fast_deaths(), 1);
}

#[test]
fn wedged_worker_no_hello_is_unhealthy_then_restartable() {
    let temp = tempfile::tempdir().unwrap();
    let prog = script(temp.path(), "silent", "sleep 5");
    let started = std::time::Instant::now();
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("expected unhealthy=>retryable, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "deadline must end the episode quickly"
    );
}

#[test]
fn protocol_version_mismatch_is_permanent() {
    let temp = tempfile::tempdir().unwrap();
    let bad = r#"{"type":"event","v":99,"name":"hello","data":{"protocol":99,"ffmpeg":{}}}"#;
    let body = format!("printf '%s\\n' '{bad}'\nexit 0\n");
    let prog = script(temp.path(), "bad-version", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::WorkerProtocol(_)) => {}
        other => panic!("expected permanent protocol error, got {other:?}"),
    }
}

#[test]
fn transient_start_refusal_retries_instead_of_stopping() {
    let temp = tempfile::tempdir().unwrap();
    // Final remediation §8: a GENUINELY transient code — `start_failed` is
    // the only one the real worker contract defines as transient (the job
    // thread could not spawn). `storage_unavailable` moved to the PERMANENT
    // contract and has its own test below.
    let refusal = r#"{"type":"response","v":1,"id":1,"ok":false,"error_code":"start_failed"}"#;
    let body = format!(
        "printf '%s\\n' '{HELLO_OK}'\n\
         read -r line\n\
         printf '%s\\n' '{refusal}'\n\
         sleep 60\n"
    );
    let prog = script(temp.path(), "transient-refusal", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-x"));
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("transient refusal must be retryable, got {other:?}"),
    }
}

#[test]
fn storage_unavailable_refusal_is_permanent_never_retried() {
    // Final remediation §8: `storage_unavailable` is genuine storage
    // infrastructure failure — a PERMANENT start refusal. Restarting the
    // worker would only loop; supervision must stop instead.
    let temp = tempfile::tempdir().unwrap();
    let refusal =
        r#"{"type":"response","v":1,"id":1,"ok":false,"error_code":"storage_unavailable"}"#;
    let body = format!(
        "printf '%s\\n' '{HELLO_OK}'\n\
         read -r line\n\
         printf '%s\\n' '{refusal}'\n\
         sleep 60\n"
    );
    let prog = script(temp.path(), "storage-unavailable-refusal", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let supervisor_launcher = CountingLauncher {
        inner: ScriptLauncher { program: prog },
        spawns: Arc::clone(&spawns),
    };
    let mut supervisor = WorkerSupervisor::with_deadlines(supervisor_launcher, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-storage"));
    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::PermanentRecordingConfig(_)) => {}
        other => panic!("storage_unavailable must be permanent, got {other:?}"),
    }
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "a permanent refusal must never lead to another spawn"
    );
}

#[test]
fn permanent_start_refusal_stops_supervision() {
    let temp = tempfile::tempdir().unwrap();
    let refusal =
        r#"{"type":"response","v":1,"id":1,"ok":false,"error_code":"invalid_params:nope"}"#;
    let body = format!(
        "printf '%s\\n' '{HELLO_OK}'\n\
         read -r line\n\
         printf '%s\\n' '{refusal}'\n\
         exit 0\n"
    );
    let prog = script(temp.path(), "permanent-refusal", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-y"));
    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::PermanentRecordingConfig(_)) => {}
        other => panic!("expected permanent failure, got {other:?}"),
    }
}

#[test]
fn recording_job_failure_is_observed_while_process_lives() {
    let temp = tempfile::tempdir().unwrap();
    // Ack id=1 with the REAL start ack, then answer EVERY later request
    // exactly like the real worker would: status polls with a canonical
    // terminal FAILED JobStatus (STABLE wire category), the protocol
    // shutdown with its id-echoed `bye` — while staying alive. finding 7:
    // terminal recording failure must be observable in a healthy process.
    let failed = response(3, &failed_status_json("storage_failed"));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\\n' '{START_ACK}' ;;\n\
             *'shutdown'*) printf '%s\\n' '{SHUTDOWN_ACK}' ;;\n\
             *) printf '%s\\n' '{failed}' ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "job-failed", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-z"));
    // Final remediation §2: a terminal FAILED job is PERMANENT for the
    // parent — typed error carrying the stable category, NOT a retryable
    // episode.
    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::PermanentRecordingFailure { category }) => {
            assert_eq!(category, "storage_failed");
        }
        other => panic!("failed observation must be permanent, got {other:?}"),
    }
    assert_eq!(
        supervisor.last_job_terminal(),
        Some(&JobTerminal::Failed("storage_failed".to_owned()))
    );
}

#[test]
fn storage_failed_job_never_spawns_another_worker() {
    // Final remediation §2 (explicit test row): a terminal StorageFailed
    // job must NOT spawn another worker. run_forever stops on the FIRST
    // episode's permanent job failure; the spawn counter proves no restart
    // ever happened.
    let temp = tempfile::tempdir().unwrap();
    let failed = response(3, &failed_status_json("storage_failed"));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\\n' '{START_ACK}' ;;\n\
             *'shutdown'*) printf '%s\\n' '{SHUTDOWN_ACK}' ;;\n\
             *) printf '%s\\n' '{failed}' ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "storage-failed-no-restart", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CountingLauncher {
            inner: ScriptLauncher { program: prog },
            spawns: Arc::clone(&spawns),
        },
        fixture_deadlines(),
    );
    supervisor.set_desired_recording(desired("cam-norestart"));
    let outcome = supervisor.run_forever(&|| false, &|_| {});
    match outcome {
        Err(ApplicationError::PermanentRecordingFailure { category }) => {
            assert_eq!(category, "storage_failed");
        }
        other => panic!("StorageFailed must stop supervision, got {other:?}"),
    }
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "a terminal StorageFailed job must never respawn the worker"
    );
}

#[test]
fn camera_in_use_job_never_spawns_another_worker() {
    let temp = tempfile::tempdir().unwrap();
    let failed = response(3, &terminal_status_json("failed", "camera_in_use"));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\\n' '{START_ACK}' ;;\n\
             *'shutdown'*) printf '%s\\n' '{SHUTDOWN_ACK}' ;;\n\
             *) printf '%s\\n' '{failed}' ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "camera-in-use-no-restart", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CountingLauncher {
            inner: ScriptLauncher { program: prog },
            spawns: Arc::clone(&spawns),
        },
        fixture_deadlines(),
    );
    supervisor.set_desired_recording(desired("cam-owned-elsewhere"));
    let outcome = supervisor.run_forever(&|| false, &|_| {});
    match outcome {
        Err(ApplicationError::PermanentRecordingFailure { category }) => {
            assert_eq!(category, "camera_in_use");
        }
        other => panic!("camera_in_use must stop supervision, got {other:?}"),
    }
    assert_eq!(
        supervisor.last_job_terminal(),
        Some(&JobTerminal::Failed("camera_in_use".to_owned()))
    );
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "camera ownership conflict must never churn worker restarts"
    );
}

#[test]
fn silent_after_start_ack_becomes_unresponsive_monitor_and_retries() {
    // Final remediation §3: a worker that acks recording.start and then
    // goes COMPLETELY SILENT (alive, never answering status polls) must be
    // bounded — Unresponsive in the monitor phase — and become a retryable
    // episode, NOT polled forever.
    let temp = tempfile::tempdir().unwrap();
    let body = format!(
        "printf '%s\\n' '{HELLO_OK}'\n\
         read -r line\n\
         printf '%s\\n' '{START_ACK}'\n\
         sleep 60\n"
    );
    let prog = script(temp.path(), "silent-after-ack", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    supervisor.set_desired_recording(desired("cam-silent"));
    let started = std::time::Instant::now();
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("monitor silence must be retryable, got {other:?}"),
    }
    // Two missed polls at 120 ms each + pacing: the bound must hold long
    // before any production-scale timeout would.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "silent worker must be bounded quickly, took {:?}",
        started.elapsed()
    );
}

#[test]
fn run_forever_backoff_wait_is_interrupted_by_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let prog = script(temp.path(), "always-dies", "exit 1");
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());

    let calls = Arc::new(AtomicUsize::new(0));
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_sleep = Arc::clone(&flag);
    let calls_for_test = Arc::clone(&calls);
    let sleep = move |duration: Duration| {
        let total = calls_for_test.fetch_add(1, Ordering::SeqCst);
        let _ = duration;
        if total == 0 {
            flag_for_sleep.store(true, Ordering::SeqCst);
        }
    };
    let result = supervisor.run_forever(&move || flag.load(Ordering::SeqCst), &sleep);
    match result.unwrap() {
        WorkerEnd::RequestedShutdown => {}
        other => panic!("shutdown during backoff must win: got {other:?}"),
    }
}
#[test]
fn terminal_completed_authoritative_even_when_shutdown_ack_is_lost() {
    // Final correctness remediation §3: the worker answers ONE status poll
    // with a terminal COMPLETED JobStatus and then NEVER acknowledges its
    // cleanup shutdown (alive but wedged). The parent must kill/reap the
    // process and STILL return JobCompletedCleanly — never reinterpret the
    // recording as an unhealthy retryable episode. run_forever must not
    // spawn a second worker.
    let temp = tempfile::tempdir().unwrap();
    let completed = response(3, &terminal_status_json("completed", ""));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         first=1\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\n' '{START_ACK}' ;;\n\
             *) if [ \"$first\" = 1 ]; then first=0; printf '%s\n' '{completed}'; else sleep 60; fi ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "completed-no-ack", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CountingLauncher {
            inner: ScriptLauncher { program: prog },
            spawns: Arc::clone(&spawns),
        },
        fixture_deadlines(),
    );
    supervisor.set_desired_recording(desired("cam-term-completed"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Ok(WorkerEnd::JobCompletedCleanly) => {}
        other => panic!("terminal Completed must stay authoritative, got {other:?}"),
    }
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "a completed job must never be restarted regardless of cleanup outcome"
    );
}

#[test]
fn terminal_failed_authoritative_even_when_shutdown_ack_is_lost() {
    // Final correctness remediation §3: terminal FAILED + lost shutdown ack
    // must remain PermanentRecordingFailure — never a retryable episode.
    let temp = tempfile::tempdir().unwrap();
    let failed = response(3, &terminal_status_json("failed", "storage_failed"));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         first=1\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\n' '{START_ACK}' ;;\n\
             *) if [ \"$first\" = 1 ]; then first=0; printf '%s\n' '{failed}'; else sleep 60; fi ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "failed-no-ack", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CountingLauncher {
            inner: ScriptLauncher { program: prog },
            spawns: Arc::clone(&spawns),
        },
        fixture_deadlines(),
    );
    supervisor.set_desired_recording(desired("cam-term-failed"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Err(ApplicationError::PermanentRecordingFailure { category }) => {
            assert_eq!(category, "storage_failed");
        }
        other => panic!("terminal Failed must stay permanent, got {other:?}"),
    }
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

#[test]
fn terminal_stopped_authoritative_even_when_shutdown_ack_is_lost() {
    // Final correctness remediation §3: terminal STOPPED + lost shutdown
    // ack remains RequestedShutdown — no retryable reinterpretation.
    let temp = tempfile::tempdir().unwrap();
    let stopped = response(3, &terminal_status_json("stopped", ""));
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         first=1\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\n' '{START_ACK}' ;;\n\
             *) if [ \"$first\" = 1 ]; then first=0; printf '%s\n' '{stopped}'; else sleep 60; fi ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "stopped-no-ack", &body);
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CountingLauncher {
            inner: ScriptLauncher { program: prog },
            spawns: Arc::clone(&spawns),
        },
        fixture_deadlines(),
    );
    supervisor.set_desired_recording(desired("cam-term-stopped"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Ok(WorkerEnd::RequestedShutdown) => {}
        other => panic!("terminal Stopped must stay RequestedShutdown, got {other:?}"),
    }
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

#[test]
fn late_status_response_cannot_satisfy_a_newer_poll() {
    // Final correctness remediation §4: poll 1 (id 3) is answered LATE
    // (400 ms, after its response window already expired); poll 2 (id 4, a
    // DIFFERENT id) is never answered. The late id-3 response arrives
    // during poll 2's window and MUST be ignored — the missed-response
    // policy stays correct and the episode becomes Unresponsive{monitor}
    // within its bound instead of being pacified by stale frames.
    let temp = tempfile::tempdir().unwrap();
    let running_status = r#"{"camera_id":"cam-late","state":"recording","retry_attempt":0,"finalized_segments":0,"finished":false,"end_kind":"","failure_category":"","recovery":null}"#;
    let running = response(3, running_status);
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         first=1\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\n' '{START_ACK}' ;;\n\
             *) if [ \"$first\" = 1 ]; then first=0; sleep 0.4; printf '%s\n' '{running}'; else sleep 60; fi ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(temp.path(), "late-response", &body);
    let mut deadlines = fixture_deadlines();
    deadlines.status_response = Duration::from_millis(150);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, deadlines);
    supervisor.set_desired_recording(desired("cam-late"));
    let started = std::time::Instant::now();
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("unanswered newer poll must stay Unresponsive, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the missed-response bound must hold, took {started:?}"
    );
}

fn violating_status_json(extra: &str) -> String {
    format!(
        r#"{{"camera_id":"cam-violation","state":"failed","retry_attempt":0,"finalized_segments":0,"finished":true{extra},"recovery":null}}"#
    )
}

fn protocol_violation_stub(
    dir: &std::path::Path,
    name: &str,
    status_json: &str,
) -> CountingLauncher {
    // Ack id=1 like the real worker, then answer EVERY status poll with the
    // violating terminal payload while staying alive.
    let violating = response(3, status_json);
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         while read -r req; do\n\
           case \"$req\" in\n\
             *'\"id\":1,'*) printf '%s\\n' '{START_ACK}' ;;\n\
             *'shutdown'*) printf '%s\\n' '{SHUTDOWN_ACK}' ;;\n\
             *) printf '%s\\n' '{violating}' ;;\n\
           esac\n\
         done\n"
    );
    let prog = script(dir, name, &body);
    CountingLauncher {
        inner: ScriptLauncher { program: prog },
        spawns: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn finished_status_with_unknown_end_kind_is_a_protocol_violation() {
    // Final safety remediation §7: finished=true with an unknown end_kind
    // must NEVER be silently interpreted as "still running" — it is a
    // permanent protocol violation, and supervision stops instead of
    // looping.
    let temp = tempfile::tempdir().unwrap();
    let status = violating_status_json(r#","end_kind":"mystery""#);
    let launcher = protocol_violation_stub(temp.path(), "unknown-end-kind", &status);
    let spawns = Arc::clone(&launcher.spawns);
    let mut supervisor = WorkerSupervisor::with_deadlines(launcher, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-violation"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Err(ApplicationError::WorkerProtocol(message)) => {
            assert!(
                message.contains("end_kind"),
                "the violation must name the offending field: {message}"
            );
        }
        other => panic!("unknown end_kind must be a protocol violation, got {other:?}"),
    }
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        1,
        "a protocol violation must never spawn another worker"
    );
}

#[test]
fn failed_status_without_category_is_a_protocol_violation() {
    // Final safety remediation §7: finished=failed with a MISSING
    // failure_category is a protocol violation — never an invented
    // `Unknown` category and never "still running".
    let temp = tempfile::tempdir().unwrap();
    let status = violating_status_json(r#","end_kind":"failed""#);
    let launcher = protocol_violation_stub(temp.path(), "failed-no-category", &status);
    let spawns = Arc::clone(&launcher.spawns);
    let mut supervisor = WorkerSupervisor::with_deadlines(launcher, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-violation"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Err(ApplicationError::WorkerProtocol(message)) => {
            assert!(
                message.contains("failure_category"),
                "the violation must name the offending field: {message}"
            );
        }
        other => panic!("missing failure_category must be a violation, got {other:?}"),
    }
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_status_with_unknown_category_is_a_protocol_violation() {
    // Final safety remediation §7: a failure_category outside the SHARED
    // canonical vocabulary (nian-domain's FailureCategory) is a protocol
    // violation — the parent never invents an interpretation for a string
    // both binaries have not agreed on.
    let temp = tempfile::tempdir().unwrap();
    let status = violating_status_json(r#","end_kind":"failed","failure_category":"mystery""#);
    let launcher = protocol_violation_stub(temp.path(), "failed-unknown-category", &status);
    let spawns = Arc::clone(&launcher.spawns);
    let mut supervisor = WorkerSupervisor::with_deadlines(launcher, fixture_deadlines());
    supervisor.set_desired_recording(desired("cam-violation"));
    match supervisor.run_forever(&|| false, &|_| {}) {
        Err(ApplicationError::WorkerProtocol(message)) => {
            assert!(
                message.contains("failure_category"),
                "the violation must name the offending field: {message}"
            );
        }
        other => panic!("unknown failure_category must be a violation, got {other:?}"),
    }
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}
