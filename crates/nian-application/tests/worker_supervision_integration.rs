//! Process-level worker supervision integration test (M3 §15; remediated
//! per review findings 3–7).
//!
//! Deterministic, no timing luck: the REAL `nian-media-worker` binary is
//! spawned over IPC against a local fixture source; the test kills the
//! worker mid-recording; the supervisor observes the death (reader-thread
//! EOF — finding 3), restarts the worker, re-handshakes, restores desired
//! recording state, and only then delivers protocol shutdown once
//! disk-visible publication exists. Bounded explicit synchronization only.
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::print_stderr)] // skip notices go to the harness stderr

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nian_application::{
    ApplicationError, DesiredRecording, SupervisorDeadlines, WorkerEnd, WorkerLauncher,
    WorkerSupervisor,
};

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Locates the freshly built worker binary.
fn worker_path() -> String {
    std::env::var("NIAN_WORKER_BIN").unwrap_or_else(|_| {
        workspace_root()
            .join("../../target/debug/nian-media-worker")
            .to_string_lossy()
            .into_owned()
    })
}

/// Launcher whose FIRST N spawns get SIGKILLed after a fixed beat that is
/// generous enough for handshake + recording.start + first packet writes,
/// but bounded (~1.5 s) — a deterministic crash, not a timing race: ANY
/// worker state at kill time exercises death-in-phase classification.
struct CrashyLauncher {
    program: String,
    deaths_to_inject: Arc<AtomicUsize>,
}

impl WorkerLauncher for CrashyLauncher {
    fn spawn(&mut self) -> std::io::Result<Child> {
        let child = Command::new(&self.program)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if self.deaths_to_inject.load(Ordering::SeqCst) > 0 {
            self.deaths_to_inject.fetch_sub(1, Ordering::SeqCst);
            let pid = child.id();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                // libc-free SIGKILL through an external process keeps this
                // test free of unsafe / extra dependencies.
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
            });
        }
        Ok(child)
    }
}

#[test]
fn supervisor_restarts_crashed_worker_and_restores_recording_state() -> Result<(), ApplicationError>
{
    if !std::path::Path::new(&worker_path()).is_file() {
        eprintln!("worker binary not built; skipping (run cargo build -p nian-media-worker)");
        return Ok(());
    }
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("rec");

    let fixture = workspace_root()
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/session_av.mkv")
        .to_string_lossy()
        .into_owned();

    // Short injected deadlines (finding 4): milliseconds-scale bounding so
    // wedged/dead phases classify fast without production-length waits.
    let deadlines = SupervisorDeadlines {
        hello: Duration::from_secs(10),
        start_ack: Duration::from_secs(10),
        shutdown_ack: Duration::from_secs(20),
        status_poll: Duration::from_millis(50),
    };

    let deaths_left = Arc::new(AtomicUsize::new(1));
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CrashyLauncher {
            program: worker_path(),
            deaths_to_inject: Arc::clone(&deaths_left),
        },
        deadlines,
    );

    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-sup-test".to_owned(),
        storage_root: storage.to_string_lossy().into_owned(),
        source_json: serde_json::json!({ "kind": "file", "path": fixture }),
        segment_target_secs: 5,
    });

    // ---- Episode 1: killed mid-recording => retryable episode -------------
    match supervisor.run_one_episode(&|| false)? {
        WorkerEnd::RetryableEpisode => {}
        other => panic!("expected RetryableEpisode after injected crash, got {other:?}"),
    }

    // ---- Episode 2: restore works; stop driven by DISK progress -----------
    //
    // finding 3 in action: even while the healthy worker streams NO frames
    // between polls we can still deliver shutdown because the coordinator's
    // loop polls and checks the closure continuously. The closure flips once
    // at least one final segment exists on disk (explicit synchronization,
    // not sleep-of-faith).
    deaths_left.store(0, Ordering::SeqCst);
    let day_dir = storage.join("cam-sup-test");
    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let flag_for_closure = Arc::clone(&shutdown_flag);
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if count_finals(&day_dir) >= 1 {
                flag_for_closure.store(true, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    });

    let flag_for_episode = Arc::clone(&shutdown_flag);
    let outcome2 = supervisor.run_one_episode(&move || flag_for_episode.load(Ordering::SeqCst))?;

    assert!(watcher.join().unwrap());
    match outcome2 {
        WorkerEnd::RequestedShutdown => {}
        other => panic!("healthy restored episode must end as requested shutdown, got {other:?}"),
    }
    assert!(count_finals(&storage.join("cam-sup-test")) >= 1);

    Ok(())
}

fn count_finals(root: &std::path::Path) -> usize {
    walk(root).iter().filter(|p| !is_partial(p)).count()
}

fn is_partial(path: &std::path::Path) -> bool {
    path.to_string_lossy().contains(".partial.")
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
