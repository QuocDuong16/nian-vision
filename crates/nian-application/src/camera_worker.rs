//! Per-camera media-worker broker.
//!
//! One camera owns at most one `nian-media-worker` process in the desktop
//! application. Recording and live-view clients share that process, allowing
//! the worker-side ingest registry to fan one RTSP profile out to multiple
//! consumers without copying media through the desktop process.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nian_domain::CameraId;
use nian_ipc::message::{Envelope, PROTOCOL_VERSION, event, method};
use nian_ipc::{FramedReader, FramedWriter};
use serde::{Deserialize, Serialize};

const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const PRE_ROLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, thiserror::Error)]
pub enum CameraWorkerError {
    #[error("media worker could not be started")]
    Spawn,
    #[error("media worker protocol failed")]
    Protocol,
    #[error("media worker became unavailable")]
    Unavailable,
    #[error("media worker request timed out")]
    Timeout,
    #[error("media worker request was cancelled")]
    Cancelled,
    #[error("media worker refused request with {0}")]
    Rpc(String),
    #[error("media worker synchronization failed")]
    Synchronization,
}

/// A bounded motion transition produced from the shared sub/main ingest.
/// No source URL, image, or camera credential crosses this DTO.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraMotionTransition {
    pub sequence: u64,
    pub motion_active: bool,
    pub observed_at_utc: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraMotionStatus {
    pub state: String,
    pub motion_active: Option<bool>,
    pub sampled_frames: u64,
    pub dropped_packets: u64,
    pub reconnect_attempt: usize,
    pub last_error_code: Option<String>,
    pub transitions: Vec<CameraMotionTransition>,
}

/// Active packet queues by purpose; no camera URL or credential is retained.
#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct CameraMediaConsumerCounts {
    pub recording: usize,
    pub live: usize,
    pub motion: usize,
    pub event: usize,
    pub other: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CameraMediaSourceDiagnostics {
    pub profile: String,
    pub lifecycle: String,
    pub retainers: usize,
    pub subscribers: usize,
    pub reliable_subscribers: usize,
    pub realtime_subscribers: usize,
    pub consumers: CameraMediaConsumerCounts,
    pub queued_packets: usize,
    pub queued_bytes: usize,
    pub dropped_packets: u64,
    pub pre_roll_packets: usize,
    pub pre_roll_bytes: usize,
    pub pre_roll_dropped_packets: u64,
    pub video_frame_rate: Option<nian_domain::MediaRational>,
    pub video_codec: Option<String>,
    pub video_width: Option<u32>,
    pub video_height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CameraMediaDiagnostics {
    pub camera_id: String,
    pub worker_available: bool,
    pub generation_starts: u64,
    pub source_count: usize,
    pub main_sources: usize,
    pub sub_sources: usize,
    pub connecting_sources: usize,
    pub ready_sources: usize,
    pub failed_sources: usize,
    pub subscribers: usize,
    pub reliable_subscribers: usize,
    pub realtime_subscribers: usize,
    pub consumers: CameraMediaConsumerCounts,
    pub queued_packets: usize,
    pub queued_bytes: usize,
    pub dropped_packets: u64,
    pub pre_roll_packets: usize,
    pub pre_roll_bytes: usize,
    pub pre_roll_dropped_packets: u64,
    pub sources: Vec<CameraMediaSourceDiagnostics>,
}

#[derive(Debug, Deserialize)]
struct WorkerMediaSourceStatus {
    profile: String,
    lifecycle: String,
    retainers: usize,
    subscribers: usize,
    reliable_subscribers: usize,
    realtime_subscribers: usize,
    #[serde(default)]
    consumers: CameraMediaConsumerCounts,
    queued_packets: usize,
    queued_bytes: usize,
    dropped_packets: u64,
    pre_roll_packets: usize,
    pre_roll_bytes: usize,
    pre_roll_dropped_packets: u64,
    #[serde(default)]
    video_frame_rate: Option<nian_domain::MediaRational>,
    video_codec: Option<String>,
    video_width: Option<u32>,
    video_height: Option<u32>,
}

impl From<WorkerMediaSourceStatus> for CameraMediaSourceDiagnostics {
    fn from(source: WorkerMediaSourceStatus) -> Self {
        Self {
            profile: source.profile,
            lifecycle: source.lifecycle,
            retainers: source.retainers,
            subscribers: source.subscribers,
            reliable_subscribers: source.reliable_subscribers,
            realtime_subscribers: source.realtime_subscribers,
            consumers: source.consumers,
            queued_packets: source.queued_packets,
            queued_bytes: source.queued_bytes,
            dropped_packets: source.dropped_packets,
            pre_roll_packets: source.pre_roll_packets,
            pre_roll_bytes: source.pre_roll_bytes,
            pre_roll_dropped_packets: source.pre_roll_dropped_packets,
            video_frame_rate: source.video_frame_rate,
            video_codec: source.video_codec,
            video_width: source.video_width,
            video_height: source.video_height,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkerMediaStatus {
    generation_starts: u64,
    source_count: usize,
    main_sources: usize,
    sub_sources: usize,
    connecting_sources: usize,
    ready_sources: usize,
    failed_sources: usize,
    subscribers: usize,
    reliable_subscribers: usize,
    realtime_subscribers: usize,
    #[serde(default)]
    consumers: CameraMediaConsumerCounts,
    queued_packets: usize,
    queued_bytes: usize,
    dropped_packets: u64,
    pre_roll_packets: usize,
    pre_roll_bytes: usize,
    pre_roll_dropped_packets: u64,
    #[serde(default)]
    sources: Vec<WorkerMediaSourceStatus>,
}

enum ReaderMessage {
    Frame(Envelope),
    Eof,
    Error,
}

struct RpcState {
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<ReaderMessage>,
    next_request_id: u64,
}

pub(crate) struct CameraWorkerProcess {
    child: Mutex<Child>,
    rpc: Mutex<RpcState>,
    reader: Mutex<Option<JoinHandle<()>>>,
    shutdown_started: AtomicBool,
}

impl CameraWorkerProcess {
    fn spawn(program: &str) -> Result<Arc<Self>, CameraWorkerError> {
        let mut command = Command::new(program);
        command
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::worker_process::spawn_worker(&mut command)
            .map_err(|_| CameraWorkerError::Spawn)?;
        let stdin = child.stdin.take().ok_or(CameraWorkerError::Protocol)?;
        let stdout = child.stdout.take().ok_or(CameraWorkerError::Protocol)?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("camera-worker-stdout".to_owned())
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
            .map_err(|_| CameraWorkerError::Spawn)?;

        let process = Arc::new(Self {
            child: Mutex::new(child),
            rpc: Mutex::new(RpcState {
                stdin: Some(stdin),
                rx,
                next_request_id: 1,
            }),
            reader: Mutex::new(Some(reader)),
            shutdown_started: AtomicBool::new(false),
        });
        if let Err(error) = process.wait_hello() {
            process.force_reap();
            return Err(error);
        }
        Ok(process)
    }

    fn wait_hello(&self) -> Result<(), CameraWorkerError> {
        let deadline = Instant::now() + HELLO_TIMEOUT;
        let rpc = self
            .rpc
            .lock()
            .map_err(|_| CameraWorkerError::Synchronization)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CameraWorkerError::Timeout);
            }
            match rpc
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(ReaderMessage::Frame(Envelope::Event { v, name, data }))
                    if v == PROTOCOL_VERSION
                        && name == event::HELLO
                        && nian_ipc::validate_worker_hello(&data).is_ok() =>
                {
                    return Ok(());
                }
                Ok(ReaderMessage::Frame(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(ReaderMessage::Eof | ReaderMessage::Error)
                | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(CameraWorkerError::Unavailable);
                }
            }
        }
    }

    pub(crate) fn request(
        &self,
        method_name: &str,
        params: serde_json::Value,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<serde_json::Value, CameraWorkerError> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            return Err(CameraWorkerError::Cancelled);
        }
        let mut rpc = self
            .rpc
            .lock()
            .map_err(|_| CameraWorkerError::Synchronization)?;
        let id = rpc.next_request_id;
        rpc.next_request_id = rpc.next_request_id.saturating_add(1);
        let stdin = rpc.stdin.as_mut().ok_or(CameraWorkerError::Unavailable)?;
        FramedWriter::new(stdin)
            .send(&Envelope::Request {
                v: PROTOCOL_VERSION,
                id,
                method: method_name.to_owned(),
                params,
            })
            .map_err(|_| CameraWorkerError::Unavailable)?;

        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                return Err(CameraWorkerError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CameraWorkerError::Timeout);
            }
            match rpc
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(ReaderMessage::Frame(Envelope::Response {
                    v,
                    id: response_id,
                    ok: true,
                    result,
                    ..
                })) if v == PROTOCOL_VERSION && response_id == id => return Ok(result),
                Ok(ReaderMessage::Frame(Envelope::Response {
                    v,
                    id: response_id,
                    ok: false,
                    error_code,
                    ..
                })) if v == PROTOCOL_VERSION && response_id == id => {
                    return Err(CameraWorkerError::Rpc(
                        error_code.unwrap_or_else(|| "internal".to_owned()),
                    ));
                }
                Ok(ReaderMessage::Frame(Envelope::Event { .. })) => {}
                Ok(ReaderMessage::Frame(Envelope::Response { .. })) => {
                    return Err(CameraWorkerError::Protocol);
                }
                Ok(ReaderMessage::Frame(_)) => return Err(CameraWorkerError::Protocol),
                Ok(ReaderMessage::Eof | ReaderMessage::Error)
                | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(CameraWorkerError::Unavailable);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    pub(crate) fn request_default(
        &self,
        method_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CameraWorkerError> {
        self.request(method_name, params, REQUEST_TIMEOUT, None)
    }

    fn is_running(&self) -> bool {
        self.child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok())
            .is_some_and(|status| status.is_none())
    }

    fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.request(
            method::SHUTDOWN,
            serde_json::json!({}),
            SHUTDOWN_TIMEOUT,
            None,
        );
        if let Ok(mut rpc) = self.rpc.lock() {
            rpc.stdin.take();
        }
        self.reap_with_timeout(SHUTDOWN_TIMEOUT);
        if let Ok(mut reader) = self.reader.lock()
            && let Some(reader) = reader.take()
        {
            let _ = reader.join();
        }
    }

    fn reap_with_timeout(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        if let Ok(mut child) = self.child.lock() {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < deadline => {
                        drop(child);
                        std::thread::sleep(Duration::from_millis(20));
                        let Ok(next) = self.child.lock() else {
                            return;
                        };
                        child = next;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn force_reap(&self) {
        if let Ok(mut rpc) = self.rpc.lock() {
            rpc.stdin.take();
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(mut reader) = self.reader.lock()
            && let Some(reader) = reader.take()
        {
            let _ = reader.join();
        }
    }
}

impl Drop for CameraWorkerProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct CameraWorkerBroker {
    worker_program: String,
    slots: Arc<Mutex<HashMap<CameraId, Arc<CameraWorkerSlot>>>>,
}

pub struct CameraPreRollLease {
    broker: CameraWorkerBroker,
    camera_id: CameraId,
    worker: Arc<CameraWorkerProcess>,
    stopped: bool,
}

/// Keeps a single local motion consumer alive inside its existing camera
/// worker. The shared ingest owns the RTSP connection. Drop stops decoding.
pub struct CameraMotionLease {
    broker: CameraWorkerBroker,
    camera_id: CameraId,
    worker: Arc<CameraWorkerProcess>,
    stopped: bool,
}

impl CameraMotionLease {
    pub fn status(&self) -> Result<CameraMotionStatus, CameraWorkerError> {
        if self.stopped {
            return Err(CameraWorkerError::Cancelled);
        }
        let value = self.worker.request(
            "motion.status",
            serde_json::json!({}),
            Duration::from_millis(750),
            None,
        )?;
        serde_json::from_value(value).map_err(|_| CameraWorkerError::Protocol)
    }

    /// Call only after persisting every transition up to `sequence`.
    pub fn acknowledge(&self, sequence: u64) -> Result<(), CameraWorkerError> {
        if self.stopped {
            return Err(CameraWorkerError::Cancelled);
        }
        self.worker
            .request_default("motion.ack", serde_json::json!({"sequence": sequence}))?;
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        let result = self.worker.request(
            "motion.stop",
            serde_json::json!({}),
            Duration::from_secs(3),
            None,
        );
        if matches!(
            result,
            Err(CameraWorkerError::Unavailable | CameraWorkerError::Protocol)
        ) {
            self.broker.invalidate(&self.camera_id, &self.worker);
        }
        self.stopped = true;
    }
}

impl Drop for CameraMotionLease {
    fn drop(&mut self) {
        self.stop();
    }
}

impl CameraPreRollLease {
    pub fn is_healthy(&self) -> bool {
        if self.stopped || !self.worker.is_running() {
            return false;
        }
        self.worker
            .request(
                "pre_roll.status",
                serde_json::json!({}),
                Duration::from_millis(750),
                None,
            )
            .ok()
            .is_some_and(|status| {
                status.get("active").and_then(serde_json::Value::as_bool) == Some(true)
                    && status.get("ready").and_then(serde_json::Value::as_bool) == Some(true)
            })
    }

    fn release(&mut self) {
        if self.stopped {
            return;
        }
        let result = self.worker.request(
            "pre_roll.stop",
            serde_json::json!({}),
            Duration::from_secs(3),
            None,
        );
        if matches!(
            result,
            Err(CameraWorkerError::Unavailable | CameraWorkerError::Protocol)
        ) {
            self.broker.invalidate(&self.camera_id, &self.worker);
        }
        self.stopped = true;
    }
}

impl Drop for CameraPreRollLease {
    fn drop(&mut self) {
        self.release();
    }
}

struct SingleFlightSlot<T> {
    state: Mutex<SingleFlightSlotState<T>>,
    changed: Condvar,
}

impl<T> Default for SingleFlightSlot<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(SingleFlightSlotState::Vacant),
            changed: Condvar::new(),
        }
    }
}

enum SingleFlightSlotState<T> {
    Vacant,
    Opening,
    Ready(Weak<T>),
}

impl<T> SingleFlightSlot<T> {
    fn acquire_with<E>(
        &self,
        is_usable: impl Fn(&Arc<T>) -> bool,
        open: impl Fn() -> Result<Arc<T>, E>,
    ) -> Result<Arc<T>, E> {
        loop {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*state {
                SingleFlightSlotState::Ready(existing) => {
                    if let Some(existing) = existing.upgrade()
                        && is_usable(&existing)
                    {
                        return Ok(existing);
                    }
                    *state = SingleFlightSlotState::Vacant;
                }
                SingleFlightSlotState::Opening => {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(state);
                }
                SingleFlightSlotState::Vacant => {
                    *state = SingleFlightSlotState::Opening;
                    drop(state);
                    let opened = open();
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match opened {
                        Ok(worker) => {
                            *state = SingleFlightSlotState::Ready(Arc::downgrade(&worker));
                            self.changed.notify_all();
                            return Ok(worker);
                        }
                        Err(error) => {
                            *state = SingleFlightSlotState::Vacant;
                            self.changed.notify_all();
                            return Err(error);
                        }
                    }
                }
            }
        }
    }

    fn current(&self) -> Option<Arc<T>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            SingleFlightSlotState::Ready(current) => current.upgrade(),
            SingleFlightSlotState::Vacant | SingleFlightSlotState::Opening => None,
        }
    }

    fn vacate_if(&self, candidate: &Arc<T>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            &*state,
            SingleFlightSlotState::Ready(current)
                if current
                    .upgrade()
                    .is_some_and(|current| Arc::ptr_eq(&current, candidate))
        ) {
            *state = SingleFlightSlotState::Vacant;
            self.changed.notify_all();
            return true;
        }
        false
    }
}

type CameraWorkerSlot = SingleFlightSlot<CameraWorkerProcess>;

impl CameraWorkerBroker {
    pub fn new(worker_program: String) -> Self {
        Self {
            worker_program,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn retain_local_motion(
        &self,
        camera_id: &CameraId,
        source_json: serde_json::Value,
    ) -> Result<CameraMotionLease, CameraWorkerError> {
        let worker = self.acquire(camera_id)?;
        match worker.request(
            "motion.start",
            serde_json::json!({"source": source_json}),
            PRE_ROLL_REQUEST_TIMEOUT,
            None,
        ) {
            Ok(result)
                if result.get("started").and_then(serde_json::Value::as_bool) == Some(true) => {}
            // A previous owner may have disabled detection while events were
            // still awaiting durable ACK. Return a lease for draining those
            // events; after ACK, the dispatcher restarts the decoder normally.
            Err(CameraWorkerError::Rpc(code)) if code == "motion_unacknowledged_transitions" => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(CameraWorkerError::Protocol),
        }
        Ok(CameraMotionLease {
            broker: self.clone(),
            camera_id: camera_id.clone(),
            worker,
            stopped: false,
        })
    }

    /// Read local detector state without spawning a worker or exposing RTSP
    /// credentials. A missing worker is different from a healthy idle worker.
    pub fn local_motion_status(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraMotionStatus>, CameraWorkerError> {
        let slot = self
            .slots
            .lock()
            .map_err(|_| CameraWorkerError::Synchronization)?
            .get(camera_id)
            .cloned();
        let Some(worker) = slot.and_then(|slot| slot.current()) else {
            return Ok(None);
        };
        if !worker.is_running() {
            return Err(CameraWorkerError::Unavailable);
        }
        let response = worker.request(
            "motion.status",
            serde_json::json!({}),
            Duration::from_millis(750),
            None,
        )?;
        serde_json::from_value(response)
            .map(Some)
            .map_err(|_| CameraWorkerError::Protocol)
    }

    pub fn retain_pre_roll(
        &self,
        camera_id: &CameraId,
        source_json: serde_json::Value,
    ) -> Result<CameraPreRollLease, CameraWorkerError> {
        let worker = self.acquire(camera_id)?;
        let result = worker.request(
            "pre_roll.start",
            serde_json::json!({"source": source_json}),
            PRE_ROLL_REQUEST_TIMEOUT,
            None,
        )?;
        if result.get("started").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(CameraWorkerError::Protocol);
        }
        Ok(CameraPreRollLease {
            broker: self.clone(),
            camera_id: camera_id.clone(),
            worker,
            stopped: false,
        })
    }

    pub(crate) fn acquire(
        &self,
        camera_id: &CameraId,
    ) -> Result<Arc<CameraWorkerProcess>, CameraWorkerError> {
        let slot = {
            let mut slots = self
                .slots
                .lock()
                .map_err(|_| CameraWorkerError::Synchronization)?;
            Arc::clone(
                slots
                    .entry(camera_id.clone())
                    .or_insert_with(|| Arc::new(CameraWorkerSlot::default())),
            )
        };

        slot.acquire_with(
            |worker| worker.is_running(),
            || CameraWorkerProcess::spawn(&self.worker_program),
        )
    }

    pub fn media_diagnostics(&self) -> Vec<CameraMediaDiagnostics> {
        let slots = match self.slots.lock() {
            Ok(slots) => slots
                .iter()
                .map(|(camera_id, slot)| (camera_id.clone(), Arc::clone(slot)))
                .collect::<Vec<_>>(),
            Err(_) => return Vec::new(),
        };
        let mut rows = Vec::new();
        for (camera_id, slot) in slots {
            let Some(worker) = slot.current() else {
                continue;
            };
            if !worker.is_running() {
                continue;
            }
            let status = worker.request(
                "media.status",
                serde_json::json!({}),
                Duration::from_millis(750),
                None,
            );
            let parsed = status
                .ok()
                .and_then(|value| serde_json::from_value::<WorkerMediaStatus>(value).ok());
            let row = match parsed {
                Some(status) => CameraMediaDiagnostics {
                    camera_id: camera_id.as_str().to_owned(),
                    worker_available: true,
                    generation_starts: status.generation_starts,
                    source_count: status.source_count,
                    main_sources: status.main_sources,
                    sub_sources: status.sub_sources,
                    connecting_sources: status.connecting_sources,
                    ready_sources: status.ready_sources,
                    failed_sources: status.failed_sources,
                    subscribers: status.subscribers,
                    reliable_subscribers: status.reliable_subscribers,
                    realtime_subscribers: status.realtime_subscribers,
                    consumers: status.consumers,
                    queued_packets: status.queued_packets,
                    queued_bytes: status.queued_bytes,
                    dropped_packets: status.dropped_packets,
                    pre_roll_packets: status.pre_roll_packets,
                    pre_roll_bytes: status.pre_roll_bytes,
                    pre_roll_dropped_packets: status.pre_roll_dropped_packets,
                    sources: status.sources.into_iter().map(Into::into).collect(),
                },
                None => CameraMediaDiagnostics {
                    camera_id: camera_id.as_str().to_owned(),
                    worker_available: false,
                    generation_starts: 0,
                    source_count: 0,
                    main_sources: 0,
                    sub_sources: 0,
                    connecting_sources: 0,
                    ready_sources: 0,
                    failed_sources: 0,
                    subscribers: 0,
                    reliable_subscribers: 0,
                    realtime_subscribers: 0,
                    consumers: CameraMediaConsumerCounts::default(),
                    queued_packets: 0,
                    queued_bytes: 0,
                    dropped_packets: 0,
                    pre_roll_packets: 0,
                    pre_roll_bytes: 0,
                    pre_roll_dropped_packets: 0,
                    sources: Vec::new(),
                },
            };
            rows.push(row);
        }
        rows.sort_by(|left, right| left.camera_id.cmp(&right.camera_id));
        rows
    }

    /// Invalidates one broken worker generation. If it is still the broker's
    /// current generation, every other client holding the same process is
    /// forced to observe process loss instead of allowing a split-brain second
    /// worker for the camera. The next acquire converges through single-flight.
    pub(crate) fn invalidate(&self, camera_id: &CameraId, worker: &Arc<CameraWorkerProcess>) {
        let slot = self
            .slots
            .lock()
            .ok()
            .and_then(|slots| slots.get(camera_id).cloned());
        if slot.is_some_and(|slot| slot.vacate_if(worker)) {
            worker.force_reap();
        }
    }
}

impl std::fmt::Debug for CameraWorkerBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slots = self.slots.lock().map(|slots| slots.len()).unwrap_or(0);
        f.debug_struct("CameraWorkerBroker")
            .field("slots", &slots)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct FakeWorker;

    #[test]
    fn local_motion_runtime_lookup_never_spawns_an_unowned_camera_worker() {
        let broker = CameraWorkerBroker::new("nonexistent-worker-must-not-spawn".to_owned());
        let camera = CameraId::parse("front-door").unwrap();
        assert!(broker.local_motion_status(&camera).unwrap().is_none());
        assert!(broker.slots.lock().unwrap().is_empty());
    }

    #[test]
    fn media_status_schema_gracefully_defaults_new_diagnostics_fields() {
        let legacy_status = serde_json::json!({
            "generation_starts": 1,
            "source_count": 1,
            "main_sources": 1,
            "sub_sources": 0,
            "connecting_sources": 0,
            "ready_sources": 1,
            "failed_sources": 0,
            "subscribers": 1,
            "reliable_subscribers": 1,
            "realtime_subscribers": 0,
            "queued_packets": 0,
            "queued_bytes": 0,
            "dropped_packets": 0,
            "pre_roll_packets": 0,
            "pre_roll_bytes": 0,
            "pre_roll_dropped_packets": 0
        });
        let parsed: WorkerMediaStatus = serde_json::from_value(legacy_status).unwrap();
        assert!(parsed.sources.is_empty());
        assert_eq!(parsed.consumers, CameraMediaConsumerCounts::default());

        let source_without_fps = serde_json::json!({
            "generation_starts": 1,
            "source_count": 1,
            "main_sources": 1,
            "sub_sources": 0,
            "connecting_sources": 0,
            "ready_sources": 1,
            "failed_sources": 0,
            "subscribers": 1,
            "reliable_subscribers": 1,
            "realtime_subscribers": 0,
            "consumers": {"recording": 1, "live": 0, "motion": 0, "event": 0, "other": 0},
            "queued_packets": 0,
            "queued_bytes": 0,
            "dropped_packets": 0,
            "pre_roll_packets": 0,
            "pre_roll_bytes": 0,
            "pre_roll_dropped_packets": 0,
            "sources": [{
                "profile": "main",
                "lifecycle": "ready",
                "retainers": 1,
                "subscribers": 1,
                "reliable_subscribers": 1,
                "realtime_subscribers": 0,
                "consumers": {"recording": 1, "live": 0, "motion": 0, "event": 0, "other": 0},
                "queued_packets": 0,
                "queued_bytes": 0,
                "dropped_packets": 0,
                "pre_roll_packets": 0,
                "pre_roll_bytes": 0,
                "pre_roll_dropped_packets": 0,
                "video_codec": "h264",
                "video_width": 1920,
                "video_height": 1080
            }]
        });
        let mut parsed: WorkerMediaStatus = serde_json::from_value(source_without_fps).unwrap();
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].video_frame_rate, None);
        assert_eq!(parsed.consumers.recording, 1);
        assert_eq!(
            CameraMediaSourceDiagnostics::from(parsed.sources.remove(0))
                .consumers
                .recording,
            1
        );
    }

    #[test]
    fn same_camera_slot_single_flights_concurrent_openers() {
        const CALLERS: usize = 8;
        let slot = Arc::new(SingleFlightSlot::<FakeWorker>::default());
        let opens = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(CALLERS));
        let hold = Arc::new(Barrier::new(CALLERS));
        let mut threads = Vec::new();

        for _ in 0..CALLERS {
            let slot = Arc::clone(&slot);
            let opens = Arc::clone(&opens);
            let start = Arc::clone(&start);
            let hold = Arc::clone(&hold);
            threads.push(std::thread::spawn(move || {
                start.wait();
                let worker = slot
                    .acquire_with(
                        |_| true,
                        || {
                            opens.fetch_add(1, Ordering::AcqRel);
                            std::thread::sleep(Duration::from_millis(30));
                            Ok::<_, ()>(Arc::new(FakeWorker))
                        },
                    )
                    .unwrap();
                hold.wait();
                worker
            }));
        }

        let workers = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(opens.load(Ordering::Acquire), 1);
        assert!(
            workers[1..]
                .iter()
                .all(|worker| Arc::ptr_eq(&workers[0], worker))
        );
    }

    #[test]
    fn stale_generation_cannot_vacate_a_newer_worker() {
        let slot = SingleFlightSlot::<FakeWorker>::default();
        let first = slot
            .acquire_with(|_| true, || Ok::<_, ()>(Arc::new(FakeWorker)))
            .unwrap();
        assert!(slot.vacate_if(&first));

        let second = slot
            .acquire_with(|_| true, || Ok::<_, ()>(Arc::new(FakeWorker)))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!slot.vacate_if(&first));
        let current = slot
            .acquire_with(|_| true, || Ok::<_, ()>(Arc::new(FakeWorker)))
            .unwrap();
        assert!(Arc::ptr_eq(&second, &current));
    }

    #[test]
    fn failed_open_wakes_waiter_and_allows_a_fresh_attempt() {
        let slot = Arc::new(SingleFlightSlot::<FakeWorker>::default());
        let attempts = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();

        for _ in 0..2 {
            let slot = Arc::clone(&slot);
            let attempts = Arc::clone(&attempts);
            let start = Arc::clone(&start);
            threads.push(std::thread::spawn(move || {
                start.wait();
                slot.acquire_with(
                    |_| true,
                    || {
                        let attempt = attempts.fetch_add(1, Ordering::AcqRel);
                        if attempt == 0 {
                            std::thread::sleep(Duration::from_millis(30));
                            Err(())
                        } else {
                            Ok(Arc::new(FakeWorker))
                        }
                    },
                )
            }));
        }

        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    }
}
