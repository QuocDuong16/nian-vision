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
    #[error("another recording controller run is still owned")]
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

/// Thread creation seam used to test start rollback without exhausting OS threads.
pub trait RecordingThreadSpawner: Send + Sync {
    fn spawn(
        &self,
        name: String,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<JoinHandle<()>>;
}

#[derive(Debug, Default)]
pub struct StdRecordingThreadSpawner;

impl RecordingThreadSpawner for StdRecordingThreadSpawner {
    fn spawn(
        &self,
        name: String,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new().name(name).spawn(task)
    }
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
    spawner: Arc<dyn RecordingThreadSpawner>,
    shared: Arc<SharedStatus>,
    stop: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RecordingController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self.shared.status.lock().ok().map(|status| status.clone());
        f.debug_struct("RecordingController")
            .field("status", &status)
            .field("thread_owned", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl RecordingController {
    pub fn new(runner: Arc<dyn RecordingRunner>) -> Self {
        Self::with_spawner(runner, Arc::new(StdRecordingThreadSpawner))
    }

    pub fn with_spawner(
        runner: Arc<dyn RecordingRunner>,
        spawner: Arc<dyn RecordingThreadSpawner>,
    ) -> Self {
        Self {
            runner,
            spawner,
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
        self.reap_finished()?;
        // JoinHandle ownership is authoritative. A terminal-looking status can
        // never authorize a second run while the previous supervision thread
        // still exists and has not been joined.
        if self.thread.is_some() {
            return Err(RecordingControllerError::AlreadyRecording);
        }

        self.replace_status(RecordingStatus {
            state: RecordingState::Starting,
            camera_id: Some(camera_id.as_str().to_owned()),
            failure_category: None,
            reconnect_attempt: 0,
            finalized_segments: 0,
        })?;

        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        let runner = self.runner.clone();
        let shared = self.shared.clone();
        let observer_shared = shared.clone();
        let observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync> = Arc::new(move |payload| {
            apply_worker_progress(&observer_shared, payload);
        });
        let name = format!("recording-controller-{}", camera_id.as_str());
        let task = Box::new(move || {
            // Only this finalizer publishes terminal Stopped/Failed, and only
            // after RecordingRunner::run has returned (including supervisor
            // worker cleanup/reap semantics).
            let result = runner.run(desired, stop, observer);
            if let Ok(mut status) = shared.status.lock() {
                match result {
                    Ok(WorkerEnd::RequestedShutdown | WorkerEnd::JobCompletedCleanly) => {
                        status.state = RecordingState::Stopped;
                        status.failure_category = None;
                    }
                    Ok(WorkerEnd::RetryableEpisode) => {
                        status.state = RecordingState::Failed;
                        status.failure_category = Some("worker_unavailable".to_owned());
                    }
                    Err(RecordingRunFailure::Permanent { failure_category }) => {
                        status.state = RecordingState::Failed;
                        status.failure_category = failure_category;
                    }
                }
            }
        });

        match self.spawner.spawn(name, task) {
            Ok(thread) => self.thread = Some(thread),
            Err(_) => {
                self.stop = None;
                self.replace_status(RecordingStatus {
                    state: RecordingState::Failed,
                    camera_id: None,
                    failure_category: Some("worker_unavailable".to_owned()),
                    reconnect_attempt: 0,
                    finalized_segments: 0,
                })?;
                return Err(RecordingControllerError::ThreadStart);
            }
        }
        self.status()
    }

    pub fn stop(&mut self) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished()?;
        if self.thread.is_none() {
            return Err(RecordingControllerError::NotRecording);
        }
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

    /// Lifecycle-side cooperative stop signal. Unlike the user Stop command this
    /// is idempotent: suspend and Quit may race an already-running teardown.
    pub fn request_lifecycle_stop(&mut self) -> Result<(), RecordingControllerError> {
        self.reap_finished()?;
        if self.thread.is_none() {
            return Ok(());
        }
        if let Ok(mut status) = self.shared.status.lock() {
            if status.state.is_active() {
                status.state = RecordingState::Stopping;
            }
        } else {
            return Err(RecordingControllerError::Synchronization);
        }
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Explicit desktop Quit owns the JoinHandle through terminal completion.
    /// WorkerSupervisor already provides the bounded graceful-then-force child
    /// teardown semantics, so this join does not detach a live recorder.
    pub fn shutdown(&mut self) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished()?;
        self.request_lifecycle_stop()?;
        if let Some(thread) = self.thread.take() {
            let join_result = thread.join();
            self.stop = None;
            if join_result.is_err() {
                self.replace_status(RecordingStatus {
                    state: RecordingState::Failed,
                    camera_id: None,
                    failure_category: Some("worker_unavailable".to_owned()),
                    reconnect_attempt: 0,
                    finalized_segments: 0,
                })?;
            }
        }
        self.status_snapshot()
    }

    /// Publishes a startup restoration failure without manufacturing a worker.
    /// Desired recording intent remains persisted independently.
    pub fn mark_failed(
        &mut self,
        camera_id: CameraId,
        failure_category: impl Into<String>,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished()?;
        if self.thread.is_some() {
            return Err(RecordingControllerError::AlreadyRecording);
        }
        let status = RecordingStatus {
            state: RecordingState::Failed,
            camera_id: Some(camera_id.as_str().to_owned()),
            failure_category: Some(failure_category.into()),
            reconnect_attempt: 0,
            finalized_segments: 0,
        };
        self.replace_status(status.clone())?;
        Ok(status)
    }

    /// Returns a status snapshot after reaping a finished runner thread.
    /// This makes runner panics observable to UI polling rather than leaving a
    /// stale Recording state forever.
    pub fn status(&mut self) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished()?;
        self.status_snapshot()
    }

    pub fn active_camera(&mut self) -> Result<Option<CameraId>, RecordingControllerError> {
        self.reap_finished()?;
        if self.thread.is_none() {
            return Ok(None);
        }
        let status = self.status_snapshot()?;
        status
            .camera_id
            .as_deref()
            .map(CameraId::parse)
            .transpose()
            .map_err(|_| RecordingControllerError::Synchronization)
    }

    fn status_snapshot(&self) -> Result<RecordingStatus, RecordingControllerError> {
        self.shared
            .status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| RecordingControllerError::Synchronization)
    }

    fn replace_status(&self, replacement: RecordingStatus) -> Result<(), RecordingControllerError> {
        *self
            .shared
            .status
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)? = replacement;
        Ok(())
    }

    fn reap_finished(&mut self) -> Result<(), RecordingControllerError> {
        let thread_finished = self.thread.as_ref().is_some_and(JoinHandle::is_finished);
        // The finalizer publishes Stopped/Failed only after the runner returns.
        // A tiny scheduler window still exists before the thread function itself
        // returns, so terminal status also proves that joining is now bounded.
        // This keeps terminal ownership cleanup independent of scheduler timing.
        let terminal_published = if self.thread.is_some() {
            matches!(
                self.status_snapshot()?.state,
                RecordingState::Stopped | RecordingState::Failed
            )
        } else {
            false
        };
        if !thread_finished && !terminal_published {
            return Ok(());
        }

        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        let join_result = thread.join();
        self.stop = None;
        if join_result.is_err() {
            self.replace_status(RecordingStatus {
                state: RecordingState::Failed,
                camera_id: None,
                failure_category: Some("worker_unavailable".to_owned()),
                reconnect_attempt: 0,
                finalized_segments: 0,
            })?;
        }
        Ok(())
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
            // shutdown semantics to finish. Runner panic must never panic host.
            let _ = thread.join();
        }
        self.stop = None;
    }
}

fn apply_worker_progress(shared: &SharedStatus, payload: &serde_json::Value) {
    let Some(worker_state) = payload.get("state").and_then(serde_json::Value::as_str) else {
        return;
    };
    let mapped = match worker_state {
        "recovering" => RecordingState::Recovering,
        "connecting" => RecordingState::Connecting,
        "recording" => RecordingState::Recording,
        "backoff" => RecordingState::Backoff,
        // Terminal worker observations are intentionally ignored. The runner
        // still owns bounded worker shutdown/kill/reap after seeing them.
        "stopped" | "completed" | "idle" | "failed" => return,
        _ => return,
    };
    if let Ok(mut status) = shared.status.lock() {
        // Once a UI stop is requested, late worker progress must not visually
        // resurrect Recording while graceful teardown is still in flight.
        if status.state != RecordingState::Stopping {
            status.state = mapped;
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
        status.failure_category = None;
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;

    struct FailFirstSpawner {
        calls: AtomicUsize,
    }

    impl RecordingThreadSpawner for FailFirstSpawner {
        fn spawn(
            &self,
            name: String,
            task: Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<JoinHandle<()>> {
            if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                Err(std::io::Error::other("injected thread spawn failure"))
            } else {
                std::thread::Builder::new().name(name).spawn(task)
            }
        }
    }

    struct ImmediateRunner;

    impl RecordingRunner for ImmediateRunner {
        fn run(
            &self,
            _desired: DesiredRecording,
            _stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<WorkerEnd, RecordingRunFailure> {
            Ok(WorkerEnd::JobCompletedCleanly)
        }
    }

    fn desired() -> DesiredRecording {
        DesiredRecording {
            camera: "cam-a".to_owned(),
            storage_root: "/tmp/nian-controller-test".to_owned(),
            source_json: serde_json::json!({"kind":"file","path":"fixture.mkv"}),
            segment_target_secs: 300,
            copy_audio: true,
        }
    }

    #[test]
    fn active_state_set_is_explicit() {
        assert!(RecordingState::Starting.is_active());
        assert!(RecordingState::Recording.is_active());
        assert!(RecordingState::Stopping.is_active());
        assert!(!RecordingState::Stopped.is_active());
        assert!(!RecordingState::Failed.is_active());
    }

    #[test]
    fn thread_spawn_failure_rolls_back_and_next_start_is_usable() {
        let spawner = Arc::new(FailFirstSpawner {
            calls: AtomicUsize::new(0),
        });
        let mut controller = RecordingController::with_spawner(Arc::new(ImmediateRunner), spawner);

        assert_eq!(
            controller
                .start(CameraId::parse("cam-a").unwrap(), desired())
                .unwrap_err(),
            RecordingControllerError::ThreadStart
        );
        let failed = controller.status().unwrap();
        assert_eq!(failed.state, RecordingState::Failed);
        assert_eq!(failed.camera_id, None);
        assert_eq!(
            failed.failure_category.as_deref(),
            Some("worker_unavailable")
        );

        controller
            .start(CameraId::parse("cam-a").unwrap(), desired())
            .unwrap();
        // Polling reaps the immediate successful run. Use a bounded wall-clock
        // deadline rather than a fixed yield count; the scheduler owes tests no
        // particular number of turns under the full parallel workspace suite.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if controller.status().unwrap().state == RecordingState::Stopped {
                return;
            }
            std::thread::yield_now();
        }
        panic!("second run did not finish");
    }
}
