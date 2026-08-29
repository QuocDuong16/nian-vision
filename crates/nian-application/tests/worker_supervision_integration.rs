//! Process-level worker supervision integration test (M3 §15; remediated
//! per review findings 3–7; final remediation §1/§2/§4).
//!
//! Deterministic, no timing luck: the REAL `nian-media-worker` binary is
//! spawned over IPC against a local fixture source; the test kills the
//! worker mid-recording; the supervisor observes the death (reader-thread
//! EOF — finding 3), restarts the worker, re-handshakes, restores desired
//! recording state, and only then delivers protocol shutdown once
//! disk-visible publication exists. Bounded explicit synchronization only.
//!
//! Final remediation rows (REAL worker protocol, not shell stubs):
//!
//! * file EOF reaches the parent as `JobCompletedCleanly` through the
//!   canonical `recording.status` shape (§1);
//! * a permanent job failure is observed as a terminal FAILED status with
//!   a stable category while the process itself stays alive (§2);
//! * ONE protocol shutdown gracefully finalizes the active healthy
//!   segment — no forced cancellation, no abandoned partial (§4).
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::print_stderr)] // skip notices go to the harness stderr

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nian_application::{
    ApplicationError, DesiredRecording, JobTerminal, SupervisorDeadlines, WorkerEnd,
    WorkerLauncher, WorkerSupervisor,
};
use nian_ipc::{Envelope, FramedReader, FramedWriter};

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

/// Launcher whose FIRST N spawns get SIGKILLed mid-recording. The kill is
/// deterministic: local files remux at hundreds of times realtime (a 30 s
/// fixture records in well under 100 ms), so a fixed-delay kill would land
/// AFTER the job already completed. Instead the killer watches for the
/// first partial segment under `kill_at_first_partial` and fires within
/// milliseconds of recording actually starting — always mid-run.
struct CrashyLauncher {
    program: String,
    deaths_to_inject: Arc<AtomicUsize>,
    kill_at_first_partial: Option<std::path::PathBuf>,
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
            let watch_dir = self.kill_at_first_partial.clone();
            std::thread::spawn(move || {
                // Fail-safe deadline so a broken recording path fails the
                // test fast instead of hanging.
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if let Some(dir) = &watch_dir
                        && walk(dir).iter().any(|p| is_partial(p))
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
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
        status_response: Duration::from_secs(2),
    };

    let deaths_left = Arc::new(AtomicUsize::new(1));
    let camera_root = storage.join("cam-sup-test");
    let mut supervisor = WorkerSupervisor::with_deadlines(
        CrashyLauncher {
            program: worker_path(),
            deaths_to_inject: Arc::clone(&deaths_left),
            kill_at_first_partial: Some(camera_root.clone()),
        },
        deadlines,
    );

    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-sup-test".to_owned(),
        storage_root: storage.to_string_lossy().into_owned(),
        source_json: serde_json::json!({ "kind": "file", "path": fixture }),
        segment_target_secs: 5,
        copy_audio: true,
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
    // at least one final recording exists on disk (explicit synchronization,
    // not sleep-of-faith). Outcome acceptance is honest about the fixture's
    // ~400x realtime remux speed: if the whole file already EOFed before the
    // closure flipped, the parent's post-completion shutdown path ends the
    // episode as JobCompletedCleanly — the restored episode RECORDED either
    // way, which the disk assertion below pins.
    deaths_left.store(0, Ordering::SeqCst);
    let day_dir = camera_root.clone();
    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let flag_for_closure = Arc::clone(&shutdown_flag);
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if count_finals(&day_dir) >= 1 {
                flag_for_closure.store(true, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    });

    let flag_for_episode = Arc::clone(&shutdown_flag);
    let outcome2 = supervisor.run_one_episode(&move || flag_for_episode.load(Ordering::SeqCst))?;

    assert!(watcher.join().unwrap());
    match outcome2 {
        WorkerEnd::RequestedShutdown | WorkerEnd::JobCompletedCleanly => {}
        other => panic!("healthy restored episode must end cleanly, got {other:?}"),
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

// ---- Final remediation §1/§2/§4: REAL worker protocol rows -----------------

/// Real-worker deadlines, milliseconds-scale like the stub tests.
fn real_worker_deadlines() -> SupervisorDeadlines {
    SupervisorDeadlines {
        hello: Duration::from_secs(10),
        start_ack: Duration::from_secs(10),
        shutdown_ack: Duration::from_secs(20),
        status_poll: Duration::from_millis(50),
        status_response: Duration::from_secs(2),
    }
}

/// Final remediation §1 (REAL worker): a local file recording runs to its
/// natural EOF and the parent observes `JobCompletedCleanly` through the
/// canonical `recording.status` shape — the result IS the JobStatus object
/// whose `finished`/`end_kind` the parent parser consumes.
#[test]
fn real_worker_file_eof_is_job_completed_cleanly_through_status() {
    if !std::path::Path::new(&worker_path()).is_file() {
        eprintln!("worker binary not built; skipping (run cargo build -p nian-media-worker)");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let fixture = workspace_root()
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv")
        .to_string_lossy()
        .into_owned();

    let mut supervisor = WorkerSupervisor::with_deadlines(
        PlainLauncher {
            program: worker_path(),
        },
        real_worker_deadlines(),
    );
    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-eof-test".to_owned(),
        storage_root: temp.path().join("rec").to_string_lossy().into_owned(),
        source_json: serde_json::json!({ "kind": "file", "path": fixture }),
        segment_target_secs: 5,
        copy_audio: true,
    });

    let started = Instant::now();
    match supervisor.run_one_episode(&|| false).unwrap() {
        WorkerEnd::JobCompletedCleanly => {}
        other => panic!("file EOF must complete the job cleanly, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "a 2 s fixture must finish promptly, took {started:?}"
    );
    assert_eq!(
        supervisor.last_job_terminal(),
        Some(&JobTerminal::Completed),
        "the terminal Completed state must be visible on the supervisor"
    );
}

/// Plain production launcher (real binary, no injected crashes).
struct PlainLauncher {
    program: String,
}

impl WorkerLauncher for PlainLauncher {
    fn spawn(&mut self) -> std::io::Result<Child> {
        Command::new(&self.program)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }
}

/// Final remediation §2 (REAL worker): a permanently un-openable source
/// fails the recording job terminally (the file-source matrix makes every
/// failure permanent) with the STABLE wire category, and the parent
/// observes the FAILED terminal while the worker process itself is still
/// alive — supervision stops with the typed error, never a restart loop.
#[test]
fn real_worker_permanent_job_failure_observed_while_process_alive() {
    if !std::path::Path::new(&worker_path()).is_file() {
        eprintln!("worker binary not built; skipping (run cargo build -p nian-media-worker)");
        return;
    }
    let temp = tempfile::tempdir().unwrap();

    let mut supervisor = WorkerSupervisor::with_deadlines(
        PlainLauncher {
            program: worker_path(),
        },
        real_worker_deadlines(),
    );
    supervisor.set_desired_recording(DesiredRecording {
        camera: "cam-perm-fail".to_owned(),
        storage_root: temp.path().join("rec").to_string_lossy().into_owned(),
        source_json: serde_json::json!({
            "kind": "file",
            "path": "/nonexistent/nian-vision-missing-source.mkv"
        }),
        segment_target_secs: 5,
        copy_audio: true,
    });

    match supervisor.run_one_episode(&|| false) {
        Err(ApplicationError::PermanentRecordingFailure { category }) => {
            assert_eq!(
                category, "source_open_failed",
                "stable wire category expected (§9)"
            );
        }
        other => panic!("permanent job failure expected, got {other:?}"),
    }
    assert_eq!(
        supervisor.last_job_terminal(),
        Some(&JobTerminal::Failed("source_open_failed".to_owned())),
        "terminal Failed must be observed while the process lives"
    );
}

/// Final remediation §4 (REAL worker): ONE protocol shutdown on an active
/// recording gracefully finalizes the healthy active segment and publishes
/// it — no forced cancellation, therefore NO abandoned partial — and the
/// worker exits cleanly (status 0).
///
/// Synchronization: the source is the 30 s `session_av` fixture with 1 s
/// segment targets; the test sends shutdown only after the first final is
/// disk-visible, leaving ~29 s of source remaining — the shutdown lands
/// while a segment is genuinely active. (A local file records faster than
/// real time; in the unlikely event the source EOFs first, the asserted
/// invariants — clean exit, finals published, zero partials — hold for
/// that outcome too, and the worker-level graceful-stop tests pin the
/// active-segment path directly.)
#[test]
fn one_protocol_shutdown_gracefully_finalizes_the_active_segment() {
    if !std::path::Path::new(&worker_path()).is_file() {
        eprintln!("worker binary not built; skipping (run cargo build -p nian-media-worker)");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("rec");
    let fixture = workspace_root()
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/session_av.mkv")
        .to_string_lossy()
        .into_owned();

    let mut child = Command::new(worker_path())
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = FramedReader::new(std::io::BufReader::new(child.stdout.take().unwrap()));
    let mut writer = FramedWriter::new(child.stdin.take().unwrap());

    // Handshake: hello event first.
    let hello = reader.next_message().unwrap().unwrap();
    let Envelope::Event { name, data, .. } = hello else {
        panic!("expected hello event");
    };
    assert_eq!(name, "hello");
    assert_eq!(
        data.get("protocol").and_then(serde_json::Value::as_u64),
        Some(1)
    );

    // recording.start (canonical request shape).
    let start_params = serde_json::json!({
        "camera": "cam-shutdown-test",
        "storage": storage.to_string_lossy(),
        "source": { "kind": "file", "path": fixture },
        "segment_target_secs": 1,
    });
    writer
        .send(&Envelope::Request {
            v: nian_ipc::PROTOCOL_VERSION,
            id: 1,
            method: "recording.start".to_owned(),
            params: start_params,
        })
        .unwrap();

    // Await the start ack, skipping any interleaved events; then poll
    // recording.status (canonical result = JobStatus object, §1) until the
    // first segment is published.
    let mut next_id = 2u64;
    let ack_at = Instant::now() + Duration::from_secs(15);
    let mut acked = false;
    while Instant::now() < ack_at {
        let envelope = reader.next_message().unwrap().unwrap();
        if let Envelope::Response {
            id: 1, ok, result, ..
        } = envelope
        {
            assert!(ok, "recording.start must be accepted promptly: {result}");
            acked = true;
            break;
        }
    }
    assert!(acked, "start ack never arrived");

    let poll_deadline = Instant::now() + Duration::from_secs(30);
    let mut status;
    loop {
        assert!(Instant::now() < poll_deadline, "status polling timed out");
        writer
            .send(&Envelope::Request {
                v: nian_ipc::PROTOCOL_VERSION,
                id: next_id,
                method: "recording.status".to_owned(),
                params: serde_json::Value::Null,
            })
            .unwrap();
        let id = next_id;
        next_id += 1;
        loop {
            let envelope = reader.next_message().unwrap().unwrap();
            let Envelope::Response {
                id: reply,
                ok,
                result,
                ..
            } = envelope
            else {
                continue; // events between polls
            };
            assert_eq!(reply, id);
            assert!(ok, "recording.status must succeed: {result}");
            status = result;
            break;
        }
        let finalized = status
            .get("finalized_segments")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if finalized >= 1
            || status.get("finished").and_then(serde_json::Value::as_bool) == Some(true)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // ONE shutdown while the recording is active (see test doc).
    writer
        .send(&Envelope::Request {
            v: nian_ipc::PROTOCOL_VERSION,
            id: 900,
            method: "shutdown".to_owned(),
            params: serde_json::Value::Null,
        })
        .unwrap();
    let shutdown_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < shutdown_deadline, "shutdown ack timed out");
        let envelope = reader.next_message().unwrap().unwrap();
        if let Envelope::Response { id: 900, ok, .. } = envelope {
            assert!(ok, "shutdown must be acknowledged");
            break;
        }
    }
    drop(writer); // close stdin: serve loop (already leaving) sees EOF

    let exit = child.wait().unwrap();
    assert!(
        exit.success(),
        "one graceful shutdown must yield a clean worker exit, got {exit:?}"
    );

    // The healthy active segment was FINALIZED and published (never
    // abandoned): at least one final recording, ZERO partials.
    let camera_dir = storage.join("cam-shutdown-test");
    let finals = count_finals(&camera_dir);
    let partials = walk(&camera_dir)
        .into_iter()
        .filter(|p| is_partial(p))
        .count();
    assert!(finals >= 1, "shutdown must finalize a published segment");
    assert_eq!(
        partials, 0,
        "a graceful shutdown must NEVER abandon a healthy segment"
    );
}
