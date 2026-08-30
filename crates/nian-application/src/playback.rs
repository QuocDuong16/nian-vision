//! M6 recording timeline and local playback orchestration.
//!
//! SQLite is a query cache only. Every playback open and every HTTP media
//! request revalidates the filesystem-derived recording identity before bytes
//! are exposed. React receives only opaque recording/session identities.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use nian_domain::{CameraId, RetentionPolicy, StorageQuota};
use nian_index::{IndexedRecording, RecordingKind};
use nian_ipc::message::{Envelope, PROTOCOL_VERSION, event, method};
use nian_ipc::{FramedReader, FramedWriter};
use nian_storage::RecordingsLayout;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage_manager::{
    PlaybackPin, PlaybackPins, RecordingLookupError, StorageManager, ValidatedRecording,
    validate_indexed_playback_path,
};

const MAX_PLAYBACK_SESSIONS: usize = 4;
const MAX_HTTP_REQUESTS: usize = 8;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const WORKER_PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const CACHE_INSTANCE_PREFIX: &str = "instance-";
const CACHE_INSTANCE_LOCK: &str = ".nian-playback-instance.lock";
const CACHE_COORDINATION_LOCK: &str = ".nian-playback-cache.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackErrorCode {
    RecordingNotFound,
    RecordingMissing,
    RecordingStale,
    RecordingNotFinalized,
    UnsupportedCodec,
    UnsupportedContainer,
    MediaUnreadable,
    PlaybackSessionExpired,
    PlaybackBusy,
    WorkerUnavailable,
    Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("recording was not found")]
    RecordingNotFound,
    #[error("recording file is missing")]
    RecordingMissing,
    #[error("recording file no longer matches its indexed identity")]
    RecordingStale,
    #[error("recording is not finalized")]
    RecordingNotFinalized,
    #[error("recording video codec is not supported without transcoding")]
    UnsupportedCodec,
    #[error("recording container cannot be prepared for browser playback")]
    UnsupportedContainer,
    #[error("recording media is unreadable")]
    MediaUnreadable,
    #[error("playback session has expired")]
    PlaybackSessionExpired,
    #[error("too many playback sessions are active")]
    PlaybackBusy,
    #[error("media worker is unavailable")]
    WorkerUnavailable,
    #[error("playback service is unavailable")]
    Internal,
}

impl PlaybackError {
    pub const fn code(&self) -> PlaybackErrorCode {
        match self {
            Self::RecordingNotFound => PlaybackErrorCode::RecordingNotFound,
            Self::RecordingMissing => PlaybackErrorCode::RecordingMissing,
            Self::RecordingStale => PlaybackErrorCode::RecordingStale,
            Self::RecordingNotFinalized => PlaybackErrorCode::RecordingNotFinalized,
            Self::UnsupportedCodec => PlaybackErrorCode::UnsupportedCodec,
            Self::UnsupportedContainer => PlaybackErrorCode::UnsupportedContainer,
            Self::MediaUnreadable => PlaybackErrorCode::MediaUnreadable,
            Self::PlaybackSessionExpired => PlaybackErrorCode::PlaybackSessionExpired,
            Self::PlaybackBusy => PlaybackErrorCode::PlaybackBusy,
            Self::WorkerUnavailable => PlaybackErrorCode::WorkerUnavailable,
            Self::Internal => PlaybackErrorCode::Internal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineRecordingKind {
    Normal,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingDto {
    pub recording_id: String,
    pub camera_id: String,
    pub kind: TimelineRecordingKind,
    pub started_at: String,
    pub sequence: u32,
    pub size_bytes: u64,
    pub media_duration_ms: Option<u64>,
    pub end_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdjacentRecordingsDto {
    pub previous: Option<RecordingDto>,
    pub next: Option<RecordingDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackInspectDto {
    pub duration_ms: Option<u64>,
    pub video_codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub audio_available: bool,
    pub container_compatibility: String,
    pub seekable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaybackOpenDto {
    pub session_id: String,
    pub url: String,
    pub recording: RecordingDto,
    pub inspect: PlaybackInspectDto,
    pub adjacent: AdjacentRecordingsDto,
}

pub trait PlaybackBackend: Send + Sync {
    fn prepare(
        &self,
        source_path: &Path,
        output_path: &Path,
    ) -> Result<PlaybackInspectDto, PlaybackError>;
}

#[derive(Debug, Clone)]
pub struct WorkerPlaybackBackend {
    pub worker_program: String,
}

impl PlaybackBackend for WorkerPlaybackBackend {
    fn prepare(
        &self,
        source_path: &Path,
        output_path: &Path,
    ) -> Result<PlaybackInspectDto, PlaybackError> {
        run_worker_prepare(&self.worker_program, source_path, output_path)
    }
}

#[derive(Debug)]
struct PlaybackCacheInstance {
    path: PathBuf,
    lock_file: Arc<File>,
}

impl PlaybackCacheInstance {
    fn create(cache_root: &Path) -> Result<Self, PlaybackError> {
        std::fs::create_dir_all(cache_root).map_err(|_| PlaybackError::Internal)?;
        let coordination = open_cache_lock(cache_root)?;
        coordination.lock().map_err(|_| PlaybackError::Internal)?;
        cleanup_stale_cache_locked(cache_root);

        for _ in 0..8 {
            let path = cache_root.join(format!("{CACHE_INSTANCE_PREFIX}{}", Uuid::new_v4()));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let lock_path = path.join(CACHE_INSTANCE_LOCK);
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&lock_path)
                        .map_err(|_| PlaybackError::Internal)?;
                    file.lock().map_err(|_| PlaybackError::Internal)?;
                    drop(coordination);
                    return Ok(Self {
                        path,
                        lock_file: Arc::new(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(PlaybackError::Internal),
            }
        }
        Err(PlaybackError::Internal)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn lease(&self) -> Arc<File> {
        self.lock_file.clone()
    }
}

fn open_cache_lock(cache_root: &Path) -> Result<File, PlaybackError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache_root.join(CACHE_COORDINATION_LOCK))
        .map_err(|_| PlaybackError::Internal)
}

#[cfg(test)]
fn cleanup_stale_cache(cache_root: &Path) -> Result<(), PlaybackError> {
    std::fs::create_dir_all(cache_root).map_err(|_| PlaybackError::Internal)?;
    let coordination = open_cache_lock(cache_root)?;
    coordination.lock().map_err(|_| PlaybackError::Internal)?;
    cleanup_stale_cache_locked(cache_root);
    Ok(())
}

fn cleanup_stale_cache_locked(cache_root: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(CACHE_INSTANCE_PREFIX) {
            continue;
        }
        let instance = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&instance) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let lock_path = instance.join(CACHE_INSTANCE_LOCK);
        let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_dir_all(&instance);
                continue;
            }
            Err(_) => continue,
        };
        let Ok(lock_metadata) = std::fs::symlink_metadata(&lock_path) else {
            continue;
        };
        if !lock_metadata.is_file() || lock_metadata.file_type().is_symlink() {
            continue;
        }
        match file.try_lock() {
            Ok(()) => {
                drop(file);
                let _ = std::fs::remove_dir_all(&instance);
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(_)) => {}
        }
    }
}

struct PlaybackCacheDir {
    path: PathBuf,
    cleanup_on_drop: bool,
}

impl PlaybackCacheDir {
    fn create(path: PathBuf) -> Result<Self, PlaybackError> {
        std::fs::create_dir(&path).map_err(|_| PlaybackError::Internal)?;
        Ok(Self {
            path,
            cleanup_on_drop: true,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn into_session_path(mut self) -> PathBuf {
        self.cleanup_on_drop = false;
        self.path.clone()
    }
}

impl Drop for PlaybackCacheDir {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

struct PlaybackSession {
    layout: RecordingsLayout,
    indexed: IndexedRecording,
    _cache_instance_lease: Arc<File>,
    source_path: PathBuf,
    _source_handle: File,
    _pin: PlaybackPin,
    media_path: PathBuf,
    temp_dir: PathBuf,
    last_activity: Instant,
    active_requests: usize,
    close_requested: bool,
}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[derive(Default)]
struct PlaybackRuntime {
    sessions: HashMap<String, PlaybackSession>,
}

pub struct PreparedPlaybackStorage {
    storage: Option<StorageManager>,
}

pub struct PlaybackController {
    storage: Option<StorageManager>,
    pins: PlaybackPins,
    backend: Arc<dyn PlaybackBackend>,
    runtime: Arc<Mutex<PlaybackRuntime>>,
    server: PlaybackHttpServer,
    cache_instance: PlaybackCacheInstance,
}

impl std::fmt::Debug for PlaybackController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackController")
            .field("configured", &self.storage.is_some())
            .field("port", &self.server.port)
            .finish_non_exhaustive()
    }
}

impl PlaybackController {
    pub fn new(worker_program: String, cache_root: PathBuf) -> Result<Self, PlaybackError> {
        Self::with_backend(
            Arc::new(WorkerPlaybackBackend { worker_program }),
            cache_root,
        )
    }

    pub fn with_backend(
        backend: Arc<dyn PlaybackBackend>,
        cache_root: PathBuf,
    ) -> Result<Self, PlaybackError> {
        let cache_instance = PlaybackCacheInstance::create(&cache_root)?;
        let runtime = Arc::new(Mutex::new(PlaybackRuntime::default()));
        let server = PlaybackHttpServer::start(runtime.clone())?;
        Ok(Self {
            storage: None,
            pins: PlaybackPins::default(),
            backend,
            runtime,
            server,
            cache_instance,
        })
    }

    pub fn configure_storage(
        &mut self,
        storage_root: Option<PathBuf>,
        retention_policy: RetentionPolicy,
        storage_quota: Option<StorageQuota>,
    ) -> Result<(), PlaybackError> {
        let prepared = self.prepare_storage(storage_root, retention_policy, storage_quota)?;
        self.commit_prepared_storage(prepared);
        Ok(())
    }

    pub fn prepare_storage(
        &self,
        storage_root: Option<PathBuf>,
        retention_policy: RetentionPolicy,
        storage_quota: Option<StorageQuota>,
    ) -> Result<PreparedPlaybackStorage, PlaybackError> {
        let Some(storage_root) = storage_root else {
            return Ok(PreparedPlaybackStorage { storage: None });
        };
        let layout = RecordingsLayout::new(storage_root).map_err(|_| PlaybackError::Internal)?;
        let mut storage = StorageManager::open_with_playback_pins(
            layout,
            retention_policy,
            storage_quota,
            self.pins.clone(),
        )
        .map_err(|_| PlaybackError::Internal)?;
        storage.reconcile().map_err(|_| PlaybackError::Internal)?;
        Ok(PreparedPlaybackStorage {
            storage: Some(storage),
        })
    }

    pub fn commit_prepared_storage(&mut self, prepared: PreparedPlaybackStorage) {
        self.close_all();
        self.storage = prepared.storage;
    }

    pub fn refresh_index(&mut self) -> Result<(), PlaybackError> {
        let storage = self.storage.as_mut().ok_or(PlaybackError::Internal)?;
        storage.reconcile().map_err(|_| PlaybackError::Internal)?;
        Ok(())
    }

    pub fn configured_storage_root(&self) -> Option<&Path> {
        self.storage.as_ref().map(|storage| storage.layout().root())
    }

    pub fn recording_days(&self, camera_id: &CameraId) -> Result<Vec<String>, PlaybackError> {
        let storage = self.storage.as_ref().ok_or(PlaybackError::Internal)?;
        storage
            .available_recording_days(camera_id)
            .map(|days| {
                days.into_iter()
                    .map(|day| day.format("%Y-%m-%d").to_string())
                    .collect()
            })
            .map_err(|_| PlaybackError::Internal)
    }

    pub fn timeline(
        &self,
        camera_id: &CameraId,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<RecordingDto>, PlaybackError> {
        if end <= start {
            return Err(PlaybackError::Internal);
        }
        self.storage
            .as_ref()
            .ok_or(PlaybackError::Internal)?
            .query_time_range(camera_id, start, end)
            .map(|rows| rows.iter().map(recording_dto).collect())
            .map_err(|_| PlaybackError::Internal)
    }

    pub fn adjacent(&self, recording_id: &str) -> Result<AdjacentRecordingsDto, PlaybackError> {
        let storage = self.storage.as_ref().ok_or(PlaybackError::Internal)?;
        let recording = storage
            .recording_by_id(recording_id)
            .map_err(|_| PlaybackError::Internal)?
            .ok_or(PlaybackError::RecordingNotFound)?;
        let previous = storage
            .previous_recording(&recording)
            .map_err(|_| PlaybackError::Internal)?;
        let next = storage
            .next_recording(&recording)
            .map_err(|_| PlaybackError::Internal)?;
        Ok(AdjacentRecordingsDto {
            previous: previous.as_ref().map(recording_dto),
            next: next.as_ref().map(recording_dto),
        })
    }

    pub fn open(&mut self, recording_id: &str) -> Result<PlaybackOpenDto, PlaybackError> {
        self.expire_sessions();
        if self.active_session_count() >= MAX_PLAYBACK_SESSIONS {
            return Err(PlaybackError::PlaybackBusy);
        }

        let storage = self.storage.as_mut().ok_or(PlaybackError::Internal)?;
        let validated = match storage.validate_recording_for_playback(recording_id) {
            Ok(validated) => validated,
            Err(error) => {
                let mapped = map_lookup_error(&error);
                if matches!(
                    error,
                    RecordingLookupError::Missing | RecordingLookupError::Stale(_)
                ) {
                    let _ = storage.reconcile();
                }
                return Err(mapped);
            }
        };

        // Pin starts only after successful filesystem validation.
        let pin = self.pins.pin(validated.indexed.relative_path.clone());
        let source_handle = File::open(&validated.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PlaybackError::RecordingMissing
            } else {
                PlaybackError::RecordingStale
            }
        })?;
        revalidate(&validated)?;

        let token = Uuid::new_v4().to_string();
        let temp_dir =
            PlaybackCacheDir::create(self.cache_instance.path().join(format!("session-{token}")))?;
        let media_path = temp_dir.path().join("media.mp4");

        let inspect = self.backend.prepare(&validated.path, &media_path)?;
        if let Err(error) = revalidate(&validated) {
            let _ = storage.reconcile();
            return Err(error);
        }
        let media_metadata =
            std::fs::symlink_metadata(&media_path).map_err(|_| PlaybackError::MediaUnreadable)?;
        if !media_metadata.is_file() || media_metadata.file_type().is_symlink() {
            return Err(PlaybackError::MediaUnreadable);
        }

        let mut indexed = validated.indexed.clone();
        if let Some(duration_ms) = inspect.duration_ms {
            // Revalidation above proves this is still the inspected object.
            if storage
                .write_media_duration(&indexed, duration_ms)
                .unwrap_or(false)
            {
                indexed.media_duration_ms = Some(duration_ms);
            } else if indexed.media_duration_ms.is_none() {
                // Playback remains valid even when the disposable index cannot
                // accept enrichment; use trusted inspection data in this DTO.
                indexed.media_duration_ms = Some(duration_ms);
            }
        }

        let previous = storage
            .previous_recording(&indexed)
            .map_err(|_| PlaybackError::Internal)?;
        let next = storage
            .next_recording(&indexed)
            .map_err(|_| PlaybackError::Internal)?;
        let adjacent = AdjacentRecordingsDto {
            previous: previous.as_ref().map(recording_dto),
            next: next.as_ref().map(recording_dto),
        };
        let layout = storage.layout().clone();
        let session = PlaybackSession {
            layout,
            indexed: indexed.clone(),
            _cache_instance_lease: self.cache_instance.lease(),
            source_path: validated.path,
            _source_handle: source_handle,
            _pin: pin,
            media_path,
            temp_dir: temp_dir.into_session_path(),
            last_activity: Instant::now(),
            active_requests: 0,
            close_requested: false,
        };
        self.runtime
            .lock()
            .map_err(|_| PlaybackError::Internal)?
            .sessions
            .insert(token.clone(), session);

        Ok(PlaybackOpenDto {
            session_id: token.clone(),
            url: format!("http://127.0.0.1:{}/playback/{token}", self.server.port),
            recording: recording_dto(&indexed),
            inspect,
            adjacent,
        })
    }

    pub fn close(&mut self, session_id: &str) -> Result<(), PlaybackError> {
        self.expire_sessions();
        let mut runtime = self.runtime.lock().map_err(|_| PlaybackError::Internal)?;
        let Some(session) = runtime.sessions.get_mut(session_id) else {
            return Err(PlaybackError::PlaybackSessionExpired);
        };
        if session.active_requests == 0 {
            runtime.sessions.remove(session_id);
        } else {
            session.close_requested = true;
        }
        Ok(())
    }

    pub fn session_active(&mut self, session_id: &str) -> Result<bool, PlaybackError> {
        self.expire_sessions();
        Ok(self
            .runtime
            .lock()
            .map_err(|_| PlaybackError::Internal)?
            .sessions
            .get(session_id)
            .is_some_and(|session| !session.close_requested))
    }

    fn active_session_count(&self) -> usize {
        self.runtime
            .lock()
            .map(|runtime| runtime.sessions.len())
            .unwrap_or(MAX_PLAYBACK_SESSIONS)
    }

    fn expire_sessions(&mut self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            expire_sessions_locked(&mut runtime);
        }
    }

    fn close_all(&mut self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.sessions.retain(|_, session| {
                if session.active_requests == 0 {
                    false
                } else {
                    session.close_requested = true;
                    true
                }
            });
        }
    }
}

impl Drop for PlaybackController {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn recording_dto(recording: &IndexedRecording) -> RecordingDto {
    let end_at = recording.media_duration_ms.and_then(|duration_ms| {
        let millis = i64::try_from(duration_ms).ok()?;
        recording
            .started_at
            .checked_add_signed(chrono::Duration::milliseconds(millis))
            .map(format_wall_time)
    });
    RecordingDto {
        recording_id: recording.relative_path.clone(),
        camera_id: recording.camera_id.as_str().to_owned(),
        kind: match recording.kind {
            RecordingKind::Normal => TimelineRecordingKind::Normal,
            RecordingKind::Recovered => TimelineRecordingKind::Recovered,
        },
        started_at: format_wall_time(recording.started_at),
        sequence: recording.sequence,
        size_bytes: recording.size_bytes,
        media_duration_ms: recording.media_duration_ms,
        end_at,
    }
}

fn format_wall_time(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()
}

fn map_lookup_error(error: &RecordingLookupError) -> PlaybackError {
    match error {
        RecordingLookupError::NotFound => PlaybackError::RecordingNotFound,
        RecordingLookupError::Missing => PlaybackError::RecordingMissing,
        RecordingLookupError::Stale(_) => PlaybackError::RecordingStale,
        RecordingLookupError::NotFinalized => PlaybackError::RecordingNotFinalized,
        RecordingLookupError::Index(_) => PlaybackError::Internal,
    }
}

fn revalidate(validated: &ValidatedRecording) -> Result<(), PlaybackError> {
    let layout = validated
        .path
        .ancestors()
        .nth(5)
        .and_then(|root| RecordingsLayout::new(root.to_path_buf()).ok())
        .ok_or(PlaybackError::RecordingStale)?;
    let path = validate_indexed_playback_path(&layout, &validated.indexed)
        .map_err(|error| map_lookup_error(&error))?;
    if path == validated.path {
        Ok(())
    } else {
        Err(PlaybackError::RecordingStale)
    }
}

struct PlaybackHttpServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PlaybackHttpServer {
    fn start(runtime: Arc<Mutex<PlaybackRuntime>>) -> Result<Self, PlaybackError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| PlaybackError::Internal)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| PlaybackError::Internal)?;
        let port = listener
            .local_addr()
            .map_err(|_| PlaybackError::Internal)?
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::Builder::new()
            .name("playback-loopback-http".to_owned())
            .spawn(move || {
                while !stop_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if active.load(Ordering::Acquire) >= MAX_HTTP_REQUESTS {
                                let _ = reject_busy(stream);
                                continue;
                            }
                            active.fetch_add(1, Ordering::AcqRel);
                            let runtime = runtime.clone();
                            let active_for_thread = active.clone();
                            let spawned = std::thread::Builder::new()
                                .name("playback-http-request".to_owned())
                                .spawn(move || {
                                    let _guard = ActiveRequestGuard(active_for_thread);
                                    let _ = serve_request(stream, port, &runtime);
                                });
                            if spawned.is_err() {
                                active.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if let Ok(mut runtime) = runtime.lock() {
                                expire_sessions_locked(&mut runtime);
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            })
            .map_err(|_| PlaybackError::Internal)?;
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for PlaybackHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ActiveRequestGuard(Arc<AtomicUsize>);

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn expire_sessions_locked(runtime: &mut PlaybackRuntime) {
    let now = Instant::now();
    runtime.sessions.retain(|_, session| {
        if session.active_requests > 0 {
            return true;
        }
        !session.close_requested && now.duration_since(session.last_activity) < SESSION_IDLE_TIMEOUT
    });
}

struct SessionRequestGuard {
    runtime: Arc<Mutex<PlaybackRuntime>>,
    token: String,
}

impl SessionRequestGuard {
    fn new(runtime: Arc<Mutex<PlaybackRuntime>>, token: String) -> Self {
        Self { runtime, token }
    }
}

impl Drop for SessionRequestGuard {
    fn drop(&mut self) {
        let Ok(mut runtime) = self.runtime.lock() else {
            return;
        };
        let should_remove = if let Some(session) = runtime.sessions.get_mut(&self.token) {
            session.active_requests = session.active_requests.saturating_sub(1);
            session.last_activity = Instant::now();
            session.active_requests == 0 && session.close_requested
        } else {
            false
        };
        if should_remove {
            runtime.sessions.remove(&self.token);
        }
    }
}

fn reject_busy(mut stream: TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
}

fn serve_request(
    mut stream: TcpStream,
    port: u16,
    runtime: &Arc<Mutex<PlaybackRuntime>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let request = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(status) => return write_empty(&mut stream, status),
    };
    if request.method != "GET" && request.method != "HEAD" {
        return write_empty(&mut stream, "405 Method Not Allowed");
    }
    if !host_allowed(request.header("host"), port) || !origin_allowed(request.header("origin")) {
        return write_empty(&mut stream, "403 Forbidden");
    }
    let Some(token) = request.path.strip_prefix("/playback/") else {
        return write_empty(&mut stream, "404 Not Found");
    };
    if token.is_empty() || token.contains('/') || Uuid::parse_str(token).is_err() {
        return write_empty(&mut stream, "404 Not Found");
    }

    let (mut file, len) = {
        let mut runtime = runtime
            .lock()
            .map_err(|_| std::io::Error::other("playback state unavailable"))?;
        expire_sessions_locked(&mut runtime);
        let Some(session) = runtime.sessions.get_mut(token) else {
            return write_empty(&mut stream, "410 Gone");
        };
        if session.close_requested {
            return write_empty(&mut stream, "410 Gone");
        }
        let stale = match validate_indexed_playback_path(&session.layout, &session.indexed) {
            Ok(path) => path != session.source_path,
            Err(_) => true,
        };
        if stale {
            let remove_now = session.active_requests == 0;
            if !remove_now {
                session.close_requested = true;
            }
            if remove_now {
                runtime.sessions.remove(token);
            }
            return write_empty(&mut stream, "410 Gone");
        }
        session.last_activity = Instant::now();
        let file = File::open(&session.media_path)?;
        let len = file.metadata()?.len();
        session.active_requests += 1;
        (file, len)
    };
    let _session_request = SessionRequestGuard::new(runtime.clone(), token.to_owned());

    let range = match request.header("range") {
        Some(raw) => match parse_range(raw, len) {
            Some(range) => Some(range),
            None => {
                stream.write_all(
                    format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )?;
                return Ok(());
            }
        },
        None => None,
    };
    let (start, end, status) = match range {
        Some((start, end)) => (start, end, "206 Partial Content"),
        None if len > 0 => (0, len - 1, "200 OK"),
        None => (0, 0, "200 OK"),
    };
    let content_len = if len == 0 { 0 } else { end - start + 1 };
    let origin_header = request
        .header("origin")
        .map(|origin| format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"))
        .unwrap_or_default();
    let content_range = if range.is_some() {
        format!("Content-Range: bytes {start}-{end}/{len}\r\n")
    } else {
        String::new()
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Length: {content_len}\r\n{content_range}{origin_header}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(headers.as_bytes())?;
    if request.method == "HEAD" || content_len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = content_len;
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    while remaining > 0 {
        let wanted =
            usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64)).unwrap_or(STREAM_CHUNK_BYTES);
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
        remaining = remaining.saturating_sub(read as u64);
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

fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let raw = value.strip_prefix("bytes=")?;
    if raw.contains(',') {
        return None;
    }
    let (start, end) = raw.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        let suffix = suffix.min(len);
        return Some((len - suffix, len - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= len {
        return None;
    }
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<u64>().ok()?.min(len - 1)
    };
    (end >= start).then_some((start, end))
}

fn write_empty(stream: &mut TcpStream, status: &str) -> std::io::Result<()> {
    stream.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

enum WorkerMessage {
    Frame(Envelope),
    Eof,
    Error,
}

struct WorkerGuard {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl WorkerGuard {
    fn cleanup(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = FramedWriter::new(stdin).send(&Envelope::request(2, method::SHUTDOWN));
        }
        self.stdin.take();
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
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn run_worker_prepare(
    program: &str,
    source_path: &Path,
    output_path: &Path,
) -> Result<PlaybackInspectDto, PlaybackError> {
    let child = Command::new(program)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| PlaybackError::WorkerUnavailable)?;
    let mut worker = WorkerGuard {
        child,
        stdin: None,
        reader: None,
    };
    let stdin = worker
        .child
        .stdin
        .take()
        .ok_or(PlaybackError::WorkerUnavailable)?;
    worker.stdin = Some(stdin);
    let stdout = worker
        .child
        .stdout
        .take()
        .ok_or(PlaybackError::WorkerUnavailable)?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("playback-worker-stdout".to_owned())
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
        .map_err(|_| PlaybackError::WorkerUnavailable)?;
    worker.reader = Some(reader);

    let hello_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = hello_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(WorkerMessage::Frame(Envelope::Event { v, name, .. }))
                if v == PROTOCOL_VERSION && name == event::HELLO =>
            {
                break;
            }
            Ok(WorkerMessage::Frame(_)) => {}
            Ok(WorkerMessage::Eof | WorkerMessage::Error) | Err(_) => {
                return Err(PlaybackError::WorkerUnavailable);
            }
        }
    }

    let request = Envelope::Request {
        v: PROTOCOL_VERSION,
        id: 1,
        method: "playback.prepare".to_owned(),
        params: serde_json::json!({
            "source_path": source_path.to_string_lossy(),
            "output_path": output_path.to_string_lossy(),
            "timeout_ms": WORKER_PREPARE_TIMEOUT.as_millis() as u64,
        }),
    };
    FramedWriter::new(
        worker
            .stdin
            .as_mut()
            .ok_or(PlaybackError::WorkerUnavailable)?,
    )
    .send(&request)
    .map_err(|_| PlaybackError::WorkerUnavailable)?;

    let deadline = Instant::now() + WORKER_PREPARE_TIMEOUT + Duration::from_secs(2);
    let result = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(WorkerMessage::Frame(Envelope::Response {
                v,
                id: 1,
                ok: true,
                result,
                ..
            })) if v == PROTOCOL_VERSION => {
                break serde_json::from_value(result).map_err(|_| PlaybackError::Internal);
            }
            Ok(WorkerMessage::Frame(Envelope::Response {
                v,
                id: 1,
                ok: false,
                error_code,
                ..
            })) if v == PROTOCOL_VERSION => {
                break Err(map_worker_error(
                    error_code.as_deref().unwrap_or("internal"),
                ));
            }
            Ok(WorkerMessage::Frame(_)) => {}
            Ok(WorkerMessage::Eof | WorkerMessage::Error) => {
                break Err(PlaybackError::WorkerUnavailable);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break Err(PlaybackError::WorkerUnavailable),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(PlaybackError::WorkerUnavailable);
            }
        }
    };
    worker.cleanup();
    result
}

fn map_worker_error(code: &str) -> PlaybackError {
    match code.split(':').next().unwrap_or(code) {
        "unsupported_codec" => PlaybackError::UnsupportedCodec,
        "unsupported_container" => PlaybackError::UnsupportedContainer,
        "media_unreadable" => PlaybackError::MediaUnreadable,
        "worker_unavailable" => PlaybackError::WorkerUnavailable,
        _ => PlaybackError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeBackend {
        bytes: Vec<u8>,
        duration_ms: Option<u64>,
    }

    impl PlaybackBackend for FakeBackend {
        fn prepare(
            &self,
            source_path: &Path,
            output_path: &Path,
        ) -> Result<PlaybackInspectDto, PlaybackError> {
            let metadata = std::fs::symlink_metadata(source_path)
                .map_err(|_| PlaybackError::MediaUnreadable)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(PlaybackError::MediaUnreadable);
            }
            std::fs::write(output_path, &self.bytes).map_err(|_| PlaybackError::Internal)?;
            Ok(PlaybackInspectDto {
                duration_ms: self.duration_ms,
                video_codec: "h264".to_owned(),
                width: Some(1920),
                height: Some(1080),
                audio_available: false,
                container_compatibility: "fragmented_mp4".to_owned(),
                seekable: true,
            })
        }
    }

    #[derive(Debug)]
    struct FailingBackend;

    impl PlaybackBackend for FailingBackend {
        fn prepare(
            &self,
            _source_path: &Path,
            output_path: &Path,
        ) -> Result<PlaybackInspectDto, PlaybackError> {
            std::fs::write(output_path, b"partial-cache").map_err(|_| PlaybackError::Internal)?;
            Err(PlaybackError::MediaUnreadable)
        }
    }

    const CACHE_CHILD_ENV: &str = "NIAN_PLAYBACK_CACHE_TEST_CHILD";
    const CACHE_ROOT_ENV: &str = "NIAN_PLAYBACK_CACHE_TEST_ROOT";
    const RECORDING_ROOT_ENV: &str = "NIAN_PLAYBACK_CACHE_TEST_RECORDINGS";

    #[test]
    #[ignore = "spawned explicitly by live_cache_instance_is_not_cleaned_by_another_process"]
    #[allow(clippy::print_stdout)]
    fn cache_instance_child() {
        use std::io::Write as _;

        if std::env::var_os(CACHE_CHILD_ENV).is_none() {
            return;
        }
        let cache_root = PathBuf::from(std::env::var_os(CACHE_ROOT_ENV).unwrap());
        let recording_root = PathBuf::from(std::env::var_os(RECORDING_ROOT_ENV).unwrap());
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let backend: Arc<dyn PlaybackBackend> = Arc::new(FakeBackend {
            bytes: b"child-prepared-media".to_vec(),
            duration_ms: Some(5_000),
        });
        let mut controller = PlaybackController::with_backend(backend, cache_root).unwrap();
        controller
            .configure_storage(Some(recording_root), RetentionPolicy::default(), None)
            .unwrap();
        let opened = controller.open(relative).unwrap();
        let media = controller
            .cache_instance
            .path()
            .join(format!("session-{}/media.mp4", opened.session_id));
        assert!(media.exists());
        println!("CACHE_READY");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    fn controller_with_files(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PlaybackController) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        for (relative, bytes) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let backend: Arc<dyn PlaybackBackend> = Arc::new(FakeBackend {
            bytes: b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec(),
            duration_ms: Some(5_000),
        });
        let mut controller =
            PlaybackController::with_backend(backend, temp.path().join("playback-cache")).unwrap();
        controller
            .configure_storage(Some(root), RetentionPolicy::default(), None)
            .unwrap();
        (temp, controller)
    }

    fn http(port: u16, request: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    }

    fn response_parts(response: &[u8]) -> (&str, &[u8]) {
        let boundary = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&response[..boundary]).unwrap();
        (headers, &response[boundary + 4..])
    }

    #[test]
    fn range_parser_supports_full_middle_open_and_suffix_ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=20-10", 100), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 100), None);
    }

    #[test]
    fn only_loopback_hosts_and_expected_desktop_origins_are_allowed() {
        assert!(host_allowed(Some("127.0.0.1:4321"), 4321));
        assert!(host_allowed(Some("localhost:4321"), 4321));
        assert!(!host_allowed(Some("192.168.1.5:4321"), 4321));
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("http://tauri.localhost")));
        assert!(origin_allowed(Some("http://localhost:1420")));
        assert!(!origin_allowed(Some("https://example.com")));
    }
    #[test]
    fn failed_prepare_cleans_session_cache_and_pin() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let source = root.join(relative);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"normal").unwrap();
        let cache_root = temp.path().join("playback-cache");
        let backend: Arc<dyn PlaybackBackend> = Arc::new(FailingBackend);
        let mut controller = PlaybackController::with_backend(backend, cache_root.clone()).unwrap();
        controller
            .configure_storage(Some(root), RetentionPolicy::default(), None)
            .unwrap();

        assert!(matches!(
            controller.open(relative),
            Err(PlaybackError::MediaUnreadable)
        ));
        assert!(!controller.pins.is_pinned(relative));
        assert!(
            std::fs::read_dir(controller.cache_instance.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with("session-"))
        );
    }

    #[test]
    fn refresh_index_discovers_new_finalized_normal_and_recovered_but_not_active_partial() {
        let first = "cam-a/2026/08/29/08-30-00.mkv";
        let second = "cam-a/2026/08/29/08-40-00.mkv";
        let recovered = "cam-a/2026/08/29/08-50-00.recovered.mkv";
        let partial = "cam-a/2026/08/29/09-00-00.partial.mkv";
        let (temp, mut controller) = controller_with_files(&[(first, b"a")]);
        let root = temp.path().join("recordings");
        let camera = CameraId::parse("cam-a").unwrap();
        let layout = RecordingsLayout::new(root.clone()).unwrap();
        let _lease = nian_storage::CameraLease::try_acquire(&layout, &camera).unwrap();
        for (relative, bytes) in [
            (second, b"b".as_slice()),
            (recovered, b"r".as_slice()),
            (partial, b"p".as_slice()),
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let start =
            NaiveDateTime::parse_from_str("2026-08-29T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let end =
            NaiveDateTime::parse_from_str("2026-08-30T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        assert_eq!(controller.timeline(&camera, start, end).unwrap().len(), 1);
        controller.refresh_index().unwrap();
        let rows = controller.timeline(&camera, start, end).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.recording_id.as_str())
                .collect::<Vec<_>>(),
            vec![first, second, recovered]
        );
        assert_eq!(rows[2].kind, TimelineRecordingKind::Recovered);
        assert!(rows.iter().all(|row| row.media_duration_ms.is_none()));
        assert!(!rows.iter().any(|row| row.recording_id == partial));
    }

    #[test]
    fn live_cache_instance_is_not_cleaned_by_another_process_but_stale_instance_is() {
        use std::io::BufRead as _;

        let temp = tempfile::tempdir().unwrap();
        let recording_root = temp.path().join("recordings");
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let source = recording_root.join(relative);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"recording").unwrap();
        let cache_root = temp.path().join("playback-cache");

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("cache_instance_child")
            .arg("--ignored")
            .arg("--nocapture")
            .env(CACHE_CHILD_ENV, "1")
            .env(CACHE_ROOT_ENV, &cache_root)
            .env(RECORDING_ROOT_ENV, &recording_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            let ready = lines.any(|line| line.is_ok_and(|line| line.contains("CACHE_READY")));
            let _ = ready_tx.send(ready);
        });
        assert!(ready_rx.recv_timeout(Duration::from_secs(5)).unwrap());

        let instance = std::fs::read_dir(&cache_root)
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(CACHE_INSTANCE_PREFIX)
            })
            .unwrap()
            .path();
        assert!(
            std::fs::read_dir(&instance)
                .unwrap()
                .flatten()
                .any(|entry| {
                    entry.file_name().to_string_lossy().starts_with("session-")
                        && entry.path().join("media.mp4").is_file()
                })
        );

        cleanup_stale_cache(&cache_root).unwrap();
        assert!(
            instance.exists(),
            "live process cache must remain untouched"
        );

        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        reader.join().unwrap();
        cleanup_stale_cache(&cache_root).unwrap();
        assert!(
            !instance.exists(),
            "abandoned cache must be cleanable after process death"
        );
    }

    #[test]
    fn cache_instance_lock_outlives_controller_owner_while_a_session_lease_exists() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("playback-cache");
        let instance = PlaybackCacheInstance::create(&cache_root).unwrap();
        let instance_path = instance.path().to_path_buf();
        let session_lease = instance.lease();

        drop(instance);
        cleanup_stale_cache(&cache_root).unwrap();
        assert!(instance_path.exists());

        drop(session_lease);
        cleanup_stale_cache(&cache_root).unwrap();
        assert!(!instance_path.exists());
    }

    #[test]
    fn timeline_recovered_duration_enrichment_and_rebuild_keep_stable_identity() {
        let normal = "cam-a/2026/08/29/08-30-00.mkv";
        let recovered = "cam-a/2026/08/29/08-30-00-2.recovered.mkv";
        let (_temp, mut controller) =
            controller_with_files(&[(recovered, b"recovered"), (normal, b"normal")]);
        let camera = CameraId::parse("cam-a").unwrap();
        assert_eq!(
            controller.recording_days(&camera).unwrap(),
            vec!["2026-08-29"]
        );
        let start =
            NaiveDateTime::parse_from_str("2026-08-29T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let end =
            NaiveDateTime::parse_from_str("2026-08-30T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let before = controller.timeline(&camera, start, end).unwrap();
        assert_eq!(
            before
                .iter()
                .map(|row| row.recording_id.as_str())
                .collect::<Vec<_>>(),
            vec![normal, recovered]
        );
        assert!(before.iter().all(|row| row.media_duration_ms.is_none()));
        assert_eq!(before[1].kind, TimelineRecordingKind::Recovered);

        let opened = controller.open(normal).unwrap();
        assert_eq!(opened.recording.recording_id, normal);
        assert_eq!(opened.recording.media_duration_ms, Some(5_000));
        assert_eq!(opened.adjacent.next.unwrap().recording_id, recovered);
        controller.close(&opened.session_id).unwrap();
        assert_eq!(
            controller.timeline(&camera, start, end).unwrap()[0].media_duration_ms,
            Some(5_000)
        );

        controller.storage.as_mut().unwrap().rebuild().unwrap();
        let rebuilt = controller.timeline(&camera, start, end).unwrap();
        assert_eq!(
            rebuilt
                .iter()
                .map(|row| row.recording_id.as_str())
                .collect::<Vec<_>>(),
            vec![normal, recovered]
        );
        assert!(rebuilt.iter().all(|row| row.media_duration_ms.is_none()));
    }

    #[test]
    fn loopback_http_serves_ranges_and_rejects_invalid_access() {
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (_temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let opened = controller.open(relative).unwrap();
        let port = controller.server.port;
        let token = opened.session_id;

        let full = http(
            port,
            &format!("GET /playback/{token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        let (headers, body) = response_parts(&full);
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(body, b"0123456789abcdefghijklmnopqrstuvwxyz");

        let middle = http(
            port,
            &format!(
                "GET /playback/{token} HTTP/1.1\r\nHost: localhost:{port}\r\nRange: bytes=10-14\r\n\r\n"
            ),
        );
        let (headers, body) = response_parts(&middle);
        assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(headers.contains("Content-Range: bytes 10-14/36"));
        assert_eq!(body, b"abcde");

        let suffix = http(
            port,
            &format!(
                "GET /playback/{token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=-4\r\n\r\n"
            ),
        );
        assert_eq!(response_parts(&suffix).1, b"wxyz");

        let invalid = http(
            port,
            &format!(
                "GET /playback/{token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=999-\r\n\r\n"
            ),
        );
        assert!(response_parts(&invalid).0.starts_with("HTTP/1.1 416"));

        let foreign_host = http(
            port,
            &format!("GET /playback/{token} HTTP/1.1\r\nHost: 192.168.1.5:{port}\r\n\r\n"),
        );
        assert!(response_parts(&foreign_host).0.starts_with("HTTP/1.1 403"));

        let wrong = Uuid::new_v4();
        let expired = http(
            port,
            &format!("GET /playback/{wrong} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(response_parts(&expired).0.starts_with("HTTP/1.1 410"));
    }

    #[test]
    fn missing_and_expired_sessions_are_typed_and_release_pins() {
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let source = temp.path().join("recordings").join(relative);

        let opened = controller.open(relative).unwrap();
        assert!(controller.pins.is_pinned(relative));
        {
            let mut runtime = controller.runtime.lock().unwrap();
            runtime
                .sessions
                .get_mut(&opened.session_id)
                .unwrap()
                .last_activity = Instant::now() - SESSION_IDLE_TIMEOUT - Duration::from_secs(1);
        }
        assert!(!controller.session_active(&opened.session_id).unwrap());
        assert!(!controller.pins.is_pinned(relative));

        std::fs::remove_file(&source).unwrap();
        assert!(matches!(
            controller.open(relative),
            Err(PlaybackError::RecordingMissing)
        ));
    }

    #[test]
    fn active_http_request_defers_idle_expiry_close_and_pin_release() {
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (_temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let opened = controller.open(relative).unwrap();

        let request_guard = {
            let mut runtime = controller.runtime.lock().unwrap();
            let session = runtime.sessions.get_mut(&opened.session_id).unwrap();
            session.active_requests = 1;
            session.last_activity = Instant::now() - SESSION_IDLE_TIMEOUT - Duration::from_secs(1);
            SessionRequestGuard::new(controller.runtime.clone(), opened.session_id.clone())
        };

        controller.expire_sessions();
        assert!(controller.session_active(&opened.session_id).unwrap());
        assert!(controller.pins.is_pinned(relative));

        controller.close(&opened.session_id).unwrap();
        assert!(!controller.session_active(&opened.session_id).unwrap());
        assert!(controller.pins.is_pinned(relative));

        drop(request_guard);
        assert!(!controller.pins.is_pinned(relative));
        assert!(!controller.session_active(&opened.session_id).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_is_never_opened_for_playback() {
        use std::os::unix::fs::symlink;

        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let source = temp.path().join("recordings").join(relative);
        let outside = temp.path().join("outside.mkv");
        std::fs::write(&outside, b"foreign").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();

        assert!(matches!(
            controller.open(relative),
            Err(PlaybackError::RecordingStale)
        ));
        assert_eq!(std::fs::read(outside).unwrap(), b"foreign");
    }

    #[test]
    fn finalized_playback_does_not_require_the_camera_lease() {
        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let lease = nian_storage::CameraLease::try_acquire(&layout, &camera).unwrap();

        let opened = controller.open(relative).unwrap();
        assert!(controller.session_active(&opened.session_id).unwrap());
        controller.close(&opened.session_id).unwrap();
        drop(lease);
    }

    #[cfg(unix)]
    #[test]
    fn active_session_revalidates_source_before_each_range_request() {
        use std::os::unix::fs::symlink;

        let relative = "cam-a/2026/08/29/08-30-00.mkv";
        let (temp, mut controller) = controller_with_files(&[(relative, b"normal")]);
        let opened = controller.open(relative).unwrap();
        let port = controller.server.port;

        let initial = http(
            port,
            &format!(
                "GET /playback/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=0-3\r\n\r\n",
                opened.session_id
            ),
        );
        assert!(response_parts(&initial).0.starts_with("HTTP/1.1 206"));

        let source = temp.path().join("recordings").join(relative);
        let outside = temp.path().join("replacement.mkv");
        std::fs::write(&outside, b"foreign").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();

        let stale = http(
            port,
            &format!(
                "GET /playback/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nRange: bytes=4-7\r\n\r\n",
                opened.session_id
            ),
        );
        assert!(
            response_parts(&stale).0.starts_with("HTTP/1.1 410"),
            "replacement must invalidate new media requests"
        );
        assert!(!controller.pins.is_pinned(relative));
        assert_eq!(std::fs::read(outside).unwrap(), b"foreign");
    }
}
