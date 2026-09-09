//! Desktop-owned simultaneous multi-camera recording coordinator.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nian_domain::CameraId;
use serde::Serialize;
use thiserror::Error;

use crate::{ApplicationError, BinaryLauncher, DesiredRecording, WorkerEnd, WorkerSupervisor};

/// Conservative process cap for the current desktop architecture.
pub const MAX_SIMULTANEOUS_RECORDINGS: usize = 8;

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

impl RecordingStatus {
    fn stopped(camera_id: &CameraId) -> Self {
        Self {
            camera_id: Some(camera_id.as_str().to_owned()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordingControllerError {
    #[error("recording for this camera is still owned")]
    AlreadyRecording,
    #[error("camera is not recording")]
    NotRecording,
    #[error("recording capacity reached")]
    Capacity,
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

/// Creates an independent runner for one camera slot. Production returns a new
/// WorkerSupervisor-backed runner for every camera so unrelated recordings are
/// never serialized through one supervisor instance.
pub trait RecordingRunnerFactory: Send + Sync {
    fn create(&self, camera_id: &CameraId) -> Arc<dyn RecordingRunner>;
}

#[derive(Debug, Clone)]
pub struct SupervisorRecordingRunnerFactory {
    pub worker_program: String,
}

impl RecordingRunnerFactory for SupervisorRecordingRunnerFactory {
    fn create(&self, _camera_id: &CameraId) -> Arc<dyn RecordingRunner> {
        Arc::new(SupervisorRecordingRunner {
            worker_program: self.worker_program.clone(),
        })
    }
}

/// Compatibility factory useful for tests whose runner is explicitly designed
/// for concurrent calls. Production should use SupervisorRecordingRunnerFactory.
struct SharedRecordingRunnerFactory {
    runner: Arc<dyn RecordingRunner>,
}

impl RecordingRunnerFactory for SharedRecordingRunnerFactory {
    fn create(&self, _camera_id: &CameraId) -> Arc<dyn RecordingRunner> {
        self.runner.clone()
    }
}

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

type StatusObserver = Arc<dyn Fn(RecordingStatus) + Send + Sync>;

struct SlotShared {
    status: Mutex<RecordingStatus>,
    observer: Arc<Mutex<Option<StatusObserver>>>,
}

struct RecordingSlot {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<SlotShared>,
}

pub struct RecordingController {
    runner_factory: Arc<dyn RecordingRunnerFactory>,
    spawner: Arc<dyn RecordingThreadSpawner>,
    observer: Arc<Mutex<Option<StatusObserver>>>,
    slots: HashMap<CameraId, RecordingSlot>,
    terminal_statuses: HashMap<CameraId, RecordingStatus>,
}

impl std::fmt::Debug for RecordingController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingController")
            .field("slot_count", &self.slots.len())
            .field("owned_count", &self.owned_count())
            .finish_non_exhaustive()
    }
}

impl RecordingController {
    pub fn new(runner: Arc<dyn RecordingRunner>) -> Self {
        Self::with_factory(Arc::new(SharedRecordingRunnerFactory { runner }))
    }

    pub fn with_factory(factory: Arc<dyn RecordingRunnerFactory>) -> Self {
        Self::with_factory_and_spawner(factory, Arc::new(StdRecordingThreadSpawner))
    }

    pub fn with_spawner(
        runner: Arc<dyn RecordingRunner>,
        spawner: Arc<dyn RecordingThreadSpawner>,
    ) -> Self {
        Self::with_factory_and_spawner(Arc::new(SharedRecordingRunnerFactory { runner }), spawner)
    }

    pub fn with_factory_and_spawner(
        runner_factory: Arc<dyn RecordingRunnerFactory>,
        spawner: Arc<dyn RecordingThreadSpawner>,
    ) -> Self {
        Self {
            runner_factory,
            spawner,
            observer: Arc::new(Mutex::new(None)),
            slots: HashMap::new(),
            terminal_statuses: HashMap::new(),
        }
    }

    pub fn ensure_startable(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<(), RecordingControllerError> {
        self.reap_finished_all()?;
        if self.slots.contains_key(camera_id) {
            return Err(RecordingControllerError::AlreadyRecording);
        }
        if self.owned_count() >= MAX_SIMULTANEOUS_RECORDINGS {
            return Err(RecordingControllerError::Capacity);
        }
        Ok(())
    }

    pub fn start(
        &mut self,
        camera_id: CameraId,
        desired: DesiredRecording,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.ensure_startable(&camera_id)?;
        self.terminal_statuses.remove(&camera_id);
        let shared = Arc::new(SlotShared {
            status: Mutex::new(RecordingStatus {
                state: RecordingState::Starting,
                camera_id: Some(camera_id.as_str().to_owned()),
                failure_category: None,
                reconnect_attempt: 0,
                finalized_segments: 0,
            }),
            observer: self.observer.clone(),
        });
        notify_slot(&shared)?;

        let stop = Arc::new(AtomicBool::new(false));
        let runner = self.runner_factory.create(&camera_id);
        let thread_shared = shared.clone();
        let worker_shared = shared.clone();
        let worker_observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync> =
            Arc::new(move |payload| apply_worker_progress(&worker_shared, payload));
        let name = format!("recording-controller-{}", camera_id.as_str());
        let task_stop = stop.clone();
        let task = Box::new(move || {
            let result = runner.run(desired, task_stop, worker_observer);
            if let Ok(mut status) = thread_shared.status.lock() {
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
                let snapshot = status.clone();
                drop(status);
                notify_status_observer(&thread_shared, snapshot);
            }
        });

        let thread = match self.spawner.spawn(name, task) {
            Ok(thread) => thread,
            Err(_) => {
                let failed = RecordingStatus {
                    state: RecordingState::Failed,
                    camera_id: Some(camera_id.as_str().to_owned()),
                    failure_category: Some("worker_unavailable".to_owned()),
                    reconnect_attempt: 0,
                    finalized_segments: 0,
                };
                *shared
                    .status
                    .lock()
                    .map_err(|_| RecordingControllerError::Synchronization)? = failed.clone();
                notify_status_observer(&shared, failed.clone());
                self.terminal_statuses.insert(camera_id, failed);
                return Err(RecordingControllerError::ThreadStart);
            }
        };

        let status = shared
            .status
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)?
            .clone();
        self.slots.insert(
            camera_id,
            RecordingSlot {
                stop,
                thread: Some(thread),
                shared,
            },
        );
        Ok(status)
    }

    pub fn stop(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished(camera_id)?;
        let slot = self
            .slots
            .get_mut(camera_id)
            .ok_or(RecordingControllerError::NotRecording)?;
        let snapshot = {
            let mut status = slot
                .shared
                .status
                .lock()
                .map_err(|_| RecordingControllerError::Synchronization)?;
            if !status.state.is_active() || status.state == RecordingState::Stopping {
                return Err(RecordingControllerError::NotRecording);
            }
            status.state = RecordingState::Stopping;
            status.clone()
        };
        slot.stop.store(true, Ordering::Release);
        notify_status_observer(&slot.shared, snapshot.clone());
        Ok(snapshot)
    }

    /// Signals every owned slot before any join is attempted.
    pub fn request_lifecycle_stop_all(&mut self) -> Result<(), RecordingControllerError> {
        self.reap_finished_all()?;
        let ids = self.sorted_slot_ids();
        for camera_id in ids {
            let Some(slot) = self.slots.get_mut(&camera_id) else {
                continue;
            };
            let snapshot = {
                let mut status = slot
                    .shared
                    .status
                    .lock()
                    .map_err(|_| RecordingControllerError::Synchronization)?;
                let changed = status.state.is_active() && status.state != RecordingState::Stopping;
                if status.state.is_active() {
                    status.state = RecordingState::Stopping;
                }
                changed.then(|| status.clone())
            };
            slot.stop.store(true, Ordering::Release);
            if let Some(snapshot) = snapshot {
                notify_status_observer(&slot.shared, snapshot);
            }
        }
        Ok(())
    }

    /// Terminal teardown: signal all first, then join all owned slots.
    pub fn shutdown_all(&mut self) -> Result<Vec<RecordingStatus>, RecordingControllerError> {
        self.request_lifecycle_stop_all()?;
        let ids = self.sorted_slot_ids();
        for camera_id in ids {
            let mut slot = self
                .slots
                .remove(&camera_id)
                .ok_or(RecordingControllerError::Synchronization)?;
            let mut status = slot
                .shared
                .status
                .lock()
                .map_err(|_| RecordingControllerError::Synchronization)?
                .clone();
            if let Some(thread) = slot.thread.take()
                && thread.join().is_err()
            {
                status = RecordingStatus {
                    state: RecordingState::Failed,
                    camera_id: Some(camera_id.as_str().to_owned()),
                    failure_category: Some("worker_unavailable".to_owned()),
                    reconnect_attempt: 0,
                    finalized_segments: 0,
                };
                notify_status_observer(&slot.shared, status.clone());
            } else if let Ok(latest) = slot.shared.status.lock() {
                status = latest.clone();
            }
            self.terminal_statuses.insert(camera_id, status);
        }
        self.statuses()
    }

    /// Backward-compatible alias used by older lifecycle call sites while they
    /// are migrated to explicit multi-slot semantics.
    pub fn shutdown(&mut self) -> Result<RecordingStatus, RecordingControllerError> {
        let statuses = self.shutdown_all()?;
        Ok(statuses.into_iter().next().unwrap_or_default())
    }

    pub fn mark_failed(
        &mut self,
        camera_id: CameraId,
        failure_category: impl Into<String>,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished(&camera_id)?;
        if self.slots.contains_key(&camera_id) {
            return Err(RecordingControllerError::AlreadyRecording);
        }
        let status = RecordingStatus {
            state: RecordingState::Failed,
            camera_id: Some(camera_id.as_str().to_owned()),
            failure_category: Some(failure_category.into()),
            reconnect_attempt: 0,
            finalized_segments: 0,
        };
        self.terminal_statuses.insert(camera_id, status.clone());
        if let Some(observer) = self
            .observer
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)?
            .clone()
        {
            observer(status.clone());
        }
        Ok(status)
    }

    pub fn status(
        &mut self,
        camera_id: &CameraId,
    ) -> Result<RecordingStatus, RecordingControllerError> {
        self.reap_finished(camera_id)?;
        if let Some(slot) = self.slots.get(camera_id) {
            return slot
                .shared
                .status
                .lock()
                .map(|status| status.clone())
                .map_err(|_| RecordingControllerError::Synchronization);
        }
        Ok(self
            .terminal_statuses
            .get(camera_id)
            .cloned()
            .unwrap_or_else(|| RecordingStatus::stopped(camera_id)))
    }

    pub fn statuses(&mut self) -> Result<Vec<RecordingStatus>, RecordingControllerError> {
        self.reap_finished_all()?;
        let mut ids = self.sorted_status_ids();
        let mut result = Vec::with_capacity(ids.len());
        for camera_id in ids.drain(..) {
            if let Some(slot) = self.slots.get(&camera_id) {
                result.push(
                    slot.shared
                        .status
                        .lock()
                        .map_err(|_| RecordingControllerError::Synchronization)?
                        .clone(),
                );
            } else if let Some(status) = self.terminal_statuses.get(&camera_id) {
                result.push(status.clone());
            }
        }
        Ok(result)
    }

    pub fn active_cameras(&mut self) -> Result<Vec<CameraId>, RecordingControllerError> {
        self.reap_finished_all()?;
        Ok(self.sorted_slot_ids())
    }

    pub fn active_camera(&mut self) -> Result<Option<CameraId>, RecordingControllerError> {
        Ok(self.active_cameras()?.into_iter().next())
    }

    pub fn is_owned(&mut self, camera_id: &CameraId) -> Result<bool, RecordingControllerError> {
        self.reap_finished(camera_id)?;
        Ok(self.slots.contains_key(camera_id))
    }

    pub fn any_active(&mut self) -> Result<bool, RecordingControllerError> {
        self.reap_finished_all()?;
        Ok(!self.slots.is_empty())
    }

    pub fn forget_status(&mut self, camera_id: &CameraId) -> Result<(), RecordingControllerError> {
        self.reap_finished(camera_id)?;
        if self.slots.contains_key(camera_id) {
            return Err(RecordingControllerError::AlreadyRecording);
        }
        self.terminal_statuses.remove(camera_id);
        Ok(())
    }

    pub fn set_status_observer(
        &mut self,
        observer: Arc<dyn Fn(RecordingStatus) + Send + Sync>,
    ) -> Result<(), RecordingControllerError> {
        *self
            .observer
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)? = Some(observer.clone());
        for status in self.statuses()? {
            observer(status);
        }
        Ok(())
    }

    fn owned_count(&self) -> usize {
        self.slots.len()
    }

    fn sorted_slot_ids(&self) -> Vec<CameraId> {
        let mut ids: Vec<_> = self.slots.keys().cloned().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    fn sorted_status_ids(&self) -> Vec<CameraId> {
        let mut ids: Vec<_> = self
            .slots
            .keys()
            .chain(self.terminal_statuses.keys())
            .cloned()
            .collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.dedup();
        ids
    }

    fn reap_finished_all(&mut self) -> Result<(), RecordingControllerError> {
        let ids = self.sorted_slot_ids();
        for id in ids {
            self.reap_finished(&id)?;
        }
        Ok(())
    }

    fn reap_finished(&mut self, camera_id: &CameraId) -> Result<(), RecordingControllerError> {
        let should_join = match self.slots.get(camera_id) {
            Some(slot) => {
                let finished = slot.thread.as_ref().is_some_and(JoinHandle::is_finished);
                let terminal = matches!(
                    slot.shared
                        .status
                        .lock()
                        .map_err(|_| RecordingControllerError::Synchronization)?
                        .state,
                    RecordingState::Stopped | RecordingState::Failed
                );
                finished || terminal
            }
            None => false,
        };
        if !should_join {
            return Ok(());
        }
        let mut slot = self
            .slots
            .remove(camera_id)
            .ok_or(RecordingControllerError::Synchronization)?;
        let mut status = slot
            .shared
            .status
            .lock()
            .map_err(|_| RecordingControllerError::Synchronization)?
            .clone();
        if let Some(thread) = slot.thread.take()
            && thread.join().is_err()
        {
            status = RecordingStatus {
                state: RecordingState::Failed,
                camera_id: Some(camera_id.as_str().to_owned()),
                failure_category: Some("worker_unavailable".to_owned()),
                reconnect_attempt: 0,
                finalized_segments: 0,
            };
            notify_status_observer(&slot.shared, status.clone());
        } else if let Ok(latest) = slot.shared.status.lock() {
            status = latest.clone();
        }
        self.terminal_statuses.insert(camera_id.clone(), status);
        Ok(())
    }
}

impl Drop for RecordingController {
    fn drop(&mut self) {
        for slot in self.slots.values() {
            if slot.thread.is_some() {
                slot.stop.store(true, Ordering::Release);
            }
        }
        for slot in self.slots.values_mut() {
            if let Some(thread) = slot.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn notify_slot(shared: &SlotShared) -> Result<(), RecordingControllerError> {
    let snapshot = shared
        .status
        .lock()
        .map_err(|_| RecordingControllerError::Synchronization)?
        .clone();
    notify_status_observer(shared, snapshot);
    Ok(())
}

fn notify_status_observer(shared: &SlotShared, status: RecordingStatus) {
    let observer = shared
        .observer
        .lock()
        .ok()
        .and_then(|observer| observer.clone());
    if let Some(observer) = observer {
        observer(status);
    }
}

fn apply_worker_progress(shared: &SlotShared, payload: &serde_json::Value) {
    let Some(worker_state) = payload.get("state").and_then(serde_json::Value::as_str) else {
        return;
    };
    let mapped = match worker_state {
        "recovering" => RecordingState::Recovering,
        "connecting" => RecordingState::Connecting,
        "recording" => RecordingState::Recording,
        "backoff" => RecordingState::Backoff,
        "stopped" | "completed" | "idle" | "failed" => return,
        _ => return,
    };
    if let Ok(mut status) = shared.status.lock() {
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
        let snapshot = status.clone();
        drop(status);
        notify_status_observer(shared, snapshot);
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
    use std::time::{Duration, Instant};

    use super::*;

    struct BlockingRunner {
        starts: Arc<AtomicUsize>,
    }

    impl RecordingRunner for BlockingRunner {
        fn run(
            &self,
            _desired: DesiredRecording,
            stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<WorkerEnd, RecordingRunFailure> {
            self.starts.fetch_add(1, AtomicOrdering::SeqCst);
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(WorkerEnd::RequestedShutdown)
        }
    }

    struct Factory {
        starts: Arc<AtomicUsize>,
        creates: Arc<AtomicUsize>,
    }

    impl RecordingRunnerFactory for Factory {
        fn create(&self, _camera_id: &CameraId) -> Arc<dyn RecordingRunner> {
            self.creates.fetch_add(1, AtomicOrdering::SeqCst);
            Arc::new(BlockingRunner {
                starts: self.starts.clone(),
            })
        }
    }

    struct FailingRunner;

    impl RecordingRunner for FailingRunner {
        fn run(
            &self,
            _desired: DesiredRecording,
            _stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<WorkerEnd, RecordingRunFailure> {
            Err(RecordingRunFailure::Permanent {
                failure_category: Some("camera_in_use".to_owned()),
            })
        }
    }

    struct IsolatingFactory {
        starts: Arc<AtomicUsize>,
    }

    impl RecordingRunnerFactory for IsolatingFactory {
        fn create(&self, camera_id: &CameraId) -> Arc<dyn RecordingRunner> {
            if camera_id.as_str() == "cam-a" {
                Arc::new(FailingRunner)
            } else {
                Arc::new(BlockingRunner {
                    starts: self.starts.clone(),
                })
            }
        }
    }

    struct CoordinatedStopRunner {
        starts: Arc<AtomicUsize>,
        stop_observed: Arc<AtomicUsize>,
        expected_stops: usize,
        ordering_violated: Arc<AtomicBool>,
    }

    impl RecordingRunner for CoordinatedStopRunner {
        fn run(
            &self,
            _desired: DesiredRecording,
            stop: Arc<AtomicBool>,
            _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
        ) -> Result<WorkerEnd, RecordingRunFailure> {
            self.starts.fetch_add(1, AtomicOrdering::SeqCst);
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            self.stop_observed.fetch_add(1, AtomicOrdering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.stop_observed.load(AtomicOrdering::SeqCst) < self.expected_stops {
                if Instant::now() >= deadline {
                    self.ordering_violated.store(true, Ordering::Release);
                    break;
                }
                std::thread::yield_now();
            }
            Ok(WorkerEnd::RequestedShutdown)
        }
    }

    struct CoordinatedStopFactory {
        starts: Arc<AtomicUsize>,
        stop_observed: Arc<AtomicUsize>,
        expected_stops: usize,
        ordering_violated: Arc<AtomicBool>,
    }

    impl RecordingRunnerFactory for CoordinatedStopFactory {
        fn create(&self, _camera_id: &CameraId) -> Arc<dyn RecordingRunner> {
            Arc::new(CoordinatedStopRunner {
                starts: self.starts.clone(),
                stop_observed: self.stop_observed.clone(),
                expected_stops: self.expected_stops,
                ordering_violated: self.ordering_violated.clone(),
            })
        }
    }

    fn desired(camera: &str) -> DesiredRecording {
        DesiredRecording {
            camera: camera.to_owned(),
            storage_root: std::env::temp_dir()
                .join("nian-controller-test")
                .to_string_lossy()
                .into_owned(),
            source_json: serde_json::json!({"kind":"file","path":"fixture.mkv"}),
            segment_target_secs: 300,
            copy_audio: true,
        }
    }

    fn wait_for(value: &AtomicUsize, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if value.load(AtomicOrdering::SeqCst) >= expected {
                return;
            }
            std::thread::yield_now();
        }
        panic!("counter did not reach expected value");
    }

    #[test]
    fn two_cameras_own_independent_runners_and_stop_is_isolated() {
        let starts = Arc::new(AtomicUsize::new(0));
        let creates = Arc::new(AtomicUsize::new(0));
        let mut controller = RecordingController::with_factory(Arc::new(Factory {
            starts: starts.clone(),
            creates: creates.clone(),
        }));
        let a = CameraId::parse("cam-a").unwrap();
        let b = CameraId::parse("cam-b").unwrap();
        controller.start(a.clone(), desired("cam-a")).unwrap();
        controller.start(b.clone(), desired("cam-b")).unwrap();
        wait_for(&starts, 2);
        assert_eq!(creates.load(AtomicOrdering::SeqCst), 2);
        controller.stop(&a).unwrap();
        assert!(controller.is_owned(&b).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && controller.is_owned(&a).unwrap() {
            std::thread::yield_now();
        }
        assert!(!controller.is_owned(&a).unwrap());
        assert!(controller.is_owned(&b).unwrap());
        controller.shutdown_all().unwrap();
    }

    #[test]
    fn duplicate_start_is_camera_scoped() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut controller = RecordingController::with_factory(Arc::new(Factory {
            starts: starts.clone(),
            creates: Arc::new(AtomicUsize::new(0)),
        }));
        let a = CameraId::parse("cam-a").unwrap();
        let b = CameraId::parse("cam-b").unwrap();
        controller.start(a.clone(), desired("cam-a")).unwrap();
        assert_eq!(
            controller.start(a.clone(), desired("cam-a")).unwrap_err(),
            RecordingControllerError::AlreadyRecording
        );
        controller.start(b, desired("cam-b")).unwrap();
        controller.shutdown_all().unwrap();
    }

    #[test]
    fn shutdown_signals_all_before_joining() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stop_observed = Arc::new(AtomicUsize::new(0));
        let ordering_violated = Arc::new(AtomicBool::new(false));
        let mut controller = RecordingController::with_factory(Arc::new(CoordinatedStopFactory {
            starts: starts.clone(),
            stop_observed: stop_observed.clone(),
            expected_stops: 3,
            ordering_violated: ordering_violated.clone(),
        }));
        for name in ["cam-a", "cam-b", "cam-c"] {
            controller
                .start(CameraId::parse(name).unwrap(), desired(name))
                .unwrap();
        }
        wait_for(&starts, 3);
        controller.shutdown_all().unwrap();
        assert_eq!(stop_observed.load(AtomicOrdering::SeqCst), 3);
        assert!(!ordering_violated.load(Ordering::Acquire));
        assert!(controller.active_cameras().unwrap().is_empty());
    }

    #[test]
    fn finished_slot_is_removed_and_releases_capacity() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut controller = RecordingController::with_factory(Arc::new(Factory {
            starts: starts.clone(),
            creates: Arc::new(AtomicUsize::new(0)),
        }));
        let names = [
            "cam-a", "cam-b", "cam-c", "cam-d", "cam-e", "cam-f", "cam-g", "cam-h",
        ];
        assert_eq!(names.len(), MAX_SIMULTANEOUS_RECORDINGS);
        for name in names {
            controller
                .start(CameraId::parse(name).unwrap(), desired(name))
                .unwrap();
        }
        wait_for(&starts, MAX_SIMULTANEOUS_RECORDINGS);
        let overflow = CameraId::parse("cam-i").unwrap();
        assert_eq!(
            controller
                .start(overflow.clone(), desired("cam-i"))
                .unwrap_err(),
            RecordingControllerError::Capacity
        );

        let released = CameraId::parse("cam-a").unwrap();
        controller.stop(&released).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && controller.is_owned(&released).unwrap() {
            std::thread::yield_now();
        }
        assert!(!controller.is_owned(&released).unwrap());
        assert_eq!(
            controller.status(&released).unwrap().state,
            RecordingState::Stopped
        );

        controller
            .start(overflow.clone(), desired("cam-i"))
            .unwrap();
        wait_for(&starts, MAX_SIMULTANEOUS_RECORDINGS + 1);
        assert!(controller.is_owned(&overflow).unwrap());
        controller.shutdown_all().unwrap();
    }

    #[test]
    fn one_camera_failure_does_not_disturb_another_slot() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut controller = RecordingController::with_factory(Arc::new(IsolatingFactory {
            starts: starts.clone(),
        }));
        let a = CameraId::parse("cam-a").unwrap();
        let b = CameraId::parse("cam-b").unwrap();
        controller.start(a.clone(), desired("cam-a")).unwrap();
        controller.start(b.clone(), desired("cam-b")).unwrap();
        wait_for(&starts, 1);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && controller.status(&a).unwrap().state != RecordingState::Failed
        {
            std::thread::yield_now();
        }
        let failed = controller.status(&a).unwrap();
        assert_eq!(failed.state, RecordingState::Failed);
        assert_eq!(failed.failure_category.as_deref(), Some("camera_in_use"));
        assert!(!controller.is_owned(&a).unwrap());
        assert!(controller.is_owned(&b).unwrap());
        controller.shutdown_all().unwrap();
    }
}
