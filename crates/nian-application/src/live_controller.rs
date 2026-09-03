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
use std::sync::{Arc, Mutex, Weak, mpsc};
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
const MAX_HTTP_REQUESTS: usize = 8;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

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

pub struct LiveOpenAdmission {
    session_id: String,
    camera_id: CameraId,
    source_json: serde_json::Value,
    temp_dir: PathBuf,
    media_path: PathBuf,
    factory: Arc<dyn LiveRunnerFactory>,
    registry: Weak<Mutex<LiveRegistry>>,
    cleanup_on_drop: bool,
}

impl LiveOpenAdmission {
    pub fn start(mut self) -> Result<StartedLiveOpen, LiveOpenStartError> {
        let result = self
            .factory
            .start(std::mem::take(&mut self.source_json), &self.media_path);
        match result {
            Ok(runner) => {
                self.cleanup_on_drop = false;
                Ok(StartedLiveOpen {
                    session_id: self.session_id.clone(),
                    camera_id: self.camera_id.clone(),
                    temp_dir: self.temp_dir.clone(),
                    media_path: self.media_path.clone(),
                    runner: Some(runner),
                    registry: self.registry.clone(),
                    cleanup_on_drop: true,
                })
            }
            Err(error) => Err(LiveOpenStartError {
                session_id: self.session_id.clone(),
                camera_id: self.camera_id.clone(),
                error,
            }),
        }
    }
}

impl Drop for LiveOpenAdmission {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = std::fs::remove_dir_all(&self.temp_dir);
            if let Some(registry) = self.registry.upgrade()
                && let Ok(mut registry) = registry.lock()
                && registry.opening.get(&self.camera_id) == Some(&self.session_id)
            {
                registry.opening.remove(&self.camera_id);
            }
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
    session_id: String,
    camera_id: CameraId,
    temp_dir: PathBuf,
    media_path: PathBuf,
    runner: Option<Box<dyn LiveRunner>>,
    registry: Weak<Mutex<LiveRegistry>>,
    cleanup_on_drop: bool,
}

impl Drop for StartedLiveOpen {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            if let Some(registry) = self.registry.upgrade()
                && let Ok(mut registry) = registry.lock()
                && registry.opening.get(&self.camera_id) == Some(&self.session_id)
            {
                registry.opening.remove(&self.camera_id);
            }
            if let Some(mut runner) = self.runner.take() {
                runner.stop();
            }
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }
}

pub trait LiveRunner: Send {
    fn status(&mut self) -> Result<LiveWorkerStatus, LiveError>;
    fn stop(&mut self);
}

pub trait LiveRunnerFactory: Send + Sync {
    fn start(
        &self,
        source_json: serde_json::Value,
        output_path: &Path,
    ) -> Result<Box<dyn LiveRunner>, LiveError>;
}

#[derive(Debug, Clone)]
pub struct WorkerLiveRunnerFactory {
    pub worker_program: String,
}

impl LiveRunnerFactory for WorkerLiveRunnerFactory {
    fn start(
        &self,
        source_json: serde_json::Value,
        output_path: &Path,
    ) -> Result<Box<dyn LiveRunner>, LiveError> {
        WorkerLiveRunner::start(&self.worker_program, source_json, output_path)
            .map(|runner| Box::new(runner) as Box<dyn LiveRunner>)
    }
}

struct LiveSession {
    camera_id: CameraId,
    temp_dir: PathBuf,
    last_keepalive: Mutex<Instant>,
    runner: Mutex<Box<dyn LiveRunner>>,
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        if let Ok(runner) = self.runner.get_mut() {
            runner.stop();
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[derive(Default)]
struct LiveRegistry {
    sessions: HashMap<String, Arc<LiveSession>>,
    camera_sessions: HashMap<CameraId, String>,
    opening: HashMap<CameraId, String>,
    accepting: bool,
}

#[derive(Clone)]
struct HttpSession {
    media_path: PathBuf,
    active: bool,
}

#[derive(Default)]
struct LiveHttpRuntime {
    sessions: HashMap<String, HttpSession>,
}

pub struct LiveViewController {
    factory: Arc<dyn LiveRunnerFactory>,
    registry: Arc<Mutex<LiveRegistry>>,
    http_runtime: Arc<Mutex<LiveHttpRuntime>>,
    server: Mutex<LiveHttpServer>,
    reaper: Mutex<LiveSessionReaper>,
    cache_root: PathBuf,
    session_timeout: Duration,
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
            session_timeout,
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
        self.expire_sessions();
        let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
        if !registry.accepting {
            return Err(LiveError::LifecycleBlocked);
        }
        if registry.camera_sessions.contains_key(&prepared.camera_id)
            || registry.opening.contains_key(&prepared.camera_id)
        {
            return Err(LiveError::AlreadyOpen);
        }
        if registry.sessions.len() + registry.opening.len() >= MAX_SIMULTANEOUS_LIVE_VIEWS {
            return Err(LiveError::Capacity);
        }

        let token = Uuid::new_v4().to_string();
        let temp_dir = self.cache_root.join(format!("session-{token}"));
        std::fs::create_dir(&temp_dir).map_err(|_| LiveError::Internal)?;
        let media_path = temp_dir.join("live.mp4");
        registry
            .opening
            .insert(prepared.camera_id.clone(), token.clone());
        Ok(LiveOpenAdmission {
            session_id: token,
            camera_id: prepared.camera_id,
            source_json: prepared.source_json,
            temp_dir,
            media_path,
            factory: self.factory.clone(),
            registry: Arc::downgrade(&self.registry),
            cleanup_on_drop: true,
        })
    }

    pub fn cancel_failed_open(&self, failed: LiveOpenStartError) -> LiveError {
        if let Ok(mut registry) = self.registry.lock()
            && registry.opening.get(&failed.camera_id) == Some(&failed.session_id)
        {
            registry.opening.remove(&failed.camera_id);
        }
        failed.error
    }

    pub fn commit_open(&self, mut started: StartedLiveOpen) -> Result<LiveOpenDto, LiveError> {
        {
            let registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
            let reserved = registry.opening.get(&started.camera_id) == Some(&started.session_id);
            if !registry.accepting || !reserved {
                return Err(LiveError::LifecycleBlocked);
            }
        }

        self.http_runtime
            .lock()
            .map_err(|_| LiveError::Internal)?
            .sessions
            .insert(
                started.session_id.clone(),
                HttpSession {
                    media_path: started.media_path.clone(),
                    active: true,
                },
            );
        let session_id = started.session_id.clone();
        let camera_id = started.camera_id.clone();
        let temp_dir = started.temp_dir.clone();
        let runner = started.runner.take().ok_or(LiveError::Internal)?;
        started.cleanup_on_drop = false;
        let session = Arc::new(LiveSession {
            camera_id: camera_id.clone(),
            temp_dir,
            last_keepalive: Mutex::new(Instant::now()),
            runner: Mutex::new(runner),
        });
        let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
        let reserved = registry.opening.get(&camera_id) == Some(&session_id);
        if !registry.accepting || !reserved || registry.camera_sessions.contains_key(&camera_id) {
            if reserved {
                registry.opening.remove(&camera_id);
            }
            drop(registry);
            if let Ok(mut runtime) = self.http_runtime.lock() {
                runtime.sessions.remove(&session_id);
            }
            return Err(LiveError::LifecycleBlocked);
        }
        registry.opening.remove(&camera_id);
        registry
            .camera_sessions
            .insert(camera_id.clone(), session_id.clone());
        registry.sessions.insert(session_id.clone(), session);
        let port = self.server.lock().map_err(|_| LiveError::Internal)?.port;
        Ok(LiveOpenDto {
            session_id: session_id.clone(),
            camera_id: camera_id.as_str().to_owned(),
            url: format!("http://127.0.0.1:{port}/live/{session_id}"),
            state: LiveState::Starting,
        })
    }

    pub fn close(&self, session_id: &str) -> Result<(), LiveError> {
        self.expire_sessions();
        let session = {
            let mut registry = self.registry.lock().map_err(|_| LiveError::Internal)?;
            let session = registry
                .sessions
                .remove(session_id)
                .ok_or(LiveError::SessionExpired)?;
            registry.camera_sessions.remove(&session.camera_id);
            session
        };
        if let Ok(mut runtime) = self.http_runtime.lock() {
            runtime.sessions.remove(session_id);
        }
        drop(session);
        Ok(())
    }

    pub fn keep_alive(&self, session_id: &str) -> Result<(), LiveError> {
        self.expire_sessions();
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
        self.expire_sessions();
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
                            .and_then(|mut runner| runner.status())
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

    pub fn close_all(&self) {
        let sessions = if let Ok(mut registry) = self.registry.lock() {
            registry.camera_sessions.clear();
            registry.opening.clear();
            std::mem::take(&mut registry.sessions)
        } else {
            HashMap::new()
        };
        if let Ok(mut runtime) = self.http_runtime.lock() {
            runtime.sessions.clear();
        }
        drop(sessions);
    }

    pub fn stop_accepting(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.accepting = false;
            registry.opening.clear();
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

    fn expire_sessions(&self) {
        expire_live_sessions(&self.registry, &self.http_runtime, self.session_timeout);
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
                registry.camera_sessions.remove(&session.camera_id);
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
            runtime.sessions.remove(session_id);
        }
    }
    drop(removed);
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
        let interval = Duration::from_secs(1).min(session_timeout);
        let thread = std::thread::Builder::new()
            .name("live-session-reaper".to_owned())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
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
    let Some(token) = request.path.strip_prefix("/live/") else {
        return write_empty(stream, "404 Not Found");
    };
    if token.is_empty() || token.contains('/') || Uuid::parse_str(token).is_err() {
        return write_empty(stream, "404 Not Found");
    }
    let media_path = {
        let runtime = runtime
            .lock()
            .map_err(|_| std::io::Error::other("live state unavailable"))?;
        let Some(session) = runtime.sessions.get(token) else {
            return write_empty(stream, "410 Gone");
        };
        if !session.active {
            return write_empty(stream, "410 Gone");
        }
        session.media_path.clone()
    };

    let mut file = File::open(&media_path)?;
    let origin_header = request
        .header("origin")
        .map(|origin| format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"))
        .unwrap_or_default();
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nTransfer-Encoding: chunked\r\n{origin_header}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    if request.method == "HEAD" {
        stream.write_all(b"0\r\n\r\n")?;
        return Ok(());
    }

    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        let still_active = runtime
            .lock()
            .map(|runtime| {
                runtime
                    .sessions
                    .get(token)
                    .is_some_and(|session| session.active)
            })
            .unwrap_or(false);
        if !still_active {
            break;
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }
        stream.write_all(format!("{read:X}\r\n").as_bytes())?;
        stream.write_all(&buffer[..read])?;
        stream.write_all(b"\r\n")?;
    }
    let _ = stream.write_all(b"0\r\n\r\n");
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
    stopped: bool,
}

impl WorkerLiveRunner {
    fn start(
        program: &str,
        source_json: serde_json::Value,
        output_path: &Path,
    ) -> Result<Self, LiveError> {
        let child = Command::new(program)
            .arg("run")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| LiveError::WorkerUnavailable)?;
        let child = crate::worker_process::contain_spawned_worker(child)
            .map_err(|_| LiveError::WorkerUnavailable)?;
        let mut runner = Self::from_child(child)?;
        runner.wait_hello()?;
        let result = runner.request(
            "live.start",
            serde_json::json!({
                "source": source_json,
                "output_path": output_path.to_string_lossy(),
            }),
        )?;
        if result.get("started").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(LiveError::WorkerUnavailable);
        }
        Ok(runner)
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
            stopped: false,
        })
    }

    fn wait_hello(&mut self) -> Result<(), LiveError> {
        let deadline = Instant::now() + WORKER_HELLO_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(WorkerMessage::Frame(Envelope::Event { v, name, data }))
                    if v == PROTOCOL_VERSION
                        && name == event::HELLO
                        && nian_ipc::validate_worker_hello(&data).is_ok() =>
                {
                    return Ok(());
                }
                Ok(WorkerMessage::Frame(_)) => {}
                Ok(WorkerMessage::Eof | WorkerMessage::Error) | Err(_) => {
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
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
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
                Ok(WorkerMessage::Frame(_)) => {}
                Ok(WorkerMessage::Eof | WorkerMessage::Error) | Err(_) => {
                    return Err(LiveError::WorkerUnavailable);
                }
            }
        }
    }

    fn cleanup(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.request("live.stop", serde_json::json!({}));
        self.stopped = true;
        if let Some(stdin) = self.stdin.as_mut() {
            let id = self.next_request_id;
            let _ = FramedWriter::new(stdin).send(&Envelope::Request {
                v: PROTOCOL_VERSION,
                id,
                method: method::SHUTDOWN.to_owned(),
                params: serde_json::json!({}),
            });
        }
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
    }
}

impl LiveRunner for WorkerLiveRunner {
    fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
        let value = self.request("live.status", serde_json::json!({}))?;
        serde_json::from_value(value).map_err(|_| LiveError::WorkerUnavailable)
    }

    fn stop(&mut self) {
        self.cleanup();
    }
}

impl Drop for WorkerLiveRunner {
    fn drop(&mut self) {
        self.cleanup();
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
        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            self.status.clone()
        }

        fn stop(&mut self) {}
    }

    impl LiveRunnerFactory for FakeFactory {
        fn start(
            &self,
            _source_json: serde_json::Value,
            output_path: &Path,
        ) -> Result<Box<dyn LiveRunner>, LiveError> {
            std::fs::write(output_path, b"fake-live").map_err(|_| LiveError::Internal)?;
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
    }

    impl LiveRunner for StopCountingRunner {
        fn status(&mut self) -> Result<LiveWorkerStatus, LiveError> {
            Ok(LiveWorkerStatus {
                state: LiveState::Live,
                failure_category: None,
                reconnect_attempt: 0,
            })
        }

        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl LiveRunnerFactory for StopCountingFactory {
        fn start(
            &self,
            _source_json: serde_json::Value,
            output_path: &Path,
        ) -> Result<Box<dyn LiveRunner>, LiveError> {
            std::fs::write(output_path, b"fake-live").map_err(|_| LiveError::Internal)?;
            Ok(Box::new(StopCountingRunner {
                stops: self.stops.clone(),
            }))
        }
    }

    struct IsolatingFactory {
        starts: AtomicUsize,
    }

    impl LiveRunnerFactory for IsolatingFactory {
        fn start(
            &self,
            _source_json: serde_json::Value,
            output_path: &Path,
        ) -> Result<Box<dyn LiveRunner>, LiveError> {
            std::fs::write(output_path, b"fake-live").map_err(|_| LiveError::Internal)?;
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
