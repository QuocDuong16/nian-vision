//! Desktop-friendly single-recording controller layered over WorkerSupervisor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nian_domain::CameraId;
use serde::Serialize;
use thiserror::Error;

use crate::{ApplicationError, BinaryLauncher, DesiredRecording, WorkerEnd, WorkerSupervisor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Stopped,
    Starting,
    Recovering,
    Connecting,
    Recording,
    Backoff,
    Stopping,
    Failed,
}

impl RecordingState {
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Recovering
                | Self::Connecting
                | Self::Recording
                | Self::Backoff
                | Self::Stopping
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingStatus {
    pub state: RecordingState,
    pub camera_id: Option<String>,
    pub failure_category: Option<String>,
    pub reconnect_attempt: u32,
    pub finalized_segments: u64,
}

impl Default for RecordingStatus {
    fn default() -> Self {
        Self {
            state: RecordingState::Stopped,
            camera_id: None,
            failure_category: None,
            reconnect_attempt: 0,
            finalized_segments: 0,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordingControllerError {
    #[error("another camera is already recording")]
    AlreadyRecording,
    #[error("no recording is active")]
    NotRecording,
    #[error("recording controller synchronization failed")]
    Synchronization,
    #[error("recording controller thread could not start")]
    ThreadStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingRunFailure {
    Permanent { failure_category: Option<String> },
}

pub trait RecordingRunner: Send + Sync {
    fn run(
        &self,
        desired: DesiredRecording,
        stop: Arc<AtomicBool>,
        observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure>;
}

#[derive(Debug, Clone)]
pub struct SupervisorRecordingRunner {
    pub worker_program: String,
}

impl RecordingRunner for SupervisorRecordingRunner {
    fn run(
        &self,
        desired: DesiredRecording,
        stop: Arc<AtomicBool>,
        observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure> {
        let mut supervisor = WorkerSupervisor::new(BinaryLauncher {
            program: self.worker_program.clone(),
        });
        supervisor.set_desired_recording(desired);
        supervisor.set_status_observer(observer);
        supervisor
            .run_forever(&|| stop.load(Ordering::Acquire), &std::thread::sleep)
            .map_err(|error| RecordingRunFailure::Permanent {
                failure_category: failure_category(&error),
            })
    }
}

struct SharedStatus {
    status: Mutex<RecordingStatus>,
}

pub struct RecordingController {
    runner: Arc<dyn RecordingRunner>,
    shared: Arc<SharedStatus>,
    stop: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RecordingController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingController")
            .field("status", &self.status().ok())
            .finish_non_exhaustive()
    }
}

impl RecordingController {
    pub fn new(runner: Arc<dyn RecordingRunner>) -> Self {
        Self {
            runner,
            shared: Arc::new(SharedStatus {
                status: Mutex::new(RecordingStatus::default()),
            }),
            stop: None,
            thread: None,
        }
    }

    pub fn start(
        &mut self,
        camera_id: CameraId,
        desired: DesiredRecording,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished();
        if self.status()?.state.is_active() {
            return Err(RecordingControllerError::AlreadyRecording);
        }

        {
            let mut status = self
                .shared
                .status
                .lock()
                .map_err(|_| RecordingControllerError::Synchronization)?;
            *status = RecordingStatus {
                state: RecordingState::Starting,
                camera_id: Some(camera_id.as_str().to_owned()),
                failure_category: None,
                reconnect_attempt: 0,
                finalized_segments: 0,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        let runner = self.runner.clone();
        let shared = self.shared.clone();
        let observer_shared = shared.clone();
        let observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync> = Arc::new(move |payload| {
            apply_worker_status(&observer_shared, payload);
        });
        let thread = std::thread::Builder::new()
            .name(format!("recording-controller-{}", camera_id.as_str()))
            .spawn(move || {
                let result = runner.run(desired, stop, observer);
                if let Ok(mut status) = shared.status.lock() {
                    match result {
                        Ok(WorkerEnd::RequestedShutdown | WorkerEnd::JobCompletedCleanly) => {
                            status.state = RecordingState::Stopped;
                            status.failure_category = None;
                        }
                        Ok(WorkerEnd::RetryableEpisode) => {
                            // run_forever never returns RetryableEpisode, but keep a
                            // defensive stable outcome if a custom runner does.
                            status.state = RecordingState::Failed;
                            status.failure_category = None;
                        }
                        Err(RecordingRunFailure::Permanent { failure_category }) => {
                            status.state = RecordingState::Failed;
                            status.failure_category = failure_category;
                        }
                    }
                }
            })
            .map_err(|_| RecordingControllerError::ThreadStart)?;
        self.thread = Some(thread);
        self.status()
    }

    pub fn stop(&mut self) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished();
        let mut status = self
            .shared
            .status
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)?;
        if !status.state.is_active() || status.state == RecordingState::Stopping {
            return Err(RecordingControllerError::NotRecording);
        }
        status.state = RecordingState::Stopping;
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
        Ok(status.clone())
    }

    pub fn status(&self) -> Result<RecordingStatus, RecordingControllerError> {
        self.shared
            .status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| RecordingControllerError::Synchronization)
    }

    pub fn active_camera(&self) -> Result<Option<CameraId>, RecordingControllerError> {
        let status = self.status()?;
        if !status.state.is_active() {
            return Ok(None);
        }
        status
            .camera_id
            .as_deref()
            .map(CameraId::parse)
            .transpose()
            .map_err(|_| RecordingControllerError::Synchronization)
    }

    fn reap_finished(&mut self) {
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            self.stop = None;
        }
    }
}

impl Drop for RecordingController {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
        if let Some(thread) = self.thread.take() {
            // Normal desktop teardown asks the existing supervisor for graceful
            // shutdown through the stop closure, then waits for bounded M3
            // shutdown semantics to finish.
            let _ = thread.join();
        }
    }
}

fn apply_worker_status(shared: &SharedStatus, payload: &serde_json::Value) {
    let Some(worker_state) = payload.get("state").and_then(serde_json::Value::as_str) else {
        return;
    };
    let mapped = match worker_state {
        "recovering" => RecordingState::Recovering,
        "connecting" => RecordingState::Connecting,
        "recording" => RecordingState::Recording,
        "backoff" => RecordingState::Backoff,
        "stopped" | "completed" | "idle" => RecordingState::Stopped,
        "failed" => RecordingState::Failed,
        _ => return,
    };
    if let Ok(mut status) = shared.status.lock() {
        // Once a UI stop is requested, late worker progress must not visually
        // resurrect Recording while graceful teardown is still in flight.
        if status.state != RecordingState::Stopping {
            status.state = mapped;
        }
        if let Some(camera) = payload
            .get("camera_id")
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
        {
            status.camera_id = Some(camera.to_owned());
        }
        status.reconnect_attempt = payload
            .get("retry_attempt")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(status.reconnect_attempt);
        status.finalized_segments = payload
            .get("finalized_segments")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(status.finalized_segments);
        status.failure_category = payload
            .get("failure_category")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
}

fn failure_category(error: &ApplicationError) -> Option<String> {
    match error {
        ApplicationError::PermanentRecordingFailure { category } => Some(category.clone()),
        ApplicationError::PermanentRecordingConfig(_) => Some("permanent_configuration".to_owned()),
        ApplicationError::WorkerLaunch(_) | ApplicationError::WorkerProtocol(_) => {
            Some("worker_unavailable".to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_state_set_is_explicit() {
        assert!(RecordingState::Starting.is_active());
        assert!(RecordingState::Recording.is_active());
        assert!(RecordingState::Stopping.is_active());
        assert!(!RecordingState::Stopped.is_active());
        assert!(!RecordingState::Failed.is_active());
    }
}
