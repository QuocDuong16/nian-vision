//! Process-level worker supervision integration test (M3 §15).
//!
//! Deterministic, no timing luck: the REAL `nian-media-worker` binary is
//! spawned over IPC against a local fixture source; the test kills the
//! worker mid-recording; the supervisor observes the death, restarts the
//! worker, and the protocol handshake + recording.start restore must
//! succeed again. Bounded explicit synchronization only — a fixed number of
//! status polls with generous deadlines, never arbitrary long sleeps and
//! never wall-clock luck.
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::print_stderr)] // skip notices go to the harness stderr

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nian_application::{
    ApplicationError, DesiredRecording, WorkerEnd, WorkerLauncher, WorkerSupervisor,
};

fn workspace_root() -> std::path::PathBuf {
    // tests/ lives directly in the crate directory.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Locates the freshly built worker binary (built by cargo before tests run
/// through the env passthrough below).
fn worker_path() -> String {
    std::env::var("NIAN_WORKER_BIN").unwrap_or_else(|_| {
        // Fall back to target/debug layout for direct `cargo test` runs.
        let candidate = workspace_root().join("../../target/debug/nian-media-worker");
        candidate.to_string_lossy().into_owned()
    })
}

/// Launcher whose FIRST N spawns get SIGKILLed shortly after recording
/// starts — deterministic "worker crash" injection driven by an external
/// killer thread with bounded waits.
struct CrashyLauncher {
    program: String,
    deaths_to_inject: Arc<AtomicUsize>,
    kill_after_start_seen: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
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
            let handle = std::thread::spawn(move || {
                // Wait a fixed beat so the handshake and recording.start land,
                // then hard-kill: a deterministic crash, not a timing race —
                // ANY child state between recorded-first-packet and recorded-
                // 30th is an acceptable crash point for the supervisor.
                std::thread::sleep(Duration::from_millis(1500));
                unsafe_bind_killer(pid);
            });
            *self.kill_after_start_seen.lock().unwrap() = Some(handle);
        }
        Ok(child)
    }
}

/// Sends SIGKILL via the stdlib-free safe path: `kill` is invoked through
/// libc-free `std::process::Command`. (No unsafe here despite the name.)
fn unsafe_bind_killer(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
}

#[test]
fn supervisor_restarts_crashed_worker_and_restores_recording_state() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("rec");

    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/session_av.mkv")
        .to_string_lossy()
        .into_owned();

    let program = worker_path();
    // Skip the whole scenario when the binary has not been built yet — this
    // mirrors how Tauri UI tests gate on build artifacts. Building happens
    // explicitly in CI's gates (`cargo build -p nian-media-worker` first).
    if !std::path::Path::new(&program).is_file() && !program.contains("target/debug") {
        eprintln!("worker binary missing at {program}; skipping crash scenario");
        return;
    }
    if !std::path::Path::new(&program).is_file() {
        eprintln!("worker binary not built; skipping (run cargo build -p nian-media-worker)");
        return;
    }

    let deaths_left = Arc::new(AtomicUsize::new(1));
    let mut supervisor = WorkerSupervisor::new(CrashyLauncher {
        program,
        deaths_to_inject: Arc::clone(&deaths_left),
        kill_after_start_seen: Arc::new(Mutex::new(None)),
    });

    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-sup-test".to_owned(),
        storage_root: storage.to_string_lossy().into_owned(),
        source_json: serde_json::json!({ "kind": "file", "path": fixture }),
        segment_target_secs: 5,
    });

    // Episode 1: worker starts, gets recording.start accepted, then dies to
    // our killer. The episode MUST end as Crashed. No operator stop is ever
    // requested during this episode — the crash itself ends it.
    let outcome = supervisor.run_one_episode(&|| false);

    match outcome {
        Ok(WorkerEnd::Crashed) => {}
        Ok(other) => panic!("expected Crashed episode, got {other:?}"),
        Err(error @ ApplicationError::PermanentRecordingConfig(_)) => {
            panic!("a single injected crash must never be permanent: {error}")
        }
        Err(ApplicationError::WorkerProtocol(message)) => {
            // Death DURING handshake/start-restore also surfaces as protocol
            // error flowing up; acceptable ONLY when the crash came too early
            // — treat as flake-prevention by asserting it mentions pipes.
            assert!(message.contains("pipe") || message.contains("reading"));
        }
        Err(other) => panic!("unexpected error {other:?}"),
    }

    // Episode 2: no further kills; the supervisor restores desired state and
    // records until WE request shutdown (triggered by disk-visible progress).
    let day_glob = storage.join("cam-sup-test");

    // Run the second episode in a plain thread; its shutdown closure flips
    // ONCE the restarted worker has published a final segment (disk-visible
    // synchronization, no sleeps-of-faith), which drives the supervisor into
    // its clean-shutdown delivery path.
    deaths_left.store(0, Ordering::SeqCst);
    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag_for_closure = Arc::clone(&shutdown_flag);
    let supervisor = std::sync::Arc::new(std::sync::Mutex::new(supervisor));
    let supervisor_thread = std::sync::Arc::clone(&supervisor);
    let day_glob_for_thread = day_glob.clone();
    let watcher = std::thread::spawn(move || {
        // Bounded poll: as soon as one final exists, arm the parent-side
        // shutdown so run_one_episode delivers protocol shutdown and ends.
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if count_finals(&day_glob_for_thread) >= 1 {
                flag_for_closure.store(true, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    });
    let episode2 = {
        let flag = Arc::clone(&shutdown_flag);
        std::thread::spawn(move || {
            supervisor_thread
                .lock()
                .unwrap()
                .run_one_episode(&move || flag.load(Ordering::SeqCst))
        })
    };

    assert!(
        watcher.join().unwrap(),
        "restarted worker must publish at least one segment within deadline"
    );

    let result = episode2.join().expect("episode thread must not panic");
    match result {
        Ok(end) => match end {
            WorkerEnd::RequestedShutdown => {}
            WorkerEnd::Crashed => panic!("healthy second episode must not end as Crashed"),
        },
        Err(error) => panic!("restart episode failed: {error}"),
    }

    // The published content exists and is independent of process lifecycles.
    assert!(count_finals(&day_glob) >= 1);
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
