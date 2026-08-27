//! Deterministic stub-worker tests for parent-side supervision policy rows
//! (M3 remediation findings 4-7, 18): each launcher spawns a TINY SHELL
//! SCRIPT through the real WorkerSupervisor coordinator - no sleeps beyond
//! injected millisecond deadlines, every outcome row pinned.

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

const HELLO_OK: &str = r#"{"type":"event","v":1,"name":"hello","data":{"protocol":1,"ffmpeg":{"libavformat_major":62}}}"#;

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
    let refusal =
        r#"{"type":"response","v":1,"id":1,"ok":false,"error_code":"storage_unavailable"}"#;
    let body = format!(
        "printf '%s\\n' '{HELLO_OK}'\n\
         read -r line\n\
         printf '%s\\n' '{refusal}'\n\
         sleep 60\n"
    );
    let prog = script(temp.path(), "transient-refusal", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-x".to_owned(),
        storage_root: "/tmp".to_owned(),
        source_json: serde_json::json!({"kind": "file", "path": "/dev/null"}),
        segment_target_secs: 300,
    });
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("transient refusal must be retryable, got {other:?}"),
    }
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
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-y".to_owned(),
        storage_root: "/tmp".to_owned(),
        source_json: serde_json::json!({"kind": "rtsp", "url": "x"}),
        segment_target_secs: 300,
    });
    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::PermanentRecordingConfig(_)) => {}
        other => panic!("expected permanent failure, got {other:?}"),
    }
}

#[test]
fn recording_job_failure_is_observed_while_process_lives() {
    let temp = tempfile::tempdir().unwrap();
    // Ack id=1 then answer EVERY later request (status poll id>=3, plus
    // any shutdown) with a FAILED job status while staying alive. finding
    // 7: terminal recording failure must be observable in a healthy process.
    let failed_status = r#"{"type":"response","v":1,"id":3,"ok":true,"result":{"finished":true,"end_kind":"failed","failure_category":"StorageFailed","finalized_segments":2}}"#;
    let generic_ack = r#"{"type":"response","v":1,"id":1,"ok":true,"result":{}}"#;
    let body = format!(
        "printf '%s\n' '{HELLO_OK}'\n\
         while read -r req; do\n\
           case \"$req\" in *'\"id\":1,'*) printf '%s\\n' '{generic_ack}' ;; *) printf '%s\\n' '{failed_status}' ;; esac\n\
         done\n"
    );
    let prog = script(temp.path(), "job-failed", &body);
    let mut supervisor =
        WorkerSupervisor::with_deadlines(ScriptLauncher { program: prog }, test_deadlines());
    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-z".to_owned(),
        storage_root: "/tmp".to_owned(),
        source_json: serde_json::json!({"kind": "file", "path": "/dev/null"}),
        segment_target_secs: 300,
    });
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("failed observation ends episode, got {other:?}"),
    }
    assert_eq!(
        supervisor.last_job_terminal(),
        Some(&JobTerminal::Failed("StorageFailed".to_owned()))
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
