//! Serialized one-shot camera probing through the media worker process.

use std::io::BufReader;
use std::process::{Command, Stdio};
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

fn run_worker_probe(program: &str, request: PreparedProbe) -> Result<ProbeResult, ProbeError> {
    let mut child = Command::new(program)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ProbeError::WorkerUnavailable)?;
    let mut stdin = child.stdin.take().ok_or(ProbeError::Protocol)?;
    let stdout = child.stdout.take().ok_or(ProbeError::Protocol)?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("camera-probe-worker-stdout".to_owned())
        .spawn(move || {
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
        })
        .map_err(|_| ProbeError::WorkerUnavailable)?;

    let hello_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = hello_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(ReaderMessage::Frame(Envelope::Event { v, name, .. })) if name == event::HELLO => {
                if v != PROTOCOL_VERSION {
                    cleanup_child(&mut child, &mut stdin);
                    return Err(ProbeError::Protocol);
                }
                break;
            }
            Ok(ReaderMessage::Frame(_)) => {}
            Ok(ReaderMessage::Eof | ReaderMessage::Error) | Err(_) => {
                cleanup_child(&mut child, &mut stdin);
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
    FramedWriter::new(&mut stdin)
        .send(&frame)
        .map_err(|_| ProbeError::WorkerUnavailable)?;

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

    cleanup_child(&mut child, &mut stdin);
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

fn cleanup_child(child: &mut std::process::Child, stdin: &mut std::process::ChildStdin) {
    let shutdown = Envelope::request(2, method::SHUTDOWN);
    let _ = FramedWriter::new(stdin).send(&shutdown);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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
}
