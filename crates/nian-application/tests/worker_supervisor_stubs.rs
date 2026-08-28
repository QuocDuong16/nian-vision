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
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
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

fn response(id: u64, result_json: &str) -> String {
    format!(r#"{{"type":"response","v":1,"id":{id},"ok":true,"result":{result_json}}}"#)
}

fn desired(camera: &str) -> DesiredRecording {
    DesiredRecording {
        camera: camera.to_owned(),
        storage_root: "/tmp".to_owned(),
        source_json: serde_json::json!({"kind": "file", "path": "/dev/null"}),
        segment_target_secs: 300,
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
