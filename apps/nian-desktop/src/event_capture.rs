use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDateTime, TimeDelta, Utc};
use nian_application::{
    PersistedEventSignal, PersistedEventSink, RecordingController, RecordingControllerError,
    RecordingState, SupervisorRecordingRunnerFactory, WorkerEventClipComposer,
};
use nian_domain::CameraId;
use nian_index::DEFAULT_EVENT_RETENTION_DAYS;
use nian_storage::paths::publish_no_replace;
use nian_storage::{RecordingsLayout, inventory_recordings};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DesktopState;

const EVENT_BUFFER_SEGMENT_SECS: u64 = 2;
const EVENT_CAPTURE_POLL: Duration = Duration::from_millis(250);
const EVENT_PRE_ROLL: Duration = Duration::from_secs(5);
const EVENT_POST_ROLL: Duration = Duration::from_secs(5);
const EVENT_BUFFER_IDLE_RETENTION: Duration = Duration::from_secs(30);
const EVENT_CLIP_MAX_DURATION: Duration = Duration::from_secs(5 * 60);
const EVENT_CLIP_HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(60);
const MAX_EVENT_CLIP_EPISODES_PER_PASS: usize = 100_000;
const MAX_ALIAS_BYTES: u64 = 4 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug)]
struct EventCaptureSink {
    sender: mpsc::Sender<PersistedEventSignal>,
}

impl PersistedEventSink for EventCaptureSink {
    fn try_publish(&self, signal: PersistedEventSignal) {
        if self.sender.send(signal).is_err() {
            tracing::warn!("event capture dispatcher is unavailable");
        }
    }
}

pub(crate) struct EventCaptureDispatcher {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sink: Arc<EventCaptureSink>,
}

impl EventCaptureDispatcher {
    pub(crate) fn new(state: Weak<DesktopState>, worker_program: String) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let controller_program = worker_program.clone();
        let thread = std::thread::Builder::new()
            .name("event-capture-dispatcher".to_owned())
            .spawn(move || {
                let mut controller =
                    RecordingController::with_factory(Arc::new(SupervisorRecordingRunnerFactory {
                        worker_program: controller_program,
                    }));
                let composer = WorkerEventClipComposer::new(worker_program);
                let mut buffers = HashMap::<CameraId, BufferRuntime>::new();
                let mut episodes = HashMap::<CameraId, ActiveEpisode>::new();
                let mut aggregate_motion = HashMap::<CameraId, bool>::new();
                let mut next_clip_housekeeping = Instant::now();

                while worker_running.load(Ordering::Acquire) {
                    match receiver.recv_timeout(EVENT_CAPTURE_POLL) {
                        Ok(signal) => {
                            apply_signal(
                                &mut episodes,
                                &mut aggregate_motion,
                                signal,
                                Instant::now(),
                            );
                            while let Ok(signal) = receiver.try_recv() {
                                apply_signal(
                                    &mut episodes,
                                    &mut aggregate_motion,
                                    signal,
                                    Instant::now(),
                                );
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }

                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    reconcile(
                        &state,
                        &mut controller,
                        &composer,
                        &mut buffers,
                        &mut episodes,
                        &aggregate_motion,
                    );
                    if Instant::now() >= next_clip_housekeeping {
                        if let Err(error) = prune_event_clip_retention(&state) {
                            tracing::warn!(%error, "event clip retention pass failed");
                        }
                        next_clip_housekeeping = Instant::now() + EVENT_CLIP_HOUSEKEEPING_INTERVAL;
                    }
                }

                let _ = controller.shutdown_all();
            })?;
        Ok(Self {
            running,
            thread: Some(thread),
            sink: Arc::new(EventCaptureSink { sender }),
        })
    }

    pub(crate) fn sink(&self) -> Arc<dyn PersistedEventSink> {
        self.sink.clone()
    }
}

impl std::fmt::Debug for EventCaptureDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCaptureDispatcher")
            .finish_non_exhaustive()
    }
}

impl Drop for EventCaptureDispatcher {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Clone)]
struct BufferRuntime {
    manual_storage_root: PathBuf,
    buffer_root: PathBuf,
    next_start_attempt: Instant,
}

#[derive(Debug, Clone)]
struct ActiveEpisode {
    episode_id: String,
    event_ids: Vec<u64>,
    trigger_utc: DateTime<Utc>,
    ended_utc: Option<DateTime<Utc>>,
    finalize_after: Option<Instant>,
}

impl ActiveEpisode {
    fn new(signal: &PersistedEventSignal) -> Self {
        Self {
            episode_id: Uuid::new_v4().to_string(),
            event_ids: vec![signal.event_id],
            trigger_utc: signal.received_time_utc,
            ended_utc: None,
            finalize_after: None,
        }
    }

    fn add_event(&mut self, event_id: u64) {
        if !self.event_ids.contains(&event_id) {
            self.event_ids.push(event_id);
        }
    }

    fn requested_window(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let pre = TimeDelta::from_std(EVENT_PRE_ROLL).ok()?;
        let post = TimeDelta::from_std(EVENT_POST_ROLL).ok()?;
        let max = TimeDelta::from_std(EVENT_CLIP_MAX_DURATION).ok()?;
        let start = self.trigger_utc.checked_sub_signed(pre)?;
        let end = self
            .ended_utc
            .and_then(|ended| ended.checked_add_signed(post))
            .unwrap_or_else(|| {
                self.trigger_utc
                    .checked_add_signed(max)
                    .unwrap_or(self.trigger_utc)
            });
        let capped = start.checked_add_signed(max).unwrap_or(end);
        Some((start, end.min(capped)))
    }
}

fn apply_signal(
    episodes: &mut HashMap<CameraId, ActiveEpisode>,
    aggregate_motion: &mut HashMap<CameraId, bool>,
    signal: PersistedEventSignal,
    now: Instant,
) {
    let Some(motion_active) = signal.motion_active else {
        return;
    };
    let Ok(camera_id) = CameraId::parse(&signal.camera_id) else {
        tracing::warn!("persisted motion signal carried an invalid camera id");
        return;
    };
    aggregate_motion.insert(camera_id.clone(), motion_active);

    if motion_active {
        let episode = episodes
            .entry(camera_id)
            .or_insert_with(|| ActiveEpisode::new(&signal));
        episode.add_event(signal.event_id);
        episode.ended_utc = None;
        episode.finalize_after = None;
    } else if let Some(episode) = episodes.get_mut(&camera_id) {
        episode.add_event(signal.event_id);
        episode.ended_utc = Some(signal.received_time_utc);
        episode.finalize_after = Some(now + EVENT_POST_ROLL);
    }
}

fn reconcile(
    state: &DesktopState,
    controller: &mut RecordingController,
    composer: &WorkerEventClipComposer,
    buffers: &mut HashMap<CameraId, BufferRuntime>,
    episodes: &mut HashMap<CameraId, ActiveEpisode>,
    aggregate_motion: &HashMap<CameraId, bool>,
) {
    let running = state
        .lifecycle
        .state()
        .is_ok_and(|lifecycle| lifecycle == nian_application::DesktopLifecycleState::Running);
    if !running {
        if let Err(error) = controller.shutdown_all() {
            tracing::warn!(
                ?error,
                "event capture buffers could not stop for lifecycle transition"
            );
        }
        buffers.clear();
        return;
    }

    let desired = match state.event_controller.statuses() {
        Ok(statuses) => statuses
            .into_iter()
            .filter(|status| status.desired)
            .filter_map(|status| CameraId::parse(&status.camera_id).ok())
            .collect::<HashSet<_>>(),
        Err(error) => {
            tracing::warn!(?error, "event capture desired state is unavailable");
            return;
        }
    };

    reconcile_buffers(state, controller, buffers, &desired);
    reconcile_episode_deadlines(episodes, aggregate_motion);
    finalize_ready_episodes(controller, composer, buffers, episodes, aggregate_motion);
    prune_buffers(buffers, episodes);
}

fn reconcile_buffers(
    state: &DesktopState,
    controller: &mut RecordingController,
    buffers: &mut HashMap<CameraId, BufferRuntime>,
    desired_cameras: &HashSet<CameraId>,
) {
    let known = buffers.keys().cloned().collect::<Vec<_>>();
    for camera_id in known {
        let status = controller.status(&camera_id).unwrap_or_default();
        if !desired_cameras.contains(&camera_id) {
            if status.state.is_active() && status.state != RecordingState::Stopping {
                if let Err(error) = controller.stop(&camera_id)
                    && !matches!(error, RecordingControllerError::NotRecording)
                {
                    tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer stop failed");
                }
            } else if !status.state.is_active() {
                buffers.remove(&camera_id);
            }
        }
    }

    for camera_id in desired_cameras {
        let now = Instant::now();
        if let Some(runtime) = buffers.get(camera_id) {
            let status = controller.status(camera_id).unwrap_or_default();
            if status.state.is_active() || now < runtime.next_start_attempt {
                continue;
            }
        }

        let mut prepared = match state
            .camera_service
            .lock()
            .ok()
            .and_then(|service| service.prepare_recording(camera_id.as_str()).ok())
        {
            Some(prepared) => prepared,
            None => continue,
        };
        let manual_storage_root = PathBuf::from(&prepared.storage_root);
        let buffer_root = match ensure_event_subdir(&manual_storage_root, "event-buffer") {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(camera_id = %camera_id.as_str(), %error, "event buffer directory could not be prepared");
                continue;
            }
        };

        let root_changed = buffers
            .get(camera_id)
            .is_some_and(|runtime| runtime.buffer_root != buffer_root);
        if root_changed {
            let status = controller.status(camera_id).unwrap_or_default();
            if status.state.is_active() {
                if status.state != RecordingState::Stopping {
                    let _ = controller.stop(camera_id);
                }
                continue;
            }
            buffers.remove(camera_id);
        }

        prepared.storage_root = buffer_root.to_string_lossy().into_owned();
        prepared.segment_target_secs = EVENT_BUFFER_SEGMENT_SECS;
        match controller.start(camera_id.clone(), prepared) {
            Ok(_) => {
                buffers.insert(
                    camera_id.clone(),
                    BufferRuntime {
                        manual_storage_root,
                        buffer_root,
                        next_start_attempt: now + Duration::from_secs(2),
                    },
                );
                tracing::info!(
                    camera_id = %camera_id.as_str(),
                    "independent motion event buffer started"
                );
            }
            Err(RecordingControllerError::AlreadyRecording) => {}
            Err(error) => {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer start failed");
                buffers.insert(
                    camera_id.clone(),
                    BufferRuntime {
                        manual_storage_root,
                        buffer_root,
                        next_start_attempt: now + Duration::from_secs(2),
                    },
                );
            }
        }
    }
}

fn reconcile_episode_deadlines(
    episodes: &mut HashMap<CameraId, ActiveEpisode>,
    aggregate_motion: &HashMap<CameraId, bool>,
) {
    let now_utc = Utc::now();
    let now = Instant::now();
    let max =
        TimeDelta::from_std(EVENT_CLIP_MAX_DURATION).unwrap_or_else(|_| TimeDelta::minutes(5));
    for (camera_id, episode) in episodes.iter_mut() {
        if episode.ended_utc.is_none() && now_utc.signed_duration_since(episode.trigger_utc) >= max
        {
            episode.ended_utc = episode.trigger_utc.checked_add_signed(max);
            episode.finalize_after = Some(now);
        }
        if !aggregate_motion.get(camera_id).copied().unwrap_or(false)
            && episode.ended_utc.is_some()
            && episode.finalize_after.is_none()
        {
            episode.finalize_after = Some(now);
        }
    }
}

fn continuation_episode(episode: &ActiveEpisode) -> ActiveEpisode {
    let trigger_utc = episode
        .requested_window()
        .map(|(_, end)| end)
        .unwrap_or_else(Utc::now);
    ActiveEpisode {
        episode_id: Uuid::new_v4().to_string(),
        event_ids: episode.event_ids.clone(),
        trigger_utc,
        ended_utc: None,
        finalize_after: None,
    }
}

fn finalize_ready_episodes(
    controller: &mut RecordingController,
    composer: &WorkerEventClipComposer,
    buffers: &HashMap<CameraId, BufferRuntime>,
    episodes: &mut HashMap<CameraId, ActiveEpisode>,
    aggregate_motion: &HashMap<CameraId, bool>,
) {
    let now = Instant::now();
    let ready = episodes
        .iter()
        .filter(|(_, episode)| {
            episode
                .finalize_after
                .is_some_and(|deadline| now >= deadline)
        })
        .map(|(camera_id, _)| camera_id.clone())
        .collect::<Vec<_>>();

    for camera_id in ready {
        let Some(runtime) = buffers.get(&camera_id) else {
            continue;
        };
        let Some(episode) = episodes.get(&camera_id).cloned() else {
            continue;
        };
        let buffer_active = controller
            .status(&camera_id)
            .map(|status| status.state.is_active())
            .unwrap_or(false);
        match materialize_episode(runtime, &camera_id, &episode, composer, buffer_active) {
            Ok(true) => {
                episodes.remove(&camera_id);
                if aggregate_motion.get(&camera_id).copied().unwrap_or(false) {
                    episodes.insert(camera_id.clone(), continuation_episode(&episode));
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                camera_id = %camera_id.as_str(),
                error = %error,
                "event clip materialization failed"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventClipManifest {
    version: u32,
    episode_id: String,
    camera_id: String,
    event_ids: Vec<u64>,
    trigger_utc: String,
    requested_start_utc: String,
    requested_end_utc: String,
    clip_started_at_local: String,
    source_segments: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventAlias {
    version: u32,
    episode_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct EventClipRef {
    pub(crate) episode_id: String,
    pub(crate) started_at_local: NaiveDateTime,
}

fn materialize_episode(
    runtime: &BufferRuntime,
    camera_id: &CameraId,
    episode: &ActiveEpisode,
    composer: &WorkerEventClipComposer,
    buffer_active: bool,
) -> io::Result<bool> {
    if episode.event_ids.is_empty() {
        return Ok(true);
    }
    let clip_root = ensure_event_subdir(&runtime.manual_storage_root, "event-clips")?;
    let camera_dir = clip_root.join(camera_id.as_str());
    create_real_dir_all_beneath(&clip_root, &camera_dir)?;
    let clip_dir = camera_dir.join(&episode.episode_id);
    create_real_dir_all_beneath(&camera_dir, &clip_dir)?;
    let manifest_path = clip_dir.join("manifest.json");
    if manifest_path.exists() {
        let manifest = read_manifest(&manifest_path)?;
        ensure_aliases(&runtime.manual_storage_root, camera_id, &manifest)?;
        return Ok(true);
    }

    let (requested_start, requested_end) = match episode.requested_window() {
        Some(window) => window,
        None => return Ok(false),
    };
    let layout = RecordingsLayout::new(runtime.buffer_root.clone())
        .map_err(|error| io::Error::other(error.to_string()))?;
    let inventory =
        inventory_recordings(&layout).map_err(|error| io::Error::other(error.to_string()))?;
    let recordings = inventory
        .recordings
        .into_iter()
        .filter(|recording| &recording.camera_id == camera_id)
        .collect::<Vec<_>>();
    if recordings.is_empty() {
        return Ok(false);
    }

    let start_local = requested_start.with_timezone(&Local).naive_local();
    let end_local = requested_end.with_timezone(&Local).naive_local();
    let boundary = recordings
        .iter()
        .position(|recording| recording.started_at > end_local);
    if buffer_active && boundary.is_none() {
        return Ok(false);
    }
    let end_index = boundary
        .map(|index| index.saturating_sub(1))
        .unwrap_or(recordings.len().saturating_sub(1));
    if recordings[end_index].started_at > end_local {
        return Ok(false);
    }
    let first_index = recordings[..=end_index]
        .iter()
        .rposition(|recording| recording.started_at <= start_local)
        .unwrap_or(0);
    let sources = recordings[first_index..=end_index]
        .iter()
        .map(|recording| recording.path.clone())
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Ok(false);
    }

    let clip_path = clip_dir.join("clip.mkv");
    let clip_exists = match std::fs::symlink_metadata(&clip_path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event clip path is not a real file",
                ));
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !clip_exists {
        cleanup_clip_partials(&clip_dir)?;
        let partial = clip_dir.join(format!("clip.partial-{}.mkv", Uuid::new_v4()));
        if let Err(error) = composer.compose(&sources, &partial) {
            let _ = std::fs::remove_file(&partial);
            return Err(io::Error::other(error.to_string()));
        }
        publish_no_replace(&partial, &clip_path)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }

    let manifest = EventClipManifest {
        version: MANIFEST_VERSION,
        episode_id: episode.episode_id.clone(),
        camera_id: camera_id.as_str().to_owned(),
        event_ids: episode.event_ids.clone(),
        trigger_utc: episode.trigger_utc.to_rfc3339(),
        requested_start_utc: requested_start.to_rfc3339(),
        requested_end_utc: requested_end.to_rfc3339(),
        clip_started_at_local: recordings[first_index]
            .started_at
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string(),
        source_segments: sources.len(),
    };
    write_json_atomic(&manifest_path, &manifest)?;
    ensure_aliases(&runtime.manual_storage_root, camera_id, &manifest)?;
    tracing::info!(
        camera_id = %camera_id.as_str(),
        episode_id = %episode.episode_id,
        segments = sources.len(),
        "motion event clip finalized"
    );
    Ok(true)
}

fn ensure_aliases(
    storage_root: &Path,
    camera_id: &CameraId,
    manifest: &EventClipManifest,
) -> io::Result<()> {
    let clip_root = ensure_event_subdir(storage_root, "event-clips")?;
    let camera_dir = clip_root.join(camera_id.as_str());
    create_real_dir_all_beneath(&clip_root, &camera_dir)?;
    let alias_root = camera_dir.join("by-event");
    create_real_dir_all_beneath(&camera_dir, &alias_root)?;
    let alias = EventAlias {
        version: MANIFEST_VERSION,
        episode_id: manifest.episode_id.clone(),
    };
    for event_id in &manifest.event_ids {
        let event_dir = alias_root.join(event_id.to_string());
        create_real_dir_all_beneath(&alias_root, &event_dir)?;
        write_json_idempotent(
            &event_dir.join(format!("{}.json", manifest.episode_id)),
            &alias,
        )?;
    }
    Ok(())
}

pub(crate) fn event_clips_for_event(
    storage_root: &Path,
    camera_id: &CameraId,
    event_id: u64,
) -> io::Result<Vec<EventClipRef>> {
    if event_id == 0 {
        return Ok(Vec::new());
    }
    let control_dir = storage_root.join(".nian");
    if !existing_real_directory(&control_dir)? {
        return Ok(Vec::new());
    }
    let clip_root = control_dir.join("event-clips");
    if !existing_real_directory(&clip_root)? {
        return Ok(Vec::new());
    }
    let camera_dir = clip_root.join(camera_id.as_str());
    if !existing_real_directory(&camera_dir)? {
        return Ok(Vec::new());
    }
    let alias_root = camera_dir.join("by-event");
    if !existing_real_directory(&alias_root)? {
        return Ok(Vec::new());
    }
    let event_dir = alias_root.join(event_id.to_string());
    if !existing_real_directory(&event_dir)? {
        return Ok(Vec::new());
    }

    let mut entries = std::fs::read_dir(&event_dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut clips = Vec::new();
    for entry in entries {
        let alias_path = entry.path();
        let metadata = std::fs::symlink_metadata(&alias_path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event alias entry is not a real file",
            ));
        }
        let Some(alias) = read_bounded_json::<EventAlias>(&alias_path, MAX_ALIAS_BYTES)? else {
            continue;
        };
        if alias.version != MANIFEST_VERSION || Uuid::parse_str(&alias.episode_id).is_err() {
            continue;
        }
        if alias_path.file_stem().and_then(|value| value.to_str())
            != Some(alias.episode_id.as_str())
        {
            continue;
        }
        let episode_dir = camera_dir.join(&alias.episode_id);
        if !existing_real_directory(&episode_dir)? {
            continue;
        }
        let manifest_path = episode_dir.join("manifest.json");
        let Some(manifest) =
            read_bounded_json::<EventClipManifest>(&manifest_path, MAX_MANIFEST_BYTES)?
        else {
            continue;
        };
        if manifest.version != MANIFEST_VERSION
            || manifest.episode_id != alias.episode_id
            || manifest.camera_id != camera_id.as_str()
            || !manifest.event_ids.contains(&event_id)
        {
            continue;
        }
        let clip = episode_dir.join("clip.mkv");
        let metadata = match std::fs::symlink_metadata(&clip) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let started_at_local =
            NaiveDateTime::parse_from_str(&manifest.clip_started_at_local, "%Y-%m-%dT%H:%M:%S%.3f")
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid event clip manifest time",
                    )
                })?;
        clips.push(EventClipRef {
            episode_id: manifest.episode_id,
            started_at_local,
        });
    }
    clips.sort_by_key(|clip| clip.started_at_local);
    Ok(clips)
}

#[derive(Debug, Clone)]
struct EventClipRetentionCandidate {
    camera_id: CameraId,
    episode_id: String,
    episode_dir: PathBuf,
    manifest: EventClipManifest,
    ended_at_utc: DateTime<Utc>,
    size_bytes: u64,
}

fn prune_event_clip_retention(state: &DesktopState) -> io::Result<()> {
    let settings = state
        .camera_service
        .lock()
        .map_err(|_| io::Error::other("camera settings lock is unavailable"))?
        .application_settings()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let Some(storage_root) = settings.storage_root.map(PathBuf::from) else {
        return Ok(());
    };
    let retention_days = settings
        .max_age_days
        .unwrap_or(DEFAULT_EVENT_RETENTION_DAYS)
        .max(1);
    let age_cutoff = Utc::now()
        .checked_sub_signed(TimeDelta::days(i64::from(retention_days)))
        .ok_or_else(|| io::Error::other("invalid event clip retention cutoff"))?;

    let layout = RecordingsLayout::new(storage_root.clone())
        .map_err(|error| io::Error::other(error.to_string()))?;
    let manual_usage = inventory_recordings(&layout)
        .map_err(|error| io::Error::other(error.to_string()))?
        .recordings
        .into_iter()
        .fold(0_u64, |total, recording| {
            total.saturating_add(recording.size_bytes)
        });
    let mut candidates = inventory_event_clip_candidates(&storage_root)?;
    candidates.sort_by_key(|candidate| candidate.ended_at_utc);
    let event_usage = candidates.iter().fold(0_u64, |total, candidate| {
        total.saturating_add(candidate.size_bytes)
    });
    let mut total_usage = manual_usage.saturating_add(event_usage);
    let quota = settings
        .max_storage_bytes
        .zip(settings.cleanup_target_bytes)
        .filter(|(max, target)| target <= max);
    let quota_triggered = quota.is_some_and(|(max, _)| total_usage > max);

    for candidate in candidates {
        let age_eligible = candidate.ended_at_utc < age_cutoff;
        let quota_eligible =
            quota_triggered && quota.is_some_and(|(_, target)| total_usage > target);
        if !age_eligible && !quota_eligible {
            continue;
        }
        let in_use = state
            .playback_controller
            .lock()
            .map(|playback| playback.event_clip_in_use(&candidate.camera_id, &candidate.episode_id))
            .unwrap_or(true);
        if in_use {
            continue;
        }
        delete_event_clip_candidate(&storage_root, &candidate)?;
        total_usage = total_usage.saturating_sub(candidate.size_bytes);
    }
    Ok(())
}

fn inventory_event_clip_candidates(
    storage_root: &Path,
) -> io::Result<Vec<EventClipRetentionCandidate>> {
    let control_dir = storage_root.join(".nian");
    if !existing_real_directory(&control_dir)? {
        return Ok(Vec::new());
    }
    let clip_root = control_dir.join("event-clips");
    if !existing_real_directory(&clip_root)? {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for camera_entry in std::fs::read_dir(&clip_root)? {
        let camera_entry = camera_entry?;
        let camera_path = camera_entry.path();
        let metadata = std::fs::symlink_metadata(&camera_path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event clip camera path is not a real directory",
            ));
        }
        let Some(camera_name) = camera_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(camera_id) = CameraId::parse(&camera_name) else {
            continue;
        };
        for episode_entry in std::fs::read_dir(&camera_path)? {
            let episode_entry = episode_entry?;
            if episode_entry.file_name() == "by-event" {
                continue;
            }
            if candidates.len() >= MAX_EVENT_CLIP_EPISODES_PER_PASS {
                return Err(io::Error::other(
                    "event clip inventory exceeds bounded scan limit",
                ));
            }
            let episode_path = episode_entry.path();
            let metadata = std::fs::symlink_metadata(&episode_path)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event clip episode path is not a real directory",
                ));
            }
            let Some(episode_id) = episode_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if Uuid::parse_str(&episode_id).is_err() {
                continue;
            }
            let Some(manifest) = read_bounded_json::<EventClipManifest>(
                &episode_path.join("manifest.json"),
                MAX_MANIFEST_BYTES,
            )?
            else {
                continue;
            };
            if manifest.version != MANIFEST_VERSION
                || manifest.episode_id != episode_id
                || manifest.camera_id != camera_id.as_str()
            {
                continue;
            }
            let clip_path = episode_path.join("clip.mkv");
            let clip_metadata = match std::fs::symlink_metadata(&clip_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !clip_metadata.is_file() || clip_metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event clip media is not a real file",
                ));
            }
            let ended_at_utc = DateTime::parse_from_rfc3339(&manifest.requested_end_utc)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid event clip end time")
                })?
                .with_timezone(&Utc);
            candidates.push(EventClipRetentionCandidate {
                camera_id: camera_id.clone(),
                episode_id,
                episode_dir: episode_path,
                manifest,
                ended_at_utc,
                size_bytes: clip_metadata.len(),
            });
        }
    }
    Ok(candidates)
}

fn delete_event_clip_candidate(
    storage_root: &Path,
    candidate: &EventClipRetentionCandidate,
) -> io::Result<()> {
    let control_dir = storage_root.join(".nian");
    let clip_root = event_clip_root(storage_root);
    let camera_dir = event_clip_camera_dir(storage_root, &candidate.camera_id);
    for directory in [
        &control_dir,
        &clip_root,
        &camera_dir,
        &candidate.episode_dir,
    ] {
        if !existing_real_directory(directory)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "event clip retention directory is unavailable",
            ));
        }
    }
    let clip_path = candidate.episode_dir.join("clip.mkv");
    let manifest_path = candidate.episode_dir.join("manifest.json");
    for path in [&clip_path, &manifest_path] {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event clip retention target is not a real file",
            ));
        }
    }
    let entries = std::fs::read_dir(&candidate.episode_dir)?.collect::<Result<Vec<_>, _>>()?;
    let unexpected = entries
        .into_iter()
        .map(|entry| entry.file_name())
        .any(|name| name != "clip.mkv" && name != "manifest.json");
    if unexpected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "event clip episode contains unexpected files",
        ));
    }

    std::fs::remove_file(&clip_path)?;
    std::fs::remove_file(&manifest_path)?;
    std::fs::remove_dir(&candidate.episode_dir)?;

    let alias_root = camera_dir.join("by-event");
    for event_id in &candidate.manifest.event_ids {
        let event_dir = alias_root.join(event_id.to_string());
        let alias_path = event_dir.join(format!("{}.json", candidate.episode_id));
        match std::fs::symlink_metadata(&alias_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                std::fs::remove_file(&alias_path)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event clip alias is not a real file",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match std::fs::remove_dir(&event_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn prune_buffers(
    buffers: &HashMap<CameraId, BufferRuntime>,
    episodes: &HashMap<CameraId, ActiveEpisode>,
) {
    let idle =
        TimeDelta::from_std(EVENT_BUFFER_IDLE_RETENTION).unwrap_or_else(|_| TimeDelta::seconds(30));
    let pre = TimeDelta::from_std(EVENT_PRE_ROLL).unwrap_or_else(|_| TimeDelta::seconds(5));
    for (camera_id, runtime) in buffers {
        let layout = match RecordingsLayout::new(runtime.buffer_root.clone()) {
            Ok(layout) => layout,
            Err(_) => continue,
        };
        let inventory = match inventory_recordings(&layout) {
            Ok(inventory) => inventory,
            Err(error) => {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer inventory failed");
                continue;
            }
        };
        let recordings = inventory
            .recordings
            .into_iter()
            .filter(|recording| &recording.camera_id == camera_id)
            .collect::<Vec<_>>();
        if recordings.len() < 2 {
            continue;
        }
        let idle_cutoff = Local::now().naive_local() - idle;
        let keep_from = episodes
            .get(camera_id)
            .map(|episode| {
                episode
                    .trigger_utc
                    .checked_sub_signed(pre)
                    .unwrap_or(episode.trigger_utc)
                    .with_timezone(&Local)
                    .naive_local()
                    .min(idle_cutoff)
            })
            .unwrap_or(idle_cutoff);
        for pair in recordings.windows(2) {
            if pair[1].started_at <= keep_from
                && let Err(error) = std::fs::remove_file(&pair[0].path)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(
                    camera_id = %camera_id.as_str(),
                    path = %pair[0].path.display(),
                    %error,
                    "event buffer prune failed"
                );
            }
        }
    }
}

#[cfg(test)]
fn event_buffer_root(storage_root: &Path) -> PathBuf {
    storage_root.join(".nian").join("event-buffer")
}

fn event_clip_root(storage_root: &Path) -> PathBuf {
    storage_root.join(".nian").join("event-clips")
}

fn event_clip_camera_dir(storage_root: &Path, camera_id: &CameraId) -> PathBuf {
    event_clip_root(storage_root).join(camera_id.as_str())
}

#[cfg(test)]
fn event_clip_episode_dir(storage_root: &Path, camera_id: &CameraId, episode_id: &str) -> PathBuf {
    event_clip_camera_dir(storage_root, camera_id).join(episode_id)
}

fn ensure_event_subdir(storage_root: &Path, name: &str) -> io::Result<PathBuf> {
    if !matches!(name, "event-buffer" | "event-clips") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid event capture subdirectory",
        ));
    }
    let layout = RecordingsLayout::new(storage_root.to_path_buf())
        .map_err(|error| io::Error::other(error.to_string()))?;
    let control = layout
        .ensure_control_dir()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let target = control.join(name);
    create_real_dir_all_beneath(&control, &target)?;
    Ok(target)
}

fn create_real_dir_all_beneath(base: &Path, target: &Path) -> io::Result<()> {
    if !existing_real_directory(base)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "event capture base directory is missing",
        ));
    }
    let relative = target.strip_prefix(base).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "event capture path escaped base",
        )
    })?;
    let mut cursor = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "event capture path contains an invalid component",
            ));
        };
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "event capture path is not a real directory",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(&cursor) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                let metadata = std::fs::symlink_metadata(&cursor)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "event capture path is not a real directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn existing_real_directory(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "event capture path is not a real directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn cleanup_clip_partials(clip_dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(clip_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("clip.partial-") || !name.ends_with(".mkv") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "event clip partial is not a real file",
            ));
        }
        std::fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn write_json_idempotent<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    match std::fs::read(path) {
        Ok(existing) => {
            if existing == bytes {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "event capture metadata conflicts with an existing immutable entry",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("event capture path has no parent"))?;
    if !existing_real_directory(parent)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "event capture metadata parent is unavailable",
        ));
    }
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("event"),
        Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    match publish_no_replace(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            match std::fs::read(path) {
                Ok(existing) if existing == bytes => Ok(()),
                _ => Err(io::Error::other(error.to_string())),
            }
        }
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("event capture path has no parent"))?;
    if !existing_real_directory(parent)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "event capture metadata parent is missing",
        ));
    }
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event capture metadata path is not a real file",
                ));
            }
            if std::fs::read(path)? == bytes {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "event capture metadata conflicts with existing data",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("event"),
        Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = publish_no_replace(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(io::Error::other(error.to_string()));
    }
    Ok(())
}

fn read_manifest(path: &Path) -> io::Result<EventClipManifest> {
    read_bounded_json(path, MAX_MANIFEST_BYTES)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "event clip manifest missing"))
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path, max: u64) -> io::Result<Option<T>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid event capture metadata"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use nian_application::EventHistoryKind;

    fn signal(event_id: u64, active: bool, at: DateTime<Utc>) -> PersistedEventSignal {
        PersistedEventSignal {
            event_id,
            camera_id: "cam-front".to_owned(),
            camera_display_name: "Front".to_owned(),
            kind: if active {
                EventHistoryKind::MotionStarted
            } else {
                EventHistoryKind::MotionEnded
            },
            motion_active: Some(active),
            received_time_utc: at,
        }
    }

    #[test]
    fn aggregate_motion_builds_one_episode_and_post_roll_is_cancelled_by_reactivation() {
        let camera = CameraId::parse("cam-front").unwrap();
        let at = Utc::now();
        let mut episodes = HashMap::new();
        let mut aggregate = HashMap::new();
        apply_signal(
            &mut episodes,
            &mut aggregate,
            signal(1, true, at),
            Instant::now(),
        );
        apply_signal(
            &mut episodes,
            &mut aggregate,
            signal(2, false, at + TimeDelta::seconds(1)),
            Instant::now(),
        );
        assert!(episodes[&camera].finalize_after.is_some());
        apply_signal(
            &mut episodes,
            &mut aggregate,
            signal(3, true, at + TimeDelta::seconds(2)),
            Instant::now(),
        );
        assert!(episodes[&camera].finalize_after.is_none());
        assert_eq!(episodes[&camera].event_ids, vec![1, 2, 3]);
    }

    #[test]
    fn requested_window_contains_real_preroll_and_is_bounded() {
        let at = Utc::now();
        let mut episode = ActiveEpisode::new(&signal(1, true, at));
        episode.ended_utc = Some(at + TimeDelta::seconds(3));
        let (start, end) = episode.requested_window().unwrap();
        assert_eq!(start, at - TimeDelta::seconds(5));
        assert_eq!(end, at + TimeDelta::seconds(8));
        assert!(end.signed_duration_since(start) <= TimeDelta::minutes(5));
    }

    #[test]
    fn event_clip_alias_is_separate_from_manual_recording_tree() {
        let root = PathBuf::from(if cfg!(windows) { r"C:\nian" } else { "/nian" });
        let camera = CameraId::parse("cam-front").unwrap();
        let clip = event_clip_episode_dir(&root, &camera, "episode");
        let buffer = event_buffer_root(&root);
        assert!(clip.starts_with(root.join(".nian").join("event-clips")));
        assert!(buffer.starts_with(root.join(".nian").join("event-buffer")));
        assert!(!clip.starts_with(root.join(camera.as_str())));
    }

    #[test]
    fn event_alias_round_trip_resolves_only_the_dedicated_event_clip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let camera = CameraId::parse("cam-front").unwrap();
        let episode_id = Uuid::new_v4().to_string();
        let clip_root = ensure_event_subdir(&root, "event-clips").unwrap();
        let camera_dir = clip_root.join(camera.as_str());
        create_real_dir_all_beneath(&clip_root, &camera_dir).unwrap();
        let episode_dir = camera_dir.join(&episode_id);
        create_real_dir_all_beneath(&camera_dir, &episode_dir).unwrap();
        std::fs::write(episode_dir.join("clip.mkv"), b"event-only-clip").unwrap();

        let at = Utc::now();
        let local = at.with_timezone(&Local).naive_local();
        let local_text = local.format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
        let expected_local =
            NaiveDateTime::parse_from_str(&local_text, "%Y-%m-%dT%H:%M:%S%.3f").unwrap();
        let manifest = EventClipManifest {
            version: MANIFEST_VERSION,
            episode_id: episode_id.clone(),
            camera_id: camera.as_str().to_owned(),
            event_ids: vec![41, 42],
            trigger_utc: at.to_rfc3339(),
            requested_start_utc: (at - TimeDelta::seconds(5)).to_rfc3339(),
            requested_end_utc: (at + TimeDelta::seconds(10)).to_rfc3339(),
            clip_started_at_local: local_text,
            source_segments: 4,
        };
        write_json_atomic(&episode_dir.join("manifest.json"), &manifest).unwrap();
        ensure_aliases(&root, &camera, &manifest).unwrap();

        let resolved = event_clips_for_event(&root, &camera, 41).unwrap().remove(0);
        assert_eq!(resolved.episode_id, episode_id);
        assert_eq!(resolved.started_at_local, expected_local);
        assert!(
            event_clips_for_event(&root, &camera, 999)
                .unwrap()
                .is_empty()
        );
        assert!(!root.join(camera.as_str()).exists());
    }

    #[test]
    fn metadata_publication_is_idempotent_but_never_retargets_an_event() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let clip_root = ensure_event_subdir(&root, "event-clips").unwrap();
        let path = clip_root.join("alias.json");
        let first = EventAlias {
            version: MANIFEST_VERSION,
            episode_id: Uuid::new_v4().to_string(),
        };
        write_json_atomic(&path, &first).unwrap();
        write_json_atomic(&path, &first).unwrap();

        let second = EventAlias {
            version: MANIFEST_VERSION,
            episode_id: Uuid::new_v4().to_string(),
        };
        let error = write_json_atomic(&path, &second).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn continuous_motion_continuation_keeps_root_event_aliases() {
        let at = Utc::now();
        let mut episode = ActiveEpisode::new(&signal(11, true, at));
        episode.add_event(12);
        episode.ended_utc = Some(at + TimeDelta::minutes(5));
        let continuation = continuation_episode(&episode);
        assert_eq!(continuation.event_ids, vec![11, 12]);
        assert!(continuation.trigger_utc > episode.trigger_utc);
        assert!(continuation.ended_utc.is_none());
    }

    #[test]
    fn one_motion_event_resolves_multiple_immutable_clip_parts_in_time_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let camera = CameraId::parse("cam-front").unwrap();
        let clip_root = ensure_event_subdir(&root, "event-clips").unwrap();
        let camera_dir = clip_root.join(camera.as_str());
        create_real_dir_all_beneath(&clip_root, &camera_dir).unwrap();
        let base = Local::now().naive_local();
        let mut expected = Vec::new();

        for (offset, event_id) in [(300_i64, 77_u64), (0_i64, 77_u64)] {
            let episode_id = Uuid::new_v4().to_string();
            let episode_dir = camera_dir.join(&episode_id);
            create_real_dir_all_beneath(&camera_dir, &episode_dir).unwrap();
            std::fs::write(episode_dir.join("clip.mkv"), b"clip-part").unwrap();
            let started = base + TimeDelta::seconds(offset);
            let started_text = started.format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
            let normalized_started =
                NaiveDateTime::parse_from_str(&started_text, "%Y-%m-%dT%H:%M:%S%.3f").unwrap();
            let manifest = EventClipManifest {
                version: MANIFEST_VERSION,
                episode_id: episode_id.clone(),
                camera_id: camera.as_str().to_owned(),
                event_ids: vec![event_id],
                trigger_utc: Utc::now().to_rfc3339(),
                requested_start_utc: Utc::now().to_rfc3339(),
                requested_end_utc: Utc::now().to_rfc3339(),
                clip_started_at_local: started_text,
                source_segments: 2,
            };
            write_json_atomic(&episode_dir.join("manifest.json"), &manifest).unwrap();
            ensure_aliases(&root, &camera, &manifest).unwrap();
            expected.push((normalized_started, episode_id));
        }
        expected.sort_by_key(|(started, _)| *started);

        let clips = event_clips_for_event(&root, &camera, 77).unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].started_at_local, expected[0].0);
        assert_eq!(clips[0].episode_id, expected[0].1);
        assert_eq!(clips[1].started_at_local, expected[1].0);
        assert_eq!(clips[1].episode_id, expected[1].1);
    }

    #[test]
    fn retention_removes_only_selected_clip_part_and_its_alias() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let camera = CameraId::parse("cam-front").unwrap();
        let clip_root = ensure_event_subdir(&root, "event-clips").unwrap();
        let camera_dir = clip_root.join(camera.as_str());
        create_real_dir_all_beneath(&clip_root, &camera_dir).unwrap();
        let base = Utc::now();
        let mut candidates = Vec::new();

        for offset_minutes in [0_i64, 5_i64] {
            let episode_id = Uuid::new_v4().to_string();
            let episode_dir = camera_dir.join(&episode_id);
            create_real_dir_all_beneath(&camera_dir, &episode_dir).unwrap();
            let clip_path = episode_dir.join("clip.mkv");
            std::fs::write(&clip_path, b"event-clip-part").unwrap();
            let ended = base + TimeDelta::minutes(offset_minutes);
            let started = ended.with_timezone(&Local).naive_local() - TimeDelta::minutes(5);
            let manifest = EventClipManifest {
                version: MANIFEST_VERSION,
                episode_id: episode_id.clone(),
                camera_id: camera.as_str().to_owned(),
                event_ids: vec![88],
                trigger_utc: base.to_rfc3339(),
                requested_start_utc: (ended - TimeDelta::minutes(5)).to_rfc3339(),
                requested_end_utc: ended.to_rfc3339(),
                clip_started_at_local: started.format("%Y-%m-%dT%H:%M:%S%.3f").to_string(),
                source_segments: 2,
            };
            write_json_atomic(&episode_dir.join("manifest.json"), &manifest).unwrap();
            ensure_aliases(&root, &camera, &manifest).unwrap();
            candidates.push(EventClipRetentionCandidate {
                camera_id: camera.clone(),
                episode_id,
                episode_dir,
                manifest,
                ended_at_utc: ended,
                size_bytes: std::fs::metadata(&clip_path).unwrap().len(),
            });
        }

        delete_event_clip_candidate(&root, &candidates[0]).unwrap();
        let clips = event_clips_for_event(&root, &camera, 88).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].episode_id, candidates[1].episode_id);
        assert!(!candidates[0].episode_dir.exists());
        assert!(candidates[1].episode_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn event_clip_lookup_rejects_symlinked_control_subtrees() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let layout = RecordingsLayout::new(root.clone()).unwrap();
        let control = layout.ensure_control_dir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, control.join("event-clips")).unwrap();

        let camera = CameraId::parse("cam-front").unwrap();
        let error = event_clips_for_event(&root, &camera, 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
