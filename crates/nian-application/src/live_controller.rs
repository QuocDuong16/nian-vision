//! M11 multi-camera live-view ownership and localhost media exposure.
//!
//! Live viewing is deliberately independent from recording. Each selected camera
//! owns one worker-backed live session and one opaque localhost endpoint. The
//! frontend receives only the opaque session id and loopback URL.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use nian_domain::CameraId;
use nian_ipc::message::{Envelope, PROTOCOL_VERSION, event, method};
use nian_ipc::{FramedReader, FramedWriter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_SIMULTANEOUS_LIVE_VIEWS: usize = 4;
const LIVE_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const WORKER_HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const LIVE_FRAGMENT_TARGET: Duration = Duration::from_secs(1);
pub const MAX_RETAINED_LIVE_FRAGMENTS: usize = 6;
pub const MAX_LIVE_FRAGMENT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_HTTP_REQUESTS: usize = 8;
pub const MAX_HTTP_READERS_PER_SESSION: usize = 2;
pub const MAX_LIVE_CACHE_FRAGMENTS: usize =
    MAX_RETAINED_LIVE_FRAGMENTS + MAX_HTTP_READERS_PER_SESSION;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_LIVE_MANIFEST_BYTES: usize = 16 * 1024;
const LIVE_HTTP_READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(16);
const LIVE_FRAGMENT_PREFIX: &str = "fragment-";
const LIVE_FRAGMENT_SUFFIX: &str = ".mp4";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    Starting,
    Connecting,
    Live,
    Backoff,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveFailureCategory {
    SourceOpenFailed,
    UnsupportedCodec,
    WorkerUnavailable,
    MediaFailed,
    MediaReadFailed,
    MediaFragmentCreateFailed,
    MediaFragmentWriteFailed,
    MediaPacketTooLarge,
    MediaFragmentLimitExceeded,
    MediaMuxWriteFailed,
    MediaFragmentFinalizeFailed,
    MediaFragmentCapacityFailed,
    LifecycleCancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveWorkerStatus {
    pub state: LiveState,
    pub failure_category: Option<LiveFailureCategory>,
    pub reconnect_attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveStatus {
    pub session_id: String,
    pub camera_id: String,
    pub state: LiveState,
    pub failure_category: Option<LiveFailureCategory>,
    pub reconnect_attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveOpenDto {
    pub session_id: String,
    pub camera_id: String,
    pub url: String,
    pub state: LiveState,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LiveError {
    #[error("camera already has an active live session")]
    AlreadyOpen,
    #[error("too many live sessions are active")]
    Capacity,
    #[error("live session has expired")]
    SessionExpired,
    #[error("media worker is unavailable")]
    WorkerUnavailable,
    #[error("live source could not be opened")]
    SourceOpenFailed,
    #[error("live source codec is unsupported")]
    UnsupportedCodec,
    #[error("live media failed")]
    MediaFailed,
    #[error("live admission is blocked by desktop lifecycle")]
    LifecycleBlocked,
    #[error("live service is unavailable")]
    Internal,
}

/// Secret-bearing worker input. Intentionally not Debug/Serialize.
pub struct PreparedLive {
    pub camera_id: CameraId,
    pub source_json: serde_json::Value,
}

struct OpeningState {
    session_id: String,
    camera_id: CameraId,
    temp_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    runner: Mutex<Option<Box<dyn LiveRunner>>>,
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl OpeningState {
    fn new(session_id: String, camera_id: CameraId, temp_dir: PathBuf) -> Self {
        Self {
            session_id,
            camera_id,
            temp_dir,
            cancel: Arc::new(AtomicBool::new(false)),
            runner: Mutex::new(None),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn install_runner(&self, runner: Box<dyn LiveRunner>) -> Result<(), Box<dyn LiveRunner>> {
        if self.is_cancelled() {
            return Err(runner);
        }
        let Ok(mut slot) = self.runner.lock() else {
            return Err(runner);
        };
        if self.is_cancelled() {
            return Err(runner);
        }
        *slot = Some(runner);
        self.done_cv.notify_all();
        Ok(())
    }

    fn start_runner(&self, source_json: serde_json::Value) -> Result<(), LiveError> {
        let mut slot = self
            .runner
            .lock()
            .map_err(|_| LiveError::WorkerUnavailable)?;
        let runner = slot.as_mut().ok_or(LiveError::WorkerUnavailable)?;
        runner.start(source_json, &self.temp_dir, self.cancel.clone())
    }

    fn take_runner(&self) -> Option<Box<dyn LiveRunner>> {
        self.runner.lock().ok()?.take()
    }

    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Ok(mut slot) = self.runner.lock()
            && let Some(runner) = slot.as_mut()
        {
            runner.request_stop();
        }
        self.done_cv.notify_all();
    }

    fn mark_done(&self) {
        if let Ok(mut done) = self.done.lock() {
            *done = true;
            self.done_cv.notify_all();
        }
    }

    fn cleanup_temp(&self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }

    fn wait_and_reap(&self) {
        self.cancel.store(true, Ordering::Release);
        loop {
            if let Some(mut runner) = self.take_runner() {
                runner.request_stop();
                runner.join_or_reap();
                self.cleanup_temp();
                self.mark_done();
                return;
            }

            let Ok(done) = self.done.lock() else {
                return;
            };
            if *done {
                return;
            }
            let wait = self.done_cv.wait_timeout(done, Duration::from_millis(50));
            match wait {
                Ok((done, _)) if *done => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }
}

pub struct LiveOpenAdmission {
    source_json: serde_json::Value,
    factory: Arc<dyn LiveRunnerFactory>,
    registry: Weak<Mutex<LiveRegistry>>,
    opening: Arc<OpeningState>,
    cleanup_on_drop: bool,
}

impl LiveOpenAdmission {
    pub fn start(mut self) -> Result<StartedLiveOpen, LiveOpenStartError> {
        if self.opening.is_cancelled() {
            return Err(self.start_error(LiveError::LifecycleBlocked));
        }
        let runner = match self.factory.spawn() {
            Ok(runner) => runner,
            Err(error) => return Err(self.start_error(error)),
        };
        if let Err(mut runner) = self.opening.install_runner(runner) {
            runner.request_stop();
            runner.join_or_reap();
            return Err(self.start_error(LiveError::LifecycleBlocked));
        }

        let source_json = std::mem::take(&mut self.source_json);
        if let Err(error) = self.opening.start_runner(source_json) {
            self.opening.request_cancel();
            self.opening.wait_and_reap();
            return Err(self.start_error(error));
        }
        if self.opening.is_cancelled() {
            self.opening.request_cancel();
            self.opening.wait_and_reap();
            return Err(self.start_error(LiveError::LifecycleBlocked));
        }

        self.cleanup_on_drop = false;
        Ok(StartedLiveOpen {
            opening: self.opening.clone(),
            registry: self.registry.clone(),
            cleanup_on_drop: true,
        })
    }

    fn start_error(&self, error: LiveError) -> LiveOpenStartError {
        LiveOpenStartError {
            session_id: self.opening.session_id.clone(),
            camera_id: self.opening.camera_id.clone(),
            error,
        }
    }
}

impl Drop for LiveOpenAdmission {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            remove_opening_reservation(&self.registry, &self.opening);
            self.opening.cleanup_temp();
            self.opening.mark_done();
        }
    }
}

#[derive(Debug)]
pub struct LiveOpenStartError {
    session_id: String,
    camera_id: CameraId,
    error: LiveError,
}

pub struct StartedLiveOpen {
    opening: Arc<OpeningState>,
    registry: Weak<Mutex<LiveRegistry>>,
    cleanup_on_drop: bool,
}

impl Drop for StartedLiveOpen {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            remove_opening_reservation(&self.registry, &self.opening);
            self.opening.request_cancel();
            self.opening.wait_and_reap();
        }
    }
}

fn remove_opening_reservation(registry: &Weak<Mutex<LiveRegistry>>, opening: &OpeningState) {
    if let Some(registry) = registry.upgrade()
        && let Ok(mut registry) = registry.lock()
        && registry
            .opening
            .get(&opening.camera_id)
            .is_some_and(|current| current.session_id == opening.session_id)
    {
        registry.opening.remove(&opening.camera_id);
    }
}

pub trait LiveRunner: Send {
    fn start(
        &mut self,
        source_json: serde_json::Value,
        output_dir: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), LiveError>;
    fn status(&mut self) -> Result<LiveWorkerStatus, LiveError>;
    fn request_stop(&mut self);
    fn join_or_reap(&mut self);
}

pub trait LiveRunnerFactory: Send + Sync {
    fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError>;
}

#[derive(Debug, Clone)]
pub struct WorkerLiveRunnerFactory {
    pub worker_program: String,
}

impl LiveRunnerFactory for WorkerLiveRunnerFactory {
    fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
        WorkerLiveRunner::spawn(&self.worker_program)
            .map(|runner| Box::new(runner) as Box<dyn LiveRunner>)
    }
}

#[derive(Default)]
struct SessionTeardownState {
    started: bool,
    done: bool,
}

struct LiveSession {
    camera_id: CameraId,
    temp_dir: PathBuf,
    last_keepalive: Mutex<Instant>,
    runner: Mutex<Option<Box<dyn LiveRunner>>>,
    http: Arc<HttpSession>,
    teardown: Mutex<SessionTeardownState>,
    teardown_cv: Condvar,
}

impl LiveSession {
    fn request_stop(&self) {
        if let Ok(mut runner) = self.runner.lock()
            && let Some(runner) = runner.as_mut()
        {
            runner.request_stop();
        }
    }

    fn join_and_cleanup(&self) {
        let leader = {
            let Ok(mut teardown) = self.teardown.lock() else {
                return;
            };
            if teardown.done {
                return;
            }
            if teardown.started {
                while !teardown.done {
                    match self.teardown_cv.wait(teardown) {
                        Ok(next) => teardown = next,
                        Err(_) => return,
                    }
                }
                return;
            }
            teardown.started = true;
            true
        };
        if !leader {
            return;
        }

        if let Ok(mut slot) = self.runner.lock()
            && let Some(mut runner) = slot.take()
        {
            runner.request_stop();
            runner.join_or_reap();
        }
        self.http.deactivate();
        if self.http.wait_for_readers() {
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
        if let Ok(mut teardown) = self.teardown.lock() {
            teardown.done = true;
            self.teardown_cv.notify_all();
        }
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.join_and_cleanup();
    }
}

#[derive(Default)]
struct LiveRegistry {
    sessions: HashMap<String, Arc<LiveSession>>,
    camera_sessions: HashMap<CameraId, String>,
    opening: HashMap<CameraId, Arc<OpeningState>>,
    draining_sessions: HashMap<String, Arc<LiveSession>>,
    draining_openings: HashMap<String, Arc<OpeningState>>,
    accepting: bool,
}

#[derive(Default)]
struct HttpReaderState {
    total: usize,
    fragments: HashMap<String, usize>,
}

struct HttpSession {
    session_dir: PathBuf,
    active: AtomicBool,
    readers: Mutex<HttpReaderState>,
    readers_cv: Condvar,
}

impl HttpSession {
    fn new(session_dir: PathBuf) -> Self {
        Self {
            session_dir,
            active: AtomicBool::new(true),
            readers: Mutex::new(HttpReaderState::default()),
            readers_cv: Condvar::new(),
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.readers_cv.notify_all();
    }

    fn try_acquire_fragment(self: &Arc<Self>, fragment: &str) -> Option<HttpFragmentReadGuard> {
        if !self.is_active() {
            return None;
        }
        let mut readers = self.readers.lock().ok()?;
        if !self.is_active() || readers.total >= MAX_HTTP_READERS_PER_SESSION {
            return None;
        }
        readers.total += 1;
        *readers.fragments.entry(fragment.to_owned()).or_insert(0) += 1;
        Some(HttpFragmentReadGuard {
            session: self.clone(),
            fragment: fragment.to_owned(),
        })
    }

    fn fragment_has_reader(&self, fragment: &str) -> bool {
        self.readers
            .lock()
            .ok()
            .and_then(|readers| readers.fragments.get(fragment).copied())
            .unwrap_or(0)
            > 0
    }

    fn wait_for_readers(&self) -> bool {
        let Ok(mut readers) = self.readers.lock() else {
            return false;
        };
        let deadline = Instant::now() + LIVE_HTTP_READER_DRAIN_TIMEOUT;
        while readers.total > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self
                .readers_cv
                .wait_timeout(readers, remaining.min(Duration::from_millis(50)))
            {
                Ok((next, _)) => readers = next,
                Err(_) => return false,
            }
        }
        true
    }
}

struct HttpFragmentReadGuard {
    session: Arc<HttpSession>,
    fragment: String,
}

impl Drop for HttpFragmentReadGuard {
    fn drop(&mut self) {
        if let Ok(mut readers) = self.session.readers.lock() {
            readers.total = readers.total.saturating_sub(1);
            if let Some(count) = readers.fragments.get_mut(&self.fragment) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    readers.fragments.remove(&self.fragment);
                }
            }
            self.session.readers_cv.notify_all();
        }
        trim_session_fragments(&self.session);
        if !self.session.is_active()
            && self
                .session
                .readers
                .lock()
                .ok()
                .is_some_and(|readers| readers.total == 0)
        {
            let _ = std::fs::remove_dir_all(&self.session.session_dir);
        }
    }
}

#[derive(Default)]
struct LiveHttpRuntime {
    sessions: HashMap<String, Arc<HttpSession>>,
}

#[must_use = "captured live owners remain draining until this batch is finished"]
pub struct LiveTeardownBatch {
    openings: Vec<(String, Arc<OpeningState>)>,
    sessions: Vec<(String, Arc<LiveSession>)>,
}

impl LiveTeardownBatch {
    fn is_empty(&self) -> bool {
        self.openings.is_empty() && self.sessions.is_empty()
    }
}

pub struct LiveViewController {
    factory: Arc<dyn LiveRunnerFactory>,
    registry: Arc<Mutex<LiveRegistry>>,
    http_runtime: Arc<Mutex<LiveHttpRuntime>>,
    server: Mutex<LiveHttpServer>,
    reaper: Mutex<LiveSessionReaper>,
    cache_root: PathBuf,
}

impl std::fmt::Debug for LiveViewController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (sessions, accepting) = self
            .registry
            .lock()
            .map(|registry| (registry.sessions.len(), registry.accepting))
            .unwrap_or((MAX_SIMULTANEOUS_LIVE_VIEWS, false));
        f.debug_struct("LiveViewController")
            .field("sessions", &sessions)
            .field("port", &self.server.lock().map(|server| server.port).ok())
            .field("accepting", &accepting)
            .finish()
    }
}

impl LiveViewController {
    pub fn new(worker_program: String, cache_root: PathBuf) -> Result<Self, LiveError> {
        Self::with_factory(
            Arc::new(WorkerLiveRunnerFactory { worker_program }),
            cache_root,
        )
    }

    pub fn with_factory(
        factory: Arc<dyn LiveRunnerFactory>,
        cache_root: PathBuf,
    ) -> Result<Self, LiveError> {
        Self::with_factory_and_timeout(factory, cache_root, LIVE_KEEPALIVE_TIMEOUT)
    }

    fn with_factory_and_timeout(
        factory: Arc<dyn LiveRunnerFactory>,
        cache_root: PathBuf,
        session_timeout: Duration,
    ) -> Result<Self, LiveError> {
        std::fs::create_dir_all(&cache_root).map_err(|_| LiveError::Internal)?;
        cleanup_abandoned_live_cache(&cache_root)?;
        let registry = Arc::new(Mutex::new(LiveRegistry {
            accepting: true,
            ..LiveRegistry::default()
        }));
        let http_runtime = Arc::new(Mutex::new(LiveHttpRuntime::default()));
        let server = LiveHttpServer::start(http_runtime.clone())?;
        let reaper =
            LiveSessionReaper::start(registry.clone(), http_runtime.clone(), session_timeout)?;
        Ok(Self {
            factory,
            registry,
            http_runtime,
            server: Mutex::new(server),
            reaper: Mutex::new(reaper),
            cache_root,
        })
    }

    pub fn open(&self, prepared: PreparedLive) -> Result<LiveOpenDto, LiveError> {
        let admission = self.admit(prepared)?;
        match admission.start() {
            Ok(started) => self.commit_open(started),
            Err(failed) => Err(self.cancel_failed_open(failed)),
        }
    }

    pub fn admit(&self, prepared: PreparedLive) -> Result<LiveOpenAdmission, LiveError> {
        let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
        if !registry.accepting {
            return Err(LiveError::LifecycleBlocked);
        }
        if registry.camera_sessions.contains_key(&prepared.camera_id)
            || registry.opening.contains_key(&prepared.camera_id)
        {
            return Err(LiveError::AlreadyOpen);
        }
        let owned_workers = registry.sessions.len()
            + registry.opening.len()
            + registry.draining_sessions.len()
            + registry.draining_openings.len();
        if owned_workers >= MAX_SIMULTANEOUS_LIVE_VIEWS {
            return Err(LiveError::Capacity);
        }

        let token = Uuid::new_v4().to_string();
        let temp_dir = self.cache_root.join(format!("session-{token}"));
        std::fs::create_dir(&temp_dir).map_err(|_| LiveError::Internal)?;
        let opening = Arc::new(OpeningState::new(
            token,
            prepared.camera_id.clone(),
            temp_dir,
        ));
        registry
            .opening
            .insert(prepared.camera_id.clone(), opening.clone());
        Ok(LiveOpenAdmission {
            source_json: prepared.source_json,
            factory: self.factory.clone(),
            registry: Arc::downgrade(&self.registry),
            opening,
            cleanup_on_drop: true,
        })
    }

    pub fn cancel_failed_open(&self, failed: LiveOpenStartError) -> LiveError {
        let opening = if let Ok(mut registry) = self.registry.lock() {
            let matches = registry
                .opening
                .get(&failed.camera_id)
                .is_some_and(|opening| opening.session_id == failed.session_id);
            if matches {
                registry.opening.remove(&failed.camera_id)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(opening) = opening {
            opening.cleanup_temp();
            opening.mark_done();
        }
        failed.error
    }

    pub fn commit_open(&self, mut started: StartedLiveOpen) -> Result<LiveOpenDto, LiveError> {
        let opening = started.opening.clone();
        let session_id = opening.session_id.clone();
        let camera_id = opening.camera_id.clone();
        let temp_dir = opening.temp_dir.clone();
        let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
        let reserved = registry
            .opening
            .get(&camera_id)
            .is_some_and(|current| current.session_id == session_id);
        if !registry.accepting
            || !reserved
            || opening.is_cancelled()
            || registry.camera_sessions.contains_key(&camera_id)
        {
            return Err(LiveError::LifecycleBlocked);
        }

        let mut runtime = self.http_runtime.lock().map_err(|_| LiveError::Internal)?;
        let Some(runner) = opening.take_runner() else {
            return Err(LiveError::LifecycleBlocked);
        };
        let http = Arc::new(HttpSession::new(temp_dir.clone()));
        let session = Arc::new(LiveSession {
            camera_id: camera_id.clone(),
            temp_dir,
            last_keepalive: Mutex::new(Instant::now()),
            runner: Mutex::new(Some(runner)),
            http: http.clone(),
            teardown: Mutex::new(SessionTeardownState::default()),
            teardown_cv: Condvar::new(),
        });
        runtime.sessions.insert(session_id.clone(), http);
        registry.opening.remove(&camera_id);
        registry
            .camera_sessions
            .insert(camera_id.clone(), session_id.clone());
        registry.sessions.insert(session_id.clone(), session);
        started.cleanup_on_drop = false;
        opening.mark_done();
        let port = self.server.lock().map_err(|_| LiveError::Internal)?.port;
        Ok(LiveOpenDto {
            session_id: session_id.clone(),
            camera_id: camera_id.as_str().to_owned(),
            url: format!("http://127.0.0.1:{port}/live/{session_id}"),
            state: LiveState::Starting,
        })
    }

    pub fn close(&self, session_id: &str) -> Result<(), LiveError> {
        let session = {
            let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
            let session = registry
                .sessions
                .remove(session_id)
                .ok_or(LiveError::SessionExpired)?;
            if registry
                .camera_sessions
                .get(&session.camera_id)
                .is_some_and(|current| current == session_id)
            {
                registry.camera_sessions.remove(&session.camera_id);
            }
            registry
                .draining_sessions
                .insert(session_id.to_owned(), session.clone());
            session
        };
        if let Ok(mut runtime) = self.http_runtime.lock()
            && let Some(http) = runtime.sessions.remove(session_id)
        {
            http.deactivate();
        }
        teardown_live_owners(
            &self.registry,
            Vec::new(),
            vec![(session_id.to_owned(), session)],
        );
        Ok(())
    }

    pub fn keep_alive(&self, session_id: &str) -> Result<(), LiveError> {
        let session = self
            .registry
            .lock()
            .map_err(|_| LiveError::Internal)?
            .sessions
            .get(session_id)
            .cloned()
            .ok_or(LiveError::SessionExpired)?;
        *session
            .last_keepalive
            .lock()
            .map_err(|_| LiveError::Internal)? = Instant::now();
        Ok(())
    }

    pub fn statuses(&self) -> Result<Vec<LiveStatus>, LiveError> {
        let sessions: Vec<(String, Arc<LiveSession>)> = self
            .registry
            .lock()
            .map_err(|_| LiveError::Internal)?
            .sessions
            .iter()
            .map(|(session_id, session)| (session_id.clone(), session.clone()))
            .collect();
        let mut statuses = std::thread::scope(|scope| {
            let handles: Vec<_> = sessions
                .into_iter()
                .map(|(session_id, session)| {
                    scope.spawn(move || {
                        let worker = session
                            .runner
                            .lock()
                            .map_err(|_| LiveError::WorkerUnavailable)
                            .and_then(|mut slot| {
                                slot.as_mut().ok_or(LiveError::WorkerUnavailable)?.status()
                            })
                            .unwrap_or_else(|error| LiveWorkerStatus {
                                state: LiveState::Failed,
                                failure_category: Some(failure_category_for_error(&error)),
                                reconnect_attempt: 0,
                            });
                        LiveStatus {
                            session_id,
                            camera_id: session.camera_id.as_str().to_owned(),
                            state: worker.state,
                            failure_category: worker.failure_category,
                            reconnect_attempt: worker.reconnect_attempt,
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect::<Vec<_>>()
        });
        statuses.sort_by(|left, right| left.camera_id.cmp(&right.camera_id));
        Ok(statuses)
    }

    pub fn begin_close_all(&self) -> LiveTeardownBatch {
        let mut captured_sessions = Vec::new();
        let mut captured_openings = Vec::new();
        if let Ok(mut registry) = self.registry.lock() {
            registry.camera_sessions.clear();

            let active_openings = std::mem::take(&mut registry.opening);
            for opening in active_openings.into_values() {
                let session_id = opening.session_id.clone();
                registry
                    .draining_openings
                    .entry(session_id.clone())
                    .or_insert_with(|| opening.clone());
                captured_openings.push((session_id, opening));
            }
            let active_sessions = std::mem::take(&mut registry.sessions);
            for (session_id, session) in active_sessions {
                registry
                    .draining_sessions
                    .entry(session_id.clone())
                    .or_insert_with(|| session.clone());
                captured_sessions.push((session_id, session));
            }
        }

        for (_, session) in &captured_sessions {
            session.http.deactivate();
        }
        if let Ok(mut runtime) = self.http_runtime.lock() {
            for (session_id, session) in &captured_sessions {
                if runtime
                    .sessions
                    .get(session_id)
                    .is_some_and(|http| Arc::ptr_eq(http, &session.http))
                {
                    runtime.sessions.remove(session_id);
                }
            }
        }

        LiveTeardownBatch {
            openings: captured_openings,
            sessions: captured_sessions,
        }
    }

    pub fn finish_close_all(&self, batch: LiveTeardownBatch) {
        if !batch.is_empty() {
            teardown_live_owners(&self.registry, batch.openings, batch.sessions);
        }
    }

    fn snapshot_draining(&self) -> LiveTeardownBatch {
        if let Ok(registry) = self.registry.lock() {
            return LiveTeardownBatch {
                openings: registry
                    .draining_openings
                    .iter()
                    .map(|(session_id, opening)| (session_id.clone(), opening.clone()))
                    .collect(),
                sessions: registry
                    .draining_sessions
                    .iter()
                    .map(|(session_id, session)| (session_id.clone(), session.clone()))
                    .collect(),
            };
        }
        LiveTeardownBatch {
            openings: Vec::new(),
            sessions: Vec::new(),
        }
    }

    pub fn close_all(&self) {
        let batch = self.begin_close_all();
        self.finish_close_all(batch);
        loop {
            let draining = self.snapshot_draining();
            if draining.is_empty() {
                break;
            }
            self.finish_close_all(draining);
        }
    }

    pub fn stop_accepting(&self) {
        let openings = if let Ok(mut registry) = self.registry.lock() {
            registry.accepting = false;
            registry
                .opening
                .values()
                .chain(registry.draining_openings.values())
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for opening in openings {
            opening.request_cancel();
        }
    }

    pub fn resume_accepting(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.accepting = true;
        }
    }

    pub fn shutdown(&self) {
        self.stop_accepting();
        self.close_all();
        if let Ok(mut reaper) = self.reaper.lock() {
            reaper.shutdown();
        }
        if let Ok(mut server) = self.server.lock() {
            server.shutdown();
        }
    }
}

fn teardown_live_owners(
    registry: &Arc<Mutex<LiveRegistry>>,
    openings: Vec<(String, Arc<OpeningState>)>,
    sessions: Vec<(String, Arc<LiveSession>)>,
) {
    for (_, opening) in &openings {
        opening.cancel.store(true, Ordering::Release);
        opening.done_cv.notify_all();
    }

    std::thread::scope(|scope| {
        let mut signals = Vec::with_capacity(openings.len() + sessions.len());
        for (_, opening) in &openings {
            signals.push(scope.spawn(|| opening.request_cancel()));
        }
        for (_, session) in &sessions {
            signals.push(scope.spawn(|| session.request_stop()));
        }
        for signal in signals {
            let _ = signal.join();
        }
    });

    std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(openings.len() + sessions.len());
        for (session_id, opening) in openings {
            let registry = registry.clone();
            joins.push(scope.spawn(move || {
                opening.wait_and_reap();
                if let Ok(mut registry) = registry.lock()
                    && registry
                        .draining_openings
                        .get(&session_id)
                        .is_some_and(|current| Arc::ptr_eq(current, &opening))
                {
                    registry.draining_openings.remove(&session_id);
                }
            }));
        }
        for (session_id, session) in sessions {
            let registry = registry.clone();
            joins.push(scope.spawn(move || {
                session.join_and_cleanup();
                if let Ok(mut registry) = registry.lock()
                    && registry
                        .draining_sessions
                        .get(&session_id)
                        .is_some_and(|current| Arc::ptr_eq(current, &session))
                {
                    registry.draining_sessions.remove(&session_id);
                }
            }));
        }
        for join in joins {
            let _ = join.join();
        }
    });
}

fn cleanup_abandoned_live_cache(cache_root: &Path) -> Result<(), LiveError> {
    let entries = std::fs::read_dir(cache_root).map_err(|_| LiveError::Internal)?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(token) = name.strip_prefix("session-") else {
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(token) else {
            continue;
        };
        if uuid.hyphenated().to_string() != token {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(
                error = %error,
                "failed to remove abandoned live-cache session"
            );
        }
    }
    Ok(())
}

#[derive(Debug)]
struct LiveFragment {
    sequence: u64,
    name: String,
    path: PathBuf,
    bytes: u64,
}

fn parse_live_fragment_name(name: &str) -> Option<u64> {
    let sequence = name
        .strip_prefix(LIVE_FRAGMENT_PREFIX)?
        .strip_suffix(LIVE_FRAGMENT_SUFFIX)?;
    if sequence.is_empty()
        || sequence.len() > 20
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    sequence.parse().ok()
}

fn list_live_fragments(session_dir: &Path) -> Vec<LiveFragment> {
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return Vec::new();
    };
    let mut fragments = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(sequence) = parse_live_fragment_name(&name) else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        fragments.push(LiveFragment {
            sequence,
            name,
            path: entry.path(),
            bytes: metadata.len(),
        });
    }
    fragments.sort_by_key(|fragment| fragment.sequence);
    fragments
}

fn trim_session_fragments(session: &HttpSession) {
    let fragments = list_live_fragments(&session.session_dir);
    if fragments.is_empty() {
        return;
    }
    let retain_from = fragments.len().saturating_sub(MAX_RETAINED_LIVE_FRAGMENTS);
    let mut remaining_bytes = fragments.iter().map(|fragment| fragment.bytes).sum::<u64>();
    let target_bytes = MAX_RETAINED_LIVE_FRAGMENTS as u64 * MAX_LIVE_FRAGMENT_BYTES;

    for (index, fragment) in fragments.into_iter().enumerate() {
        let older_than_window = index < retain_from;
        let over_bytes = remaining_bytes > target_bytes;
        let oversized = fragment.bytes > MAX_LIVE_FRAGMENT_BYTES;
        if !(older_than_window || over_bytes || oversized) {
            continue;
        }
        if session.fragment_has_reader(&fragment.name) {
            continue;
        }
        if std::fs::remove_file(&fragment.path).is_ok() {
            remaining_bytes = remaining_bytes.saturating_sub(fragment.bytes);
        }
    }
}

fn maintain_live_fragments(http_runtime: &Arc<Mutex<LiveHttpRuntime>>) {
    let sessions = http_runtime
        .lock()
        .map(|runtime| runtime.sessions.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for session in sessions {
        trim_session_fragments(&session);
    }
}

fn expire_live_sessions(
    registry: &Arc<Mutex<LiveRegistry>>,
    http_runtime: &Arc<Mutex<LiveHttpRuntime>>,
    session_timeout: Duration,
) {
    let now = Instant::now();
    let removed = {
        let Ok(mut registry) = registry.lock() else {
            return;
        };
        let expired: Vec<String> = registry
            .sessions
            .iter()
            .filter(|(_, session)| {
                session
                    .last_keepalive
                    .lock()
                    .ok()
                    .is_some_and(|last| now.duration_since(*last) >= session_timeout)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        let mut removed = Vec::with_capacity(expired.len());
        for session_id in expired {
            if let Some(session) = registry.sessions.remove(&session_id) {
                if registry
                    .camera_sessions
                    .get(&session.camera_id)
                    .is_some_and(|current| current == &session_id)
                {
                    registry.camera_sessions.remove(&session.camera_id);
                }
                registry
                    .draining_sessions
                    .insert(session_id.clone(), session.clone());
                removed.push((session_id, session));
            }
        }
        removed
    };
    if removed.is_empty() {
        return;
    }
    if let Ok(mut runtime) = http_runtime.lock() {
        for (session_id, _) in &removed {
            if let Some(http) = runtime.sessions.remove(session_id) {
                http.deactivate();
            }
        }
    }
    teardown_live_owners(registry, Vec::new(), removed);
}

struct LiveSessionReaper {
    stop_tx: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveSessionReaper {
    fn start(
        registry: Arc<Mutex<LiveRegistry>>,
        http_runtime: Arc<Mutex<LiveHttpRuntime>>,
        session_timeout: Duration,
    ) -> Result<Self, LiveError> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let interval = Duration::from_millis(500).min(session_timeout);
        let thread = std::thread::Builder::new()
            .name("live-session-reaper".to_owned())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            maintain_live_fragments(&http_runtime);
                            expire_live_sessions(&registry, &http_runtime, session_timeout);
                        }
                    }
                }
            })
            .map_err(|_| LiveError::Internal)?;
        Ok(Self {
            stop_tx: Some(stop_tx),
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LiveSessionReaper {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn failure_category_for_error(error: &LiveError) -> LiveFailureCategory {
    match error {
        LiveError::SourceOpenFailed => LiveFailureCategory::SourceOpenFailed,
        LiveError::UnsupportedCodec => LiveFailureCategory::UnsupportedCodec,
        LiveError::MediaFailed => LiveFailureCategory::MediaFailed,
        LiveError::LifecycleBlocked => LiveFailureCategory::LifecycleCancelled,
        LiveError::AlreadyOpen
        | LiveError::Capacity
        | LiveError::SessionExpired
        | LiveError::WorkerUnavailable
        | LiveError::Internal => LiveFailureCategory::WorkerUnavailable,
    }
}

impl Drop for LiveViewController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct LiveHttpServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveHttpServer {
    fn start(runtime: Arc<Mutex<LiveHttpRuntime>>) -> Result<Self, LiveError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| LiveError::Internal)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| LiveError::Internal)?;
        let port = listener
            .local_addr()
            .map_err(|_| LiveError::Internal)?
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::Builder::new()
            .name("live-loopback-http".to_owned())
            .spawn(move || {
                while !stop_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if active.load(Ordering::Acquire) >= MAX_HTTP_REQUESTS {
                                let _ = write_empty(stream, "503 Service Unavailable");
                                continue;
                            }
                            active.fetch_add(1, Ordering::AcqRel);
                            let runtime = runtime.clone();
                            let active_for_thread = active.clone();
                            let spawned = std::thread::Builder::new()
                                .name("live-http-request".to_owned())
                                .spawn(move || {
                                    let _guard = ActiveRequestGuard(active_for_thread);
                                    let _ = serve_live_request(stream, port, &runtime);
                                });
                            if spawned.is_err() {
                                active.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            })
            .map_err(|_| LiveError::Internal)?;
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LiveHttpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ActiveRequestGuard(Arc<AtomicUsize>);

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_live_request(
    mut stream: TcpStream,
    port: u16,
    runtime: &Arc<Mutex<LiveHttpRuntime>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let request = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(status) => return write_empty(stream, status),
    };
    if request.method != "GET" && request.method != "HEAD" {
        return write_empty(stream, "405 Method Not Allowed");
    }
    if !host_allowed(request.header("host"), port) || !origin_allowed(request.header("origin")) {
        return write_empty(stream, "403 Forbidden");
    }

    let Some(rest) = request.path.strip_prefix("/live/") else {
        return write_empty(stream, "404 Not Found");
    };
    let mut parts = rest.split('/');
    let Some(token) = parts.next() else {
        return write_empty(stream, "404 Not Found");
    };
    if token.is_empty() || Uuid::parse_str(token).is_err() {
        return write_empty(stream, "404 Not Found");
    }
    let session = {
        let runtime = runtime
            .lock()
            .map_err(|_| std::io::Error::other("live state unavailable"))?;
        let Some(session) = runtime.sessions.get(token) else {
            return write_empty(stream, "410 Gone");
        };
        if !session.is_active() {
            return write_empty(stream, "410 Gone");
        }
        session.clone()
    };

    trim_session_fragments(&session);
    let tail = parts.collect::<Vec<_>>();
    match tail.as_slice() {
        [] => {
            let fragments = list_live_fragments(&session.session_dir);
            let Some(latest) = fragments.last() else {
                return write_empty(stream, "425 Too Early");
            };
            serve_fragment(stream, &request, &session, &latest.name)
        }
        ["manifest"] => serve_manifest(stream, &request, token, &session),
        ["fragment", name] if parse_live_fragment_name(name).is_some() => {
            serve_fragment(stream, &request, &session, name)
        }
        _ => write_empty(stream, "404 Not Found"),
    }
}

fn serve_manifest(
    mut stream: TcpStream,
    request: &HttpRequest,
    token: &str,
    session: &Arc<HttpSession>,
) -> std::io::Result<()> {
    let fragments = list_live_fragments(&session.session_dir);
    let body = serde_json::to_vec(&serde_json::json!({
        "session_id": token,
        "fragments": fragments
            .iter()
            .filter(|fragment| fragment.bytes <= MAX_LIVE_FRAGMENT_BYTES)
            .map(|fragment| fragment.sequence)
            .collect::<Vec<_>>(),
    }))
    .map_err(|_| std::io::Error::other("live manifest encode failed"))?;
    if body.len() > MAX_LIVE_MANIFEST_BYTES {
        return write_empty(stream, "500 Internal Server Error");
    }
    write_http_body(
        &mut stream,
        request,
        "application/json",
        &body,
        request.header("origin"),
    )
}

fn serve_fragment(
    mut stream: TcpStream,
    request: &HttpRequest,
    session: &Arc<HttpSession>,
    fragment_name: &str,
) -> std::io::Result<()> {
    if parse_live_fragment_name(fragment_name).is_none() {
        return write_empty(stream, "404 Not Found");
    }
    let Some(_reader) = session.try_acquire_fragment(fragment_name) else {
        return write_empty(stream, "503 Service Unavailable");
    };
    let path = session.session_dir.join(fragment_name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            metadata
        }
        _ => return write_empty(stream, "404 Not Found"),
    };
    if metadata.len() > MAX_LIVE_FRAGMENT_BYTES {
        return write_empty(stream, "413 Content Too Large");
    }
    let file = File::open(path)?;
    let mut body = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_LIVE_FRAGMENT_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_LIVE_FRAGMENT_BYTES {
        return write_empty(stream, "413 Content Too Large");
    }
    write_http_body(
        &mut stream,
        request,
        "video/mp4",
        &body,
        request.header("origin"),
    )
}

fn write_http_body(
    stream: &mut TcpStream,
    request: &HttpRequest,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
) -> std::io::Result<()> {
    let origin_header = origin
        .map(|origin| format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"))
        .unwrap_or_default();
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{origin_header}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )?;
    if request.method == "GET" {
        stream.write_all(body)?;
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, &'static str> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).map_err(|_| "400 Bad Request")?;
        if count == 0 {
            return Err("400 Bad Request");
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_HTTP_HEADER_BYTES {
            return Err("431 Request Header Fields Too Large");
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "400 Bad Request")?;
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or("400 Bad Request")?;
    let mut first_parts = first.split_whitespace();
    let method = first_parts.next().ok_or("400 Bad Request")?.to_owned();
    let path = first_parts.next().ok_or("400 Bad Request")?.to_owned();
    if first_parts.next() != Some("HTTP/1.1") || first_parts.next().is_some() {
        return Err("400 Bad Request");
    }
    let mut headers = HashMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err("400 Bad Request");
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
    })
}

fn host_allowed(host: Option<&str>, port: u16) -> bool {
    matches!(
        host,
        Some(value)
            if value.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
                || value.eq_ignore_ascii_case(&format!("localhost:{port}"))
    )
}

fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin == "https://tauri.localhost"
        || origin.starts_with("http://localhost:")
        || origin.starts_with("https://localhost:")
}

fn write_empty(mut stream: TcpStream, status: &str) -> std::io::Result<()> {
    stream.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

enum WorkerMessage {
    Frame(Envelope),
    Eof,
    Error,
}

struct WorkerLiveRunner {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<WorkerMessage>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_request_id: u64,
    stop_signalled: bool,
    reaped: bool,
}

impl WorkerLiveRunner {
    fn spawn(program: &str) -> Result<Self, LiveError> {
        let mut command = Command::new(program);
        command
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let child = crate::worker_process::spawn_worker(&mut command)
            .map_err(|_| LiveError::WorkerUnavailable)?;
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Result<Self, LiveError> {
        let stdin = child.stdin.take().ok_or(LiveError::WorkerUnavailable)?;
        let stdout = child.stdout.take().ok_or(LiveError::WorkerUnavailable)?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("live-worker-stdout".to_owned())
            .spawn(move || {
                let mut reader = FramedReader::new(BufReader::new(stdout));
                loop {
                    match reader.next_message() {
                        Ok(Some(frame)) => {
                            if tx.send(WorkerMessage::Frame(frame)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ = tx.send(WorkerMessage::Eof);
                            return;
                        }
                        Err(_) => {
                            let _ = tx.send(WorkerMessage::Error);
                            return;
                        }
                    }
                }
            })
            .map_err(|_| LiveError::WorkerUnavailable)?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            rx,
            reader: Some(reader),
            next_request_id: 1,
            stop_signalled: false,
            reaped: false,
        })
    }

    fn wait_hello(&mut self, cancel: &AtomicBool) -> Result<(), LiveError> {
        let deadline = Instant::now() + WORKER_HELLO_TIMEOUT;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(LiveError::LifecycleBlocked);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LiveError::WorkerUnavailable);
            }
            let slice = remaining.min(Duration::from_millis(50));
            match self.rx.recv_timeout(slice) {
                Ok(WorkerMessage::Frame(Envelope::Event { v, name, data }))
                    if v == PROTOCOL_VERSION
                        && name == event::HELLO
                        && nian_ipc::validate_worker_hello(&data).is_ok() =>
                {
                    return Ok(());
                }
                Ok(WorkerMessage::Frame(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(WorkerMessage::Eof | WorkerMessage::Error)
                | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LiveError::WorkerUnavailable);
                }
            }
        }
    }

    fn request(
        &mut self,
        method_name: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LiveError> {
        self.request_cancelable(method_name, params, None)
    }

    fn request_cancelable(
        &mut self,
        method_name: &str,
        params: serde_json::Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<serde_json::Value, LiveError> {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            return Err(LiveError::LifecycleBlocked);
        }
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let stdin = self.stdin.as_mut().ok_or(LiveError::WorkerUnavailable)?;
        FramedWriter::new(stdin)
            .send(&Envelope::Request {
                v: PROTOCOL_VERSION,
                id,
                method: method_name.to_owned(),
                params,
            })
            .map_err(|_| LiveError::WorkerUnavailable)?;
        let deadline = Instant::now() + WORKER_REQUEST_TIMEOUT;
        loop {
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                return Err(LiveError::LifecycleBlocked);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LiveError::WorkerUnavailable);
            }
            let slice = remaining.min(Duration::from_millis(50));
            match self.rx.recv_timeout(slice) {
                Ok(WorkerMessage::Frame(Envelope::Response {
                    v,
                    id: response_id,
                    ok: true,
                    result,
                    ..
                })) if v == PROTOCOL_VERSION && response_id == id => return Ok(result),
                Ok(WorkerMessage::Frame(Envelope::Response {
                    v,
                    id: response_id,
                    ok: false,
                    error_code,
                    ..
                })) if v == PROTOCOL_VERSION && response_id == id => {
                    return Err(map_worker_error(
                        error_code.as_deref().unwrap_or("internal"),
                    ));
                }
                Ok(WorkerMessage::Frame(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(WorkerMessage::Eof | WorkerMessage::Error)
                | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LiveError::WorkerUnavailable);
                }
            }
        }
    }

    fn signal_request(&mut self, method_name: &str) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let _ = FramedWriter::new(stdin).send(&Envelope::Request {
            v: PROTOCOL_VERSION,
            id,
            method: method_name.to_owned(),
            params: serde_json::json!({}),
        });
    }

    fn signal_stop(&mut self) {
        if self.stop_signalled {
            return;
        }
        self.stop_signalled = true;
        self.signal_request("live.stop");
        self.signal_request(method::SHUTDOWN);
    }

    fn reap(&mut self) {
        if self.reaped {
            return;
        }
        self.signal_stop();
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
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
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.reaped = true;
    }
}

impl LiveRunner for WorkerLiveRunner {
    fn start(
        &mut self,
        source_json: serde_json::Value,
        output_dir: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), LiveError> {
        if cancel.load(Ordering::Acquire) {
            return Err(LiveError::LifecycleBlocked);
        }
        self.wait_hello(&cancel)?;
        let result = self.request_cancelable(
            "live.start",
            serde_json::json!({
                "source": source_json,
                "output_dir": output_dir.to_string_lossy(),
                "fragment_target_ms": LIVE_FRAGMENT_TARGET.as_millis() as u64,
                "max_fragment_bytes": MAX_LIVE_FRAGMENT_BYTES,
                "max_fragment_count": MAX_LIVE_CACHE_FRAGMENTS,
            }),
            Some(&cancel),
        )?;
        if result.get("started").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(LiveError::WorkerUnavailable);
        }
        Ok(())
    }

    fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
        let value = self.request("live.status", serde_json::json!({}))?;
        serde_json::from_value(value).map_err(|_| LiveError::WorkerUnavailable)
    }

    fn request_stop(&mut self) {
        self.signal_stop();
    }

    fn join_or_reap(&mut self) {
        self.reap();
    }
}

impl Drop for WorkerLiveRunner {
    fn drop(&mut self) {
        self.reap();
    }
}

fn map_worker_error(code: &str) -> LiveError {
    match code.split(':').next().unwrap_or(code) {
        "source_open_failed" => LiveError::SourceOpenFailed,
        "unsupported_codec" => LiveError::UnsupportedCodec,
        "media_failed" => LiveError::MediaFailed,
        "worker_unavailable" => LiveError::WorkerUnavailable,
        _ => LiveError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeFactory;

    struct FakeRunner {
        status: Result<LiveWorkerStatus, LiveError>,
    }

    impl LiveRunner for FakeRunner {
        fn start(
            &mut self,
            _source_json: serde_json::Value,
            output_dir: &Path,
            _cancel: Arc<AtomicBool>,
        ) -> Result<(), LiveError> {
            std::fs::write(output_dir.join("fragment-000000000000.mp4"), b"fake-live")
                .map_err(|_| LiveError::Internal)
        }

        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            self.status.clone()
        }

        fn request_stop(&mut self) {}

        fn join_or_reap(&mut self) {}
    }

    impl LiveRunnerFactory for FakeFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            Ok(Box::new(FakeRunner {
                status: Ok(LiveWorkerStatus {
                    state: LiveState::Live,
                    failure_category: None,
                    reconnect_attempt: 0,
                }),
            }))
        }
    }

    #[derive(Clone)]
    struct StopCountingFactory {
        stops: Arc<AtomicUsize>,
    }

    struct StopCountingRunner {
        stops: Arc<AtomicUsize>,
        signalled: bool,
    }

    impl LiveRunner for StopCountingRunner {
        fn start(
            &mut self,
            _source_json: serde_json::Value,
            output_dir: &Path,
            _cancel: Arc<AtomicBool>,
        ) -> Result<(), LiveError> {
            std::fs::write(output_dir.join("fragment-000000000000.mp4"), b"fake-live")
                .map_err(|_| LiveError::Internal)
        }

        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            Ok(LiveWorkerStatus {
                state: LiveState::Live,
                failure_category: None,
                reconnect_attempt: 0,
            })
        }

        fn request_stop(&mut self) {
            if !self.signalled {
                self.signalled = true;
                self.stops.fetch_add(1, Ordering::AcqRel);
            }
        }

        fn join_or_reap(&mut self) {}
    }

    impl LiveRunnerFactory for StopCountingFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            Ok(Box::new(StopCountingRunner {
                stops: self.stops.clone(),
                signalled: false,
            }))
        }
    }

    struct IsolatingFactory {
        starts: AtomicUsize,
    }

    impl LiveRunnerFactory for IsolatingFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            let status = if self.starts.fetch_add(1, Ordering::AcqRel) == 0 {
                Err(LiveError::WorkerUnavailable)
            } else {
                Ok(LiveWorkerStatus {
                    state: LiveState::Live,
                    failure_category: None,
                    reconnect_attempt: 0,
                })
            };
            Ok(Box::new(FakeRunner { status }))
        }
    }

    fn prepared(camera: &str) -> PreparedLive {
        PreparedLive {
            camera_id: CameraId::parse(camera).unwrap(),
            source_json: serde_json::json!({"kind":"rtsp","url":"rtsp://hidden"}),
        }
    }

    fn http_status(
        controller: &LiveViewController,
        method: &str,
        path: &str,
        host: &str,
        origin: Option<&str>,
    ) -> String {
        let port = controller.server.lock().unwrap().port;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let origin = origin
            .map(|value| format!("Origin: {value}\r\n"))
            .unwrap_or_default();
        let request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{origin}\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response.lines().next().unwrap_or_default().to_owned()
    }

    #[test]
    fn sessions_are_per_camera_and_capacity_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let first = controller.open(prepared("a")).unwrap();
        assert!(matches!(
            controller.open(prepared("a")),
            Err(LiveError::AlreadyOpen)
        ));
        for camera in ["b", "c", "d"] {
            controller.open(prepared(camera)).unwrap();
        }
        assert!(matches!(
            controller.open(prepared("e")),
            Err(LiveError::Capacity)
        ));
        controller.close(&first.session_id).unwrap();
        assert_eq!(controller.statuses().unwrap().len(), 3);
    }

    #[test]
    fn keepalive_and_close_use_opaque_session_identity() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        assert!(Uuid::parse_str(&opened.session_id).is_ok());
        assert!(opened.url.contains("127.0.0.1"));
        assert!(!opened.url.contains("rtsp"));
        controller.keep_alive(&opened.session_id).unwrap();
        controller.close(&opened.session_id).unwrap();
        assert_eq!(
            controller.keep_alive(&opened.session_id),
            Err(LiveError::SessionExpired)
        );
    }

    #[test]
    fn close_all_releases_all_camera_owners() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        controller.open(prepared("a")).unwrap();
        controller.open(prepared("b")).unwrap();
        controller.close_all();
        assert!(controller.statuses().unwrap().is_empty());
        controller.open(prepared("a")).unwrap();
    }

    #[test]
    fn admission_and_started_handle_drop_release_reservations() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();

        let admission = controller.admit(prepared("a")).unwrap();
        assert!(matches!(
            controller.admit(prepared("a")),
            Err(LiveError::AlreadyOpen)
        ));
        drop(admission);
        controller.open(prepared("a")).unwrap();
        controller.close_all();

        let started = controller.admit(prepared("b")).unwrap().start().unwrap();
        assert!(matches!(
            controller.admit(prepared("b")),
            Err(LiveError::AlreadyOpen)
        ));
        drop(started);
        controller.open(prepared("b")).unwrap();
    }

    #[test]
    fn started_reservations_hold_capacity_until_commit_or_drop() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();

        let started: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|camera| controller.admit(prepared(camera)).unwrap().start().unwrap())
            .collect();
        assert!(matches!(
            controller.admit(prepared("e")),
            Err(LiveError::Capacity)
        ));

        drop(started);
        controller.open(prepared("e")).unwrap();
    }

    #[test]
    fn stale_started_commit_cannot_erase_a_newer_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();

        let stale = controller.admit(prepared("a")).unwrap().start().unwrap();
        controller.close_all();
        let fresh = controller.admit(prepared("a")).unwrap();

        assert_eq!(
            controller.commit_open(stale),
            Err(LiveError::LifecycleBlocked)
        );
        assert!(matches!(
            controller.admit(prepared("a")),
            Err(LiveError::AlreadyOpen)
        ));

        let opened = controller.commit_open(fresh.start().unwrap()).unwrap();
        assert_eq!(opened.camera_id, "a");
    }

    #[test]
    fn one_worker_status_failure_does_not_hide_other_camera_statuses() {
        let temp = tempfile::tempdir().unwrap();
        let controller = LiveViewController::with_factory(
            Arc::new(IsolatingFactory {
                starts: AtomicUsize::new(0),
            }),
            temp.path().to_path_buf(),
        )
        .unwrap();
        controller.open(prepared("a")).unwrap();
        controller.open(prepared("b")).unwrap();

        let statuses = controller.statuses().unwrap();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].camera_id, "a");
        assert_eq!(statuses[0].state, LiveState::Failed);
        assert_eq!(
            statuses[0].failure_category,
            Some(LiveFailureCategory::WorkerUnavailable)
        );
        assert_eq!(statuses[1].camera_id, "b");
        assert_eq!(statuses[1].state, LiveState::Live);
    }

    #[test]
    fn background_reaper_expires_abandoned_session_without_controller_traffic() {
        let temp = tempfile::tempdir().unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let controller = LiveViewController::with_factory_and_timeout(
            Arc::new(StopCountingFactory {
                stops: stops.clone(),
            }),
            temp.path().to_path_buf(),
            Duration::from_millis(60),
        )
        .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        let session_dir = temp.path().join(format!("session-{}", opened.session_id));

        std::thread::sleep(Duration::from_millis(35));
        controller.keep_alive(&opened.session_id).unwrap();
        std::thread::sleep(Duration::from_millis(35));
        assert_eq!(stops.load(Ordering::Acquire), 0);

        let deadline = Instant::now() + Duration::from_secs(1);
        while (stops.load(Ordering::Acquire) == 0 || session_dir.exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(stops.load(Ordering::Acquire), 1);
        assert!(!session_dir.exists());

        let port = controller.server.lock().unwrap().port;
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &format!("/live/{}", opened.session_id),
                &format!("127.0.0.1:{port}"),
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 410 Gone"
        );
    }

    #[test]
    fn loopback_endpoint_is_session_scoped_and_not_a_general_proxy() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        let port = controller.server.lock().unwrap().port;
        let host = format!("127.0.0.1:{port}");
        let path = format!("/live/{}", opened.session_id);

        assert_eq!(
            http_status(&controller, "HEAD", &path, &host, Some("tauri://localhost")),
            "HTTP/1.1 200 OK"
        );
        assert_eq!(
            http_status(&controller, "POST", &path, &host, Some("tauri://localhost")),
            "HTTP/1.1 405 Method Not Allowed"
        );
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &path,
                "camera.lan:554",
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 403 Forbidden"
        );
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                "/proxy/rtsp://192.168.1.10/stream",
                &host,
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 404 Not Found"
        );
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                "/live/not-a-uuid",
                &host,
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 404 Not Found"
        );

        controller.close(&opened.session_id).unwrap();
        assert_eq!(
            http_status(&controller, "HEAD", &path, &host, Some("tauri://localhost")),
            "HTTP/1.1 410 Gone"
        );
    }

    fn write_test_fragment(dir: &Path, sequence: u64, bytes: usize) -> String {
        let name = format!("fragment-{sequence:012}.mp4");
        std::fs::write(dir.join(&name), vec![sequence as u8; bytes]).unwrap();
        name
    }

    fn committed_session(controller: &LiveViewController, session_id: &str) -> Arc<LiveSession> {
        controller
            .registry
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .cloned()
            .unwrap()
    }

    #[derive(Clone)]
    struct OrderedTeardownFactory {
        next: Arc<AtomicUsize>,
        signals: Arc<AtomicUsize>,
        join_signal_snapshots: Arc<Mutex<Vec<usize>>>,
    }

    struct OrderedTeardownRunner {
        id: usize,
        signalled: bool,
        signals: Arc<AtomicUsize>,
        join_signal_snapshots: Arc<Mutex<Vec<usize>>>,
    }

    impl LiveRunner for OrderedTeardownRunner {
        fn start(
            &mut self,
            _source_json: serde_json::Value,
            output_dir: &Path,
            _cancel: Arc<AtomicBool>,
        ) -> Result<(), LiveError> {
            write_test_fragment(output_dir, 0, 32);
            Ok(())
        }

        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            Ok(LiveWorkerStatus {
                state: LiveState::Live,
                failure_category: None,
                reconnect_attempt: 0,
            })
        }

        fn request_stop(&mut self) {
            if !self.signalled {
                self.signalled = true;
                self.signals.fetch_add(1, Ordering::AcqRel);
            }
        }

        fn join_or_reap(&mut self) {
            self.join_signal_snapshots
                .lock()
                .unwrap()
                .push(self.signals.load(Ordering::Acquire));
            if self.id == 0 {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
    }

    impl LiveRunnerFactory for OrderedTeardownFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            let id = self.next.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(OrderedTeardownRunner {
                id,
                signalled: false,
                signals: self.signals.clone(),
                join_signal_snapshots: self.join_signal_snapshots.clone(),
            }))
        }
    }

    #[derive(Default)]
    struct BlockingTeardownControl {
        signals: AtomicUsize,
        completed: AtomicUsize,
        join_started: Mutex<usize>,
        join_started_cv: Condvar,
        released: Mutex<bool>,
        released_cv: Condvar,
    }

    impl BlockingTeardownControl {
        fn wait_for_join_starts(&self, expected: usize) {
            let mut started = self.join_started.lock().unwrap();
            while *started < expected {
                started = self.join_started_cv.wait(started).unwrap();
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.released_cv.notify_all();
        }
    }

    #[derive(Clone)]
    struct BlockingTeardownFactory {
        control: Arc<BlockingTeardownControl>,
    }

    struct BlockingTeardownRunner {
        control: Arc<BlockingTeardownControl>,
        signalled: bool,
    }

    impl LiveRunner for BlockingTeardownRunner {
        fn start(
            &mut self,
            _source_json: serde_json::Value,
            output_dir: &Path,
            _cancel: Arc<AtomicBool>,
        ) -> Result<(), LiveError> {
            write_test_fragment(output_dir, 0, 32);
            Ok(())
        }

        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            Ok(LiveWorkerStatus {
                state: LiveState::Live,
                failure_category: None,
                reconnect_attempt: 0,
            })
        }

        fn request_stop(&mut self) {
            if !self.signalled {
                self.signalled = true;
                self.control.signals.fetch_add(1, Ordering::AcqRel);
            }
        }

        fn join_or_reap(&mut self) {
            {
                let mut started = self.control.join_started.lock().unwrap();
                *started += 1;
                self.control.join_started_cv.notify_all();
            }
            let mut released = self.control.released.lock().unwrap();
            while !*released {
                released = self.control.released_cv.wait(released).unwrap();
            }
            self.control.completed.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl LiveRunnerFactory for BlockingTeardownFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            Ok(Box::new(BlockingTeardownRunner {
                control: self.control.clone(),
                signalled: false,
            }))
        }
    }

    #[derive(Clone)]
    struct CancelBlockingFactory {
        entered: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        return_runner_after_cancel: bool,
        signals: Arc<AtomicUsize>,
        joins: Arc<AtomicUsize>,
    }

    struct CancelAwareRunner {
        entered: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        succeed_after_cancel: bool,
        signalled: bool,
        signals: Arc<AtomicUsize>,
        joins: Arc<AtomicUsize>,
    }

    impl LiveRunner for CancelAwareRunner {
        fn start(
            &mut self,
            _source_json: serde_json::Value,
            output_dir: &Path,
            cancel: Arc<AtomicBool>,
        ) -> Result<(), LiveError> {
            self.entered.fetch_add(1, Ordering::AcqRel);
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.cancelled.fetch_add(1, Ordering::AcqRel);
            if self.succeed_after_cancel {
                write_test_fragment(output_dir, 0, 32);
                Ok(())
            } else {
                Err(LiveError::LifecycleBlocked)
            }
        }

        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            Ok(LiveWorkerStatus {
                state: LiveState::Starting,
                failure_category: None,
                reconnect_attempt: 0,
            })
        }

        fn request_stop(&mut self) {
            if !self.signalled {
                self.signalled = true;
                self.signals.fetch_add(1, Ordering::AcqRel);
            }
        }

        fn join_or_reap(&mut self) {
            self.joins.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl LiveRunnerFactory for CancelBlockingFactory {
        fn spawn(&self) -> Result<Box<dyn LiveRunner>, LiveError> {
            Ok(Box::new(CancelAwareRunner {
                entered: self.entered.clone(),
                cancelled: self.cancelled.clone(),
                succeed_after_cancel: self.return_runner_after_cancel,
                signalled: false,
                signals: self.signals.clone(),
                joins: self.joins.clone(),
            }))
        }
    }

    #[test]
    fn rolling_fragment_retention_reclaims_old_output_and_stays_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        let session = committed_session(&controller, &opened.session_id);
        for sequence in 1..=80 {
            write_test_fragment(&session.temp_dir, sequence, 1024);
        }

        trim_session_fragments(&session.http);
        let fragments = list_live_fragments(&session.temp_dir);
        assert_eq!(fragments.len(), MAX_RETAINED_LIVE_FRAGMENTS);
        assert_eq!(fragments.first().unwrap().sequence, 75);
        assert!(
            fragments.iter().map(|fragment| fragment.bytes).sum::<u64>()
                <= MAX_RETAINED_LIVE_FRAGMENTS as u64 * MAX_LIVE_FRAGMENT_BYTES
        );
    }

    #[test]
    fn fragment_with_active_reader_is_reclaimed_only_after_reader_release() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        let session = committed_session(&controller, &opened.session_id);
        for sequence in 1..=10 {
            write_test_fragment(&session.temp_dir, sequence, 1024);
        }
        let oldest = "fragment-000000000000.mp4";
        let reader = session.http.try_acquire_fragment(oldest).unwrap();
        trim_session_fragments(&session.http);
        assert!(session.temp_dir.join(oldest).exists());
        assert!(list_live_fragments(&session.temp_dir).len() <= MAX_RETAINED_LIVE_FRAGMENTS + 1);

        drop(reader);
        assert!(!session.temp_dir.join(oldest).exists());
        assert_eq!(
            list_live_fragments(&session.temp_dir).len(),
            MAX_RETAINED_LIVE_FRAGMENTS
        );
    }

    #[test]
    fn closing_session_removes_transient_cache_directory() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller.open(prepared("front-door")).unwrap();
        let session_dir = temp.path().join(format!("session-{}", opened.session_id));
        assert!(session_dir.exists());
        controller.close(&opened.session_id).unwrap();
        assert!(!session_dir.exists());
    }

    #[test]
    fn startup_cleans_only_provably_owned_abandoned_session_directories() {
        let temp = tempfile::tempdir().unwrap();
        let stale_id = Uuid::new_v4().to_string();
        let stale = temp.path().join(format!("session-{stale_id}"));
        let lookalike = temp.path().join("session-not-a-uuid");
        let unrelated = temp.path().join("keep-me.txt");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("fragment-000000000001.mp4"), b"old").unwrap();
        std::fs::create_dir(&lookalike).unwrap();
        std::fs::write(&unrelated, b"foreign").unwrap();

        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        assert!(!stale.exists());
        assert!(lookalike.exists());
        assert!(unrelated.exists());
        drop(controller);
        assert!(lookalike.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn four_sessions_are_independently_fragment_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = ["a", "b", "c", "d"]
            .into_iter()
            .map(|camera| controller.open(prepared(camera)).unwrap())
            .collect::<Vec<_>>();
        for live in &opened {
            let session = committed_session(&controller, &live.session_id);
            for sequence in 1..=32 {
                write_test_fragment(&session.temp_dir, sequence, 512);
            }
            trim_session_fragments(&session.http);
            assert_eq!(
                list_live_fragments(&session.temp_dir).len(),
                MAX_RETAINED_LIVE_FRAGMENTS
            );
        }
    }

    #[test]
    fn close_all_signals_every_worker_before_any_join_wait() {
        let temp = tempfile::tempdir().unwrap();
        let signals = Arc::new(AtomicUsize::new(0));
        let join_signal_snapshots = Arc::new(Mutex::new(Vec::new()));
        let controller = LiveViewController::with_factory(
            Arc::new(OrderedTeardownFactory {
                next: Arc::new(AtomicUsize::new(0)),
                signals: signals.clone(),
                join_signal_snapshots: join_signal_snapshots.clone(),
            }),
            temp.path().to_path_buf(),
        )
        .unwrap();
        for camera in ["a", "b", "c", "d"] {
            controller.open(prepared(camera)).unwrap();
        }

        controller.close_all();
        assert_eq!(signals.load(Ordering::Acquire), 4);
        let snapshots = join_signal_snapshots.lock().unwrap().clone();
        assert_eq!(snapshots.len(), 4);
        assert!(
            snapshots
                .into_iter()
                .all(|signals_before_join| signals_before_join == 4)
        );
    }

    #[test]
    fn close_draining_owner_remains_tracked_and_shutdown_waits_for_reap() {
        let temp = tempfile::tempdir().unwrap();
        let control = Arc::new(BlockingTeardownControl::default());
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(BlockingTeardownFactory {
                    control: control.clone(),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        let opened = controller.open(prepared("a")).unwrap();
        let close_controller = controller.clone();
        let session_id = opened.session_id.clone();
        let close_thread = std::thread::spawn(move || close_controller.close(&session_id));
        control.wait_for_join_starts(1);

        {
            let registry = controller.registry.lock().unwrap();
            assert!(registry.sessions.is_empty());
            assert!(registry.draining_sessions.contains_key(&opened.session_id));
        }

        let (done_tx, done_rx) = mpsc::channel();
        let shutdown_controller = controller.clone();
        let shutdown_thread = std::thread::spawn(move || {
            shutdown_controller.shutdown();
            let _ = done_tx.send(());
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());

        control.release();
        assert_eq!(close_thread.join().unwrap(), Ok(()));
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        shutdown_thread.join().unwrap();
        let registry = controller.registry.lock().unwrap();
        assert!(registry.sessions.is_empty());
        assert!(registry.opening.is_empty());
        assert!(registry.draining_sessions.is_empty());
        assert!(registry.draining_openings.is_empty());
        assert_eq!(control.signals.load(Ordering::Acquire), 1);
        assert_eq!(control.completed.load(Ordering::Acquire), 1);
    }

    #[test]
    fn close_all_keeps_all_workers_draining_until_parallel_reap_and_shutdown_waits() {
        let temp = tempfile::tempdir().unwrap();
        let control = Arc::new(BlockingTeardownControl::default());
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(BlockingTeardownFactory {
                    control: control.clone(),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        for camera in ["a", "b", "c", "d"] {
            controller.open(prepared(camera)).unwrap();
        }

        let close_controller = controller.clone();
        let close_thread = std::thread::spawn(move || close_controller.close_all());
        control.wait_for_join_starts(4);
        assert_eq!(control.signals.load(Ordering::Acquire), 4);
        {
            let registry = controller.registry.lock().unwrap();
            assert!(registry.sessions.is_empty());
            assert_eq!(registry.draining_sessions.len(), 4);
        }

        let (done_tx, done_rx) = mpsc::channel();
        let shutdown_controller = controller.clone();
        let shutdown_thread = std::thread::spawn(move || {
            shutdown_controller.shutdown();
            let _ = done_tx.send(());
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());

        control.release();
        close_thread.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        shutdown_thread.join().unwrap();
        let registry = controller.registry.lock().unwrap();
        assert!(registry.opening.is_empty());
        assert!(registry.sessions.is_empty());
        assert!(registry.draining_openings.is_empty());
        assert!(registry.draining_sessions.is_empty());
        assert_eq!(control.signals.load(Ordering::Acquire), 4);
        assert_eq!(control.completed.load(Ordering::Acquire), 4);
    }

    #[test]
    fn stale_draining_completion_does_not_remove_newer_same_camera_session() {
        let temp = tempfile::tempdir().unwrap();
        let control = Arc::new(BlockingTeardownControl::default());
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(BlockingTeardownFactory {
                    control: control.clone(),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        let old = controller.open(prepared("a")).unwrap();
        let close_controller = controller.clone();
        let old_session_id = old.session_id.clone();
        let close_thread = std::thread::spawn(move || close_controller.close(&old_session_id));
        control.wait_for_join_starts(1);

        let fresh = controller.open(prepared("a")).unwrap();
        assert_ne!(fresh.session_id, old.session_id);
        {
            let registry = controller.registry.lock().unwrap();
            assert!(registry.draining_sessions.contains_key(&old.session_id));
            assert!(registry.sessions.contains_key(&fresh.session_id));
            assert_eq!(
                registry.camera_sessions.get(&CameraId::parse("a").unwrap()),
                Some(&fresh.session_id)
            );
        }

        control.release();
        assert_eq!(close_thread.join().unwrap(), Ok(()));
        let statuses = controller.statuses().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].session_id, fresh.session_id);
        let registry = controller.registry.lock().unwrap();
        assert!(!registry.draining_sessions.contains_key(&old.session_id));
        assert!(registry.sessions.contains_key(&fresh.session_id));
    }

    #[test]
    fn hide_capture_freezes_ownership_before_reactivation_and_preserves_fresh_capability() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let old = controller.open(prepared("a")).unwrap();
        let port = controller.server.lock().unwrap().port;
        let host = format!("127.0.0.1:{port}");
        let old_path = format!("/live/{}", old.session_id);
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &old_path,
                &host,
                Some("tauri://localhost")
            ),
            "HTTP/1.1 200 OK"
        );

        controller.stop_accepting();
        let hide_batch = controller.begin_close_all();
        assert!(controller.statuses().unwrap().is_empty());
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &old_path,
                &host,
                Some("tauri://localhost")
            ),
            "HTTP/1.1 410 Gone"
        );

        // Deliberately reactivate before the old teardown batch is even started.
        controller.resume_accepting();
        let fresh = controller.open(prepared("a")).unwrap();
        let fresh_path = format!("/live/{}", fresh.session_id);
        assert_ne!(fresh.session_id, old.session_id);
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &fresh_path,
                &host,
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 200 OK"
        );

        controller.finish_close_all(hide_batch);

        let statuses = controller.statuses().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].session_id, fresh.session_id);
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &fresh_path,
                &host,
                Some("tauri://localhost"),
            ),
            "HTTP/1.1 200 OK"
        );
        assert_eq!(
            http_status(
                &controller,
                "HEAD",
                &old_path,
                &host,
                Some("tauri://localhost")
            ),
            "HTTP/1.1 410 Gone"
        );
        let registry = controller.registry.lock().unwrap();
        assert_eq!(
            registry.camera_sessions.get(&CameraId::parse("a").unwrap()),
            Some(&fresh.session_id)
        );
        assert!(registry.sessions.contains_key(&fresh.session_id));
    }

    #[test]
    fn asynchronous_hide_teardown_cannot_stop_fresh_same_camera_session() {
        let temp = tempfile::tempdir().unwrap();
        let control = Arc::new(BlockingTeardownControl::default());
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(BlockingTeardownFactory {
                    control: control.clone(),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        let old = controller.open(prepared("a")).unwrap();
        controller.stop_accepting();
        let hide_batch = controller.begin_close_all();

        let hide_controller = controller.clone();
        let hide_thread = std::thread::spawn(move || hide_controller.finish_close_all(hide_batch));
        control.wait_for_join_starts(1);

        controller.resume_accepting();
        let fresh = controller.open(prepared("a")).unwrap();
        assert_ne!(fresh.session_id, old.session_id);
        assert_eq!(
            controller.statuses().unwrap()[0].session_id,
            fresh.session_id
        );

        control.release();
        hide_thread.join().unwrap();

        let statuses = controller.statuses().unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].session_id, fresh.session_id);
        let registry = controller.registry.lock().unwrap();
        assert!(!registry.draining_sessions.contains_key(&old.session_id));
        assert!(registry.sessions.contains_key(&fresh.session_id));
        assert_eq!(control.signals.load(Ordering::Acquire), 1);
        assert_eq!(control.completed.load(Ordering::Acquire), 1);
    }

    #[test]
    fn draining_hide_batch_continues_to_count_against_live_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        for camera in ["a", "b", "c", "d"] {
            controller.open(prepared(camera)).unwrap();
        }

        controller.stop_accepting();
        let hide_batch = controller.begin_close_all();
        controller.resume_accepting();
        assert!(matches!(
            controller.open(prepared("e")),
            Err(LiveError::Capacity)
        ));

        controller.finish_close_all(hide_batch);
        let fresh = controller.open(prepared("e")).unwrap();
        assert_eq!(fresh.camera_id, "e");
    }

    #[test]
    fn keepalive_does_not_synchronously_reap_an_unrelated_expired_session() {
        let temp = tempfile::tempdir().unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let controller = LiveViewController::with_factory_and_timeout(
            Arc::new(StopCountingFactory {
                stops: stops.clone(),
            }),
            temp.path().to_path_buf(),
            Duration::from_secs(5),
        )
        .unwrap();
        let expired = controller.open(prepared("a")).unwrap();
        let healthy = controller.open(prepared("b")).unwrap();
        let expired_session = committed_session(&controller, &expired.session_id);
        *expired_session.last_keepalive.lock().unwrap() = Instant::now() - Duration::from_secs(10);

        controller.keep_alive(&healthy.session_id).unwrap();
        assert_eq!(stops.load(Ordering::Acquire), 0);
    }

    #[test]
    fn lifecycle_cancels_and_waits_for_all_inflight_openings() {
        let temp = tempfile::tempdir().unwrap();
        let entered = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(CancelBlockingFactory {
                    entered: entered.clone(),
                    cancelled: cancelled.clone(),
                    return_runner_after_cancel: false,
                    signals: Arc::new(AtomicUsize::new(0)),
                    joins: Arc::new(AtomicUsize::new(0)),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        let admissions = ["a", "b", "c", "d"]
            .into_iter()
            .map(|camera| controller.admit(prepared(camera)).unwrap())
            .collect::<Vec<_>>();
        let threads = admissions
            .into_iter()
            .map(|admission| std::thread::spawn(move || admission.start()))
            .collect::<Vec<_>>();
        let deadline = Instant::now() + Duration::from_secs(1);
        while entered.load(Ordering::Acquire) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(entered.load(Ordering::Acquire), 4);

        controller.stop_accepting();
        controller.close_all();
        for thread in threads {
            assert!(matches!(
                thread.join().unwrap(),
                Err(LiveOpenStartError {
                    error: LiveError::LifecycleBlocked,
                    ..
                })
            ));
        }
        assert_eq!(cancelled.load(Ordering::Acquire), 4);
        assert!(controller.registry.lock().unwrap().opening.is_empty());
        assert!(controller.registry.lock().unwrap().sessions.is_empty());
        assert!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with("session-"))
        );
        controller.resume_accepting();
        let fresh = controller.admit(prepared("e")).unwrap();
        drop(fresh);
    }

    #[test]
    fn late_runner_after_lifecycle_cancel_is_signalled_reaped_and_cannot_commit() {
        let temp = tempfile::tempdir().unwrap();
        let entered = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let signals = Arc::new(AtomicUsize::new(0));
        let joins = Arc::new(AtomicUsize::new(0));
        let controller = Arc::new(
            LiveViewController::with_factory(
                Arc::new(CancelBlockingFactory {
                    entered: entered.clone(),
                    cancelled: cancelled.clone(),
                    return_runner_after_cancel: true,
                    signals: signals.clone(),
                    joins: joins.clone(),
                }),
                temp.path().to_path_buf(),
            )
            .unwrap(),
        );
        let admission = controller.admit(prepared("a")).unwrap();
        let thread = std::thread::spawn(move || admission.start());
        let deadline = Instant::now() + Duration::from_secs(1);
        while entered.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }

        controller.stop_accepting();
        controller.close_all();
        assert!(matches!(
            thread.join().unwrap(),
            Err(LiveOpenStartError {
                error: LiveError::LifecycleBlocked,
                ..
            })
        ));
        assert_eq!(cancelled.load(Ordering::Acquire), 1);
        assert_eq!(signals.load(Ordering::Acquire), 1);
        assert_eq!(joins.load(Ordering::Acquire), 1);
        assert!(controller.registry.lock().unwrap().sessions.is_empty());
        assert!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with("session-"))
        );
    }

    #[test]
    fn frontend_live_dto_never_contains_authenticated_source_url() {
        let temp = tempfile::tempdir().unwrap();
        let controller =
            LiveViewController::with_factory(Arc::new(FakeFactory), temp.path().to_path_buf())
                .unwrap();
        let opened = controller
            .open(PreparedLive {
                camera_id: CameraId::parse("front-door").unwrap(),
                source_json: serde_json::json!({
                    "kind": "rtsp",
                    "url": "rtsp://camera-user:super-secret@192.168.1.50/stream1"
                }),
            })
            .unwrap();

        let serialized = serde_json::to_string(&opened).unwrap();
        assert!(!serialized.contains("super-secret"));
        assert!(!serialized.contains("camera-user"));
        assert!(!serialized.contains("rtsp://"));
        assert!(serialized.contains("127.0.0.1"));
    }
}
