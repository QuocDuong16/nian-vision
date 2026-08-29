//! Serialized one-shot camera probing through the media worker process.

use std::io::BufReader;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use nian_ipc::message::{Envelope, PROTOCOL_VERSION, event, method};
use nian_ipc::{FramedReader, FramedWriter};
use serde::Serialize;
use thiserror::Error;

use crate::PreparedProbe;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeResult {
    pub reachable: bool,
    pub video_stream_found: bool,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub audio_stream_count: u32,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ProbeError {
    #[error("another camera probe is already running")]
    Busy,
    #[error("media worker is unavailable")]
    WorkerUnavailable,
    #[error("camera source could not be opened")]
    SourceOpenFailed,
    #[error("camera source probe timed out")]
    SourceTimeout,
    #[error("camera source probe was cancelled")]
    Cancelled,
    #[error("camera source parameters are invalid")]
    InvalidSource,
    #[error("media worker protocol failed")]
    Protocol,
    #[error("camera source probe failed")]
    SourceProbeFailed,
}

pub trait ProbeRunner: Send + Sync {
    fn run(&self, request: PreparedProbe) -> Result<ProbeResult, ProbeError>;
}

#[derive(Debug, Clone)]
pub struct WorkerProbeRunner {
    pub worker_program: String,
}

impl ProbeRunner for WorkerProbeRunner {
    fn run(&self, request: PreparedProbe) -> Result<ProbeResult, ProbeError> {
        run_worker_probe(&self.worker_program, request)
    }
}

pub struct ProbeController {
    runner: Arc<dyn ProbeRunner>,
    gate: Mutex<()>,
}

impl std::fmt::Debug for ProbeController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeController").finish_non_exhaustive()
    }
}

impl ProbeController {
    pub fn new(runner: Arc<dyn ProbeRunner>) -> Self {
        Self {
            runner,
            gate: Mutex::new(()),
        }
    }

    pub fn probe(&self, request: PreparedProbe) -> Result<ProbeResult, ProbeError> {
        let _guard = self.gate.try_lock().map_err(|_| ProbeError::Busy)?;
        self.runner.run(request)
    }
}

enum ReaderMessage {
    Frame(Envelope),
    Eof,
    Error,
}

trait ProbeSetup: Send + Sync {
    fn spawn_reader(
        &self,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<std::thread::JoinHandle<()>, ProbeError>;

    fn send_request(&self, stdin: &mut ChildStdin, frame: &Envelope) -> Result<(), ProbeError>;

    fn on_reaped(&self, _pid: u32) {}
}

struct DefaultProbeSetup;

impl ProbeSetup for DefaultProbeSetup {
    fn spawn_reader(
        &self,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<std::thread::JoinHandle<()>, ProbeError> {
        std::thread::Builder::new()
            .name("camera-probe-worker-stdout".to_owned())
            .spawn(task)
            .map_err(|_| ProbeError::WorkerUnavailable)
    }

    fn send_request(&self, stdin: &mut ChildStdin, frame: &Envelope) -> Result<(), ProbeError> {
        FramedWriter::new(stdin)
            .send(frame)
            .map_err(|_| ProbeError::WorkerUnavailable)
    }
}

struct ProbeChildGuard<'a, S: ProbeSetup> {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    reader: Option<std::thread::JoinHandle<()>>,
    setup: &'a S,
    cleaned: bool,
}

impl<'a, S: ProbeSetup> ProbeChildGuard<'a, S> {
    fn new(mut child: Child, setup: &'a S) -> Self {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Self {
            child,
            stdin,
            stdout,
            reader: None,
            setup,
            cleaned: false,
        }
    }

    fn stdin_mut(&mut self) -> Result<&mut ChildStdin, ProbeError> {
        self.stdin.as_mut().ok_or(ProbeError::Protocol)
    }

    fn start_reader(&mut self, tx: mpsc::Sender<ReaderMessage>) -> Result<(), ProbeError> {
        let stdout = self.stdout.take().ok_or(ProbeError::Protocol)?;
        let task = Box::new(move || {
            let mut reader = FramedReader::new(BufReader::new(stdout));
            loop {
                match reader.next_message() {
                    Ok(Some(frame)) => {
                        if tx.send(ReaderMessage::Frame(frame)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(ReaderMessage::Eof);
                        return;
                    }
                    Err(_) => {
                        let _ = tx.send(ReaderMessage::Error);
                        return;
                    }
                }
            }
        });
        self.reader = Some(self.setup.spawn_reader(task)?);
        Ok(())
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let pid = self.child.id();

        if let Some(stdin) = self.stdin.as_mut() {
            let shutdown = Envelope::request(2, method::SHUTDOWN);
            let _ = FramedWriter::new(stdin).send(&shutdown);
        }
        // EOF is part of the graceful shutdown contract and also guarantees a
        // child that ignores the shutdown request cannot wait on our stdin.
        self.stdin.take();
        self.stdout.take();

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut reaped = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        if !reaped {
            let _ = self.child.kill();
            reaped = self.child.wait().is_ok();
        }
        if reaped {
            self.setup.on_reaped(pid);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl<S: ProbeSetup> Drop for ProbeChildGuard<'_, S> {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn run_worker_probe(program: &str, request: PreparedProbe) -> Result<ProbeResult, ProbeError> {
    run_worker_probe_with(program, request, &DefaultProbeSetup)
}

fn run_worker_probe_with<S: ProbeSetup>(
    program: &str,
    request: PreparedProbe,
    setup: &S,
) -> Result<ProbeResult, ProbeError> {
    let child = Command::new(program)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ProbeError::WorkerUnavailable)?;
    // From this point onward the guard is authoritative for bounded child
    // cleanup/reap on every return path, including setup/send failures.
    let mut worker = ProbeChildGuard::new(child, setup);
    if worker.stdin.is_none() || worker.stdout.is_none() {
        return Err(ProbeError::Protocol);
    }

    let (tx, rx) = mpsc::channel();
    worker.start_reader(tx)?;

    let hello_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = hello_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(ReaderMessage::Frame(Envelope::Event { v, name, .. })) if name == event::HELLO => {
                if v != PROTOCOL_VERSION {
                    return Err(ProbeError::Protocol);
                }
                break;
            }
            Ok(ReaderMessage::Frame(_)) => {}
            Ok(ReaderMessage::Eof | ReaderMessage::Error) | Err(_) => {
                return Err(ProbeError::WorkerUnavailable);
            }
        }
    }

    let frame = Envelope::Request {
        v: PROTOCOL_VERSION,
        id: 1,
        method: "camera.probe".to_owned(),
        params: serde_json::json!({
            "source": request.source_json,
            "timeout_ms": request.timeout_ms,
        }),
    };
    setup.send_request(worker.stdin_mut()?, &frame)?;

    let response_deadline =
        Instant::now() + Duration::from_millis(request.timeout_ms) + Duration::from_secs(2);
    let outcome = loop {
        let remaining = response_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(ReaderMessage::Frame(Envelope::Response {
                v,
                id: 1,
                ok: true,
                result,
                ..
            })) => {
                if v != PROTOCOL_VERSION {
                    break Err(ProbeError::Protocol);
                }
                break decode_probe_result(&result);
            }
            Ok(ReaderMessage::Frame(Envelope::Response {
                v,
                id: 1,
                ok: false,
                error_code,
                ..
            })) => {
                if v != PROTOCOL_VERSION {
                    break Err(ProbeError::Protocol);
                }
                break Err(map_probe_code(
                    error_code.as_deref().unwrap_or("source_probe_failed"),
                ));
            }
            Ok(ReaderMessage::Frame(_)) => {}
            Ok(ReaderMessage::Eof | ReaderMessage::Error) => {
                break Err(ProbeError::WorkerUnavailable);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break Err(ProbeError::SourceTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(ProbeError::WorkerUnavailable),
        }
    };

    worker.cleanup();
    outcome
}

fn decode_probe_result(value: &serde_json::Value) -> Result<ProbeResult, ProbeError> {
    Ok(ProbeResult {
        reachable: value
            .get("reachable")
            .and_then(serde_json::Value::as_bool)
            .ok_or(ProbeError::Protocol)?,
        video_stream_found: value
            .get("video_stream_found")
            .and_then(serde_json::Value::as_bool)
            .ok_or(ProbeError::Protocol)?,
        codec: value
            .get("codec")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        width: value
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        height: value
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        audio_stream_count: value
            .get("audio_stream_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(ProbeError::Protocol)?,
    })
}

fn map_probe_code(code: &str) -> ProbeError {
    let stable = code.split(':').next().unwrap_or(code);
    match stable {
        "source_open_failed" => ProbeError::SourceOpenFailed,
        "source_timeout" => ProbeError::SourceTimeout,
        "cancelled" => ProbeError::Cancelled,
        "invalid_params" => ProbeError::InvalidSource,
        "source_probe_failed" => ProbeError::SourceProbeFailed,
        _ => ProbeError::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Condvar, Mutex};

    use super::*;

    struct BlockingRunner {
        entered: Arc<Barrier>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ProbeRunner for BlockingRunner {
        fn run(&self, _request: PreparedProbe) -> Result<ProbeResult, ProbeError> {
            self.entered.wait();
            let (lock, condvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
            Ok(ProbeResult {
                reachable: true,
                video_stream_found: true,
                codec: Some("h264".to_owned()),
                width: Some(1920),
                height: Some(1080),
                audio_stream_count: 1,
            })
        }
    }

    fn request() -> PreparedProbe {
        PreparedProbe {
            camera_id: "front-door".to_owned(),
            source_json: serde_json::json!({"kind": "file", "path": "fixture.mkv"}),
            timeout_ms: 2_000,
        }
    }

    #[test]
    fn only_one_probe_runs_at_a_time() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let controller = Arc::new(ProbeController::new(Arc::new(BlockingRunner {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })));

        let first_controller = Arc::clone(&controller);
        let first = std::thread::spawn(move || first_controller.probe(request()));
        entered.wait();

        assert_eq!(controller.probe(request()), Err(ProbeError::Busy));

        let (lock, condvar) = &*release;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
        assert!(first.join().unwrap().is_ok());
    }

    #[cfg(unix)]
    struct TestSetup {
        fail_reader: bool,
        fail_send: bool,
        reaped: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[cfg(unix)]
    impl ProbeSetup for TestSetup {
        fn spawn_reader(
            &self,
            task: Box<dyn FnOnce() + Send + 'static>,
        ) -> Result<std::thread::JoinHandle<()>, ProbeError> {
            if self.fail_reader {
                Err(ProbeError::WorkerUnavailable)
            } else {
                DefaultProbeSetup.spawn_reader(task)
            }
        }

        fn send_request(&self, stdin: &mut ChildStdin, frame: &Envelope) -> Result<(), ProbeError> {
            if self.fail_send {
                Err(ProbeError::WorkerUnavailable)
            } else {
                DefaultProbeSetup.send_request(stdin, frame)
            }
        }

        fn on_reaped(&self, _pid: u32) {
            self.reaped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(unix)]
    fn probe_worker_script(hello: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe-worker.sh");
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{hello}'\ncat >/dev/null\n");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[cfg(unix)]
    const HELLO_OK: &str =
        r#"{"type":"event","v":1,"name":"hello","data":{"worker":"stub","protocol":1}}"#;

    #[cfg(unix)]
    #[test]
    fn reader_setup_failure_after_spawn_reaps_child() {
        let (_dir, program) = probe_worker_script(HELLO_OK);
        let reaped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let setup = TestSetup {
            fail_reader: true,
            fail_send: false,
            reaped: Arc::clone(&reaped),
        };

        assert_eq!(
            run_worker_probe_with(&program, request(), &setup),
            Err(ProbeError::WorkerUnavailable)
        );
        assert_eq!(reaped.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn initial_request_send_failure_after_spawn_reaps_child() {
        let (_dir, program) = probe_worker_script(HELLO_OK);
        let reaped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let setup = TestSetup {
            fail_reader: false,
            fail_send: true,
            reaped: Arc::clone(&reaped),
        };

        assert_eq!(
            run_worker_probe_with(&program, request(), &setup),
            Err(ProbeError::WorkerUnavailable)
        );
        assert_eq!(reaped.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn protocol_failure_after_spawn_reaps_child() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe-protocol-worker.sh");
        let malformed_result = r#"{"type":"response","v":1,"id":1,"ok":true,"result":{}}"#;
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' '{HELLO_OK}'\nIFS= read -r _request\nprintf '%s\\n' '{malformed_result}'\ncat >/dev/null\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let program = path.to_string_lossy().into_owned();
        let reaped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let setup = TestSetup {
            fail_reader: false,
            fail_send: false,
            reaped: Arc::clone(&reaped),
        };

        assert_eq!(
            run_worker_probe_with(&program, request(), &setup),
            Err(ProbeError::Protocol)
        );
        assert_eq!(reaped.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
