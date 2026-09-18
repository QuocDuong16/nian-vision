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
    BrokerEventBufferRunnerFactory, CameraMotionLease, CameraMotionStatus, CameraPreRollLease,
    CameraWorkerBroker, CameraWorkerError, EventRuntimeState, PersistedEventSignal,
    PersistedEventSink, RecordingController, RecordingControllerError, RecordingState,
    WorkerEventClipComposer,
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
const PRE_ROLL_RETRY_DELAY: Duration = Duration::from_secs(2);
const PRE_ROLL_HEALTH_INTERVAL: Duration = Duration::from_secs(5);
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
    pub(crate) fn new(
        state: Weak<DesktopState>,
        worker_program: String,
        camera_workers: CameraWorkerBroker,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let thread = std::thread::Builder::new()
            .name("event-capture-dispatcher".to_owned())
            .spawn(move || {
                let mut controller =
                    RecordingController::with_factory(Arc::new(BrokerEventBufferRunnerFactory {
                        broker: camera_workers.clone(),
                    }));
                let composer = WorkerEventClipComposer::new(worker_program);
                let mut runtime = EventCaptureRuntime::default();
                let mut next_clip_housekeeping = Instant::now();

                while worker_running.load(Ordering::Acquire) {
                    match receiver.recv_timeout(EVENT_CAPTURE_POLL) {
                        Ok(signal) => {
                            apply_signal(
                                &mut runtime.active_episodes,
                                &mut runtime.pending_episodes,
                                &mut runtime.aggregate_motion,
                                signal,
                                Instant::now(),
                            );
                            while let Ok(signal) = receiver.try_recv() {
                                apply_signal(
                                    &mut runtime.active_episodes,
                                    &mut runtime.pending_episodes,
                                    &mut runtime.aggregate_motion,
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
                        &camera_workers,
                        &mut controller,
                        &composer,
                        &mut runtime,
                    );
                    if Instant::now() >= next_clip_housekeeping {
                        if let Err(error) = prune_event_clip_retention(&state) {
                            tracing::warn!(%error, "event clip retention pass failed");
                        }
                        next_clip_housekeeping = Instant::now() + EVENT_CLIP_HOUSEKEEPING_INTERVAL;
                    }
                }

                match controller.shutdown_all() {
                    Ok(_) => cleanup_event_buffers(&runtime.buffers),
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            "event capture buffers could not stop during dispatcher shutdown"
                        );
                    }
                }
                runtime.local_motion.clear();
                runtime.pre_rolls.clear();
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

struct PreRollRuntime {
    lease: CameraPreRollLease,
    next_health_check: Instant,
}

struct LocalMotionRuntime {
    lease: CameraMotionLease,
    /// Highest contiguous transition durably committed to EventIndex. An ACK
    /// response can be lost after the worker already removed the queue prefix.
    last_persisted: u64,
    /// Highest sequence for which the worker confirmed the removal.
    last_acknowledged: u64,
}

const LOCAL_MOTION_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Default)]
struct EventCaptureRuntime {
    pre_rolls: HashMap<CameraId, PreRollRuntime>,
    pre_roll_retry: HashMap<CameraId, Instant>,
    local_motion: HashMap<CameraId, LocalMotionRuntime>,
    local_motion_retry: HashMap<CameraId, Instant>,
    buffers: HashMap<CameraId, BufferRuntime>,
    active_episodes: HashMap<CameraId, ActiveEpisode>,
    pending_episodes: Vec<(CameraId, ActiveEpisode)>,
    aggregate_motion: HashMap<CameraId, bool>,
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
    active_episodes: &mut HashMap<CameraId, ActiveEpisode>,
    pending_episodes: &mut Vec<(CameraId, ActiveEpisode)>,
    aggregate_motion: &mut HashMap<CameraId, bool>,
    signal: PersistedEventSignal,
    now: Instant,
) {
    let Ok(camera_id) = CameraId::parse(&signal.camera_id) else {
        tracing::warn!("persisted motion signal carried an invalid camera id");
        return;
    };
    // Person classification comes from a trusted camera ONVIF topic, not a
    // second recording trigger. Link its separately persisted Event Review row
    // to the already-owned aggregate motion episode when time windows overlap.
    if matches!(
        signal.kind,
        nian_application::EventHistoryKind::PersonStarted
            | nian_application::EventHistoryKind::PersonEnded
    ) && signal.motion_active.is_none()
    {
        if let Some(episode) = active_episodes.get_mut(&camera_id)
            && signal.received_time_utc >= episode.trigger_utc
        {
            episode.add_event(signal.event_id);
            return;
        }
        if let Some((_, episode)) = pending_episodes.iter_mut().rev().find(|(id, episode)| {
            id == &camera_id
                && signal.received_time_utc >= episode.trigger_utc
                && episode
                    .ended_utc
                    .is_some_and(|end| signal.received_time_utc <= end + TimeDelta::seconds(5))
        }) {
            episode.add_event(signal.event_id);
        }
        return;
    }
    let Some(motion_active) = signal.motion_active else {
        return;
    };
    aggregate_motion.insert(camera_id.clone(), motion_active);

    if motion_active {
        let episode = active_episodes
            .entry(camera_id)
            .or_insert_with(|| ActiveEpisode::new(&signal));
        episode.add_event(signal.event_id);
    } else if let Some(mut episode) = active_episodes.remove(&camera_id) {
        episode.add_event(signal.event_id);
        episode.ended_utc = Some(signal.received_time_utc);
        episode.finalize_after = Some(now + EVENT_POST_ROLL);
        pending_episodes.push((camera_id, episode));
    }
}

fn reconcile(
    state: &DesktopState,
    camera_workers: &CameraWorkerBroker,
    controller: &mut RecordingController,
    composer: &WorkerEventClipComposer,
    runtime: &mut EventCaptureRuntime,
) {
    let running = state
        .lifecycle
        .state()
        .is_ok_and(|lifecycle| lifecycle == nian_application::DesktopLifecycleState::Running);
    if !running {
        runtime.local_motion.clear();
        runtime.local_motion_retry.clear();
        runtime.pre_rolls.clear();
        runtime.pre_roll_retry.clear();
        match controller.shutdown_all() {
            Ok(_) => {
                cleanup_event_buffers(&runtime.buffers);
                runtime.buffers.clear();
                runtime.active_episodes.clear();
                runtime.pending_episodes.clear();
                runtime.aggregate_motion.clear();
            }
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "event capture buffers could not stop for lifecycle transition"
                );
            }
        }
        return;
    }

    let statuses = match state.event_controller.statuses() {
        Ok(statuses) => statuses,
        Err(error) => {
            tracing::warn!(
                ?error,
                "event capture desired state is unavailable; settling active capture fail-closed"
            );
            Vec::new()
        }
    };
    let local_desired = match state.local_motion_settings.lock() {
        Ok(settings) => settings
            .local_motion_enabled_cameras()
            .map(|cameras| cameras.into_iter().collect::<HashSet<_>>()),
        Err(_) => Err(nian_settings::SettingsError::InvalidData(
            "local motion settings lock unavailable".to_owned(),
        )),
    }
    .unwrap_or_else(|error| {
        tracing::warn!(
            ?error,
            "local motion desired state unavailable; settling capture fail-closed"
        );
        HashSet::new()
    });
    let onvif_desired = statuses
        .iter()
        .filter(|status| status.desired)
        .filter_map(|status| CameraId::parse(&status.camera_id).ok())
        .collect::<HashSet<_>>();
    // An invalid persisted dual-mode configuration never spawns two motion
    // consumers. ONVIF remains authoritative until the operator resolves it.
    let local_desired = local_desired
        .difference(&onvif_desired)
        .cloned()
        .collect::<HashSet<_>>();
    let desired = onvif_desired
        .union(&local_desired)
        .cloned()
        .collect::<HashSet<_>>();
    let mut monitoring_unavailable = statuses
        .iter()
        .filter(|status| {
            status.desired
                && status.motion_active.is_none()
                && !matches!(status.state, EventRuntimeState::Polling)
        })
        .filter_map(|status| CameraId::parse(&status.camera_id).ok())
        .collect::<HashSet<_>>();

    // Retain the archival main ring *before* starting any substream decoder,
    // so local detection cannot race pre-roll initialization.
    reconcile_pre_roll(
        state,
        camera_workers,
        &mut runtime.pre_rolls,
        &mut runtime.pre_roll_retry,
        &desired,
    );
    // A healthy substream cannot trigger archival events when its main
    // pre-roll source is unavailable (for example, missing storage).
    let ready_main = runtime.pre_rolls.keys().cloned().collect::<HashSet<_>>();
    let (local_admitted, local_blocked) = local_motion_admission(&local_desired, &ready_main);
    monitoring_unavailable.extend(local_blocked);
    reconcile_local_motion(
        state,
        camera_workers,
        &local_admitted,
        &mut runtime.local_motion,
        &mut runtime.local_motion_retry,
        &mut monitoring_unavailable,
    );
    settle_unmonitorable_episodes(
        &mut runtime.active_episodes,
        &mut runtime.pending_episodes,
        &mut runtime.aggregate_motion,
        &desired,
        &monitoring_unavailable,
        Utc::now(),
        Instant::now(),
    );
    reconcile_episode_deadlines(
        &mut runtime.active_episodes,
        &mut runtime.pending_episodes,
        &runtime.aggregate_motion,
    );
    let capture_cameras = runtime
        .active_episodes
        .keys()
        .cloned()
        .chain(
            runtime
                .pending_episodes
                .iter()
                .map(|(camera_id, _)| camera_id.clone()),
        )
        .collect::<HashSet<_>>();
    reconcile_buffers(state, controller, &mut runtime.buffers, &capture_cameras);
    finalize_ready_episodes(
        controller,
        composer,
        &runtime.buffers,
        &mut runtime.pending_episodes,
    );
    prune_buffers(
        &runtime.buffers,
        &runtime.active_episodes,
        &runtime.pending_episodes,
    );
}

fn settle_unmonitorable_episodes(
    active_episodes: &mut HashMap<CameraId, ActiveEpisode>,
    pending_episodes: &mut Vec<(CameraId, ActiveEpisode)>,
    aggregate_motion: &mut HashMap<CameraId, bool>,
    desired_cameras: &HashSet<CameraId>,
    monitoring_unavailable: &HashSet<CameraId>,
    now_utc: DateTime<Utc>,
    now: Instant,
) {
    let cameras = active_episodes.keys().cloned().collect::<Vec<_>>();
    for camera_id in cameras {
        let desired = desired_cameras.contains(&camera_id);
        if desired && !monitoring_unavailable.contains(&camera_id) {
            continue;
        }
        let Some(mut episode) = active_episodes.remove(&camera_id) else {
            continue;
        };
        episode.ended_utc = Some(now_utc);
        episode.finalize_after = Some(now + EVENT_POST_ROLL);
        pending_episodes.push((camera_id.clone(), episode));
        aggregate_motion.remove(&camera_id);
    }

    aggregate_motion.retain(|camera_id, _| {
        desired_cameras.contains(camera_id) && !monitoring_unavailable.contains(camera_id)
    });
}

/// Only camera IDs with an owned main pre-roll lease may run the local
/// detector. Unavailable main streams must fail closed even if Grid is healthy.
fn local_motion_admission(
    desired: &HashSet<CameraId>,
    main_ready: &HashSet<CameraId>,
) -> (HashSet<CameraId>, HashSet<CameraId>) {
    (
        desired.intersection(main_ready).cloned().collect(),
        desired.difference(main_ready).cloned().collect(),
    )
}

/// No local video decoding in the desktop process: every enabled camera
/// acquires a worker-owned subscriber to the Grid/sub profile (main fallback).
/// Transitions are acknowledged only after EventIndex persistence. Errors
/// revoke authority so event clips settle rather than growing indefinitely.
fn reconcile_local_motion(
    state: &DesktopState,
    broker: &CameraWorkerBroker,
    desired: &HashSet<CameraId>,
    runtimes: &mut HashMap<CameraId, LocalMotionRuntime>,
    retries: &mut HashMap<CameraId, Instant>,
    unavailable: &mut HashSet<CameraId>,
) {
    let now = Instant::now();
    let stopped = runtimes
        .keys()
        .filter(|id| !desired.contains(*id))
        .cloned()
        .collect::<Vec<_>>();
    for camera_id in stopped {
        runtimes.remove(&camera_id);
        retries.remove(&camera_id);
    }
    for camera_id in desired {
        if runtimes.contains_key(camera_id)
            || retries.get(camera_id).is_some_and(|until| now < *until)
        {
            continue;
        }
        let prepared = state.camera_service.lock().ok().and_then(|service| {
            service
                .prepare_live_profile(
                    camera_id.as_str(),
                    nian_application::LiveStreamProfile::Grid,
                )
                .ok()
        });
        let Some(prepared) = prepared else {
            unavailable.insert(camera_id.clone());
            retries.insert(camera_id.clone(), now + LOCAL_MOTION_RETRY_DELAY);
            continue;
        };
        match broker.retain_local_motion(camera_id, prepared.source_json) {
            Ok(lease) => {
                runtimes.insert(
                    camera_id.clone(),
                    LocalMotionRuntime {
                        lease,
                        last_persisted: 0,
                        last_acknowledged: 0,
                    },
                );
                retries.remove(camera_id);
            }
            Err(error) => {
                tracing::warn!(camera_id=%camera_id.as_str(), ?error, "local motion monitor unavailable");
                unavailable.insert(camera_id.clone());
                retries.insert(camera_id.clone(), now + LOCAL_MOTION_RETRY_DELAY);
            }
        }
    }

    let mut restart = Vec::new();
    for (camera_id, runtime) in runtimes.iter_mut() {
        let status = match runtime.lease.status() {
            Ok(status) => status,
            Err(error) => {
                unavailable.insert(camera_id.clone());
                // A dead or protocol-broken worker cannot recover through a
                // stale lease. Reacquire through the broker's single-flight
                // slot. A transient timeout does not justify killing a
                // healthy shared Recorder/Live worker.
                if local_motion_requires_new_lease(&error) {
                    tracing::warn!(camera_id=%camera_id.as_str(), ?error, "reacquiring failed local motion worker lease");
                    restart.push(camera_id.clone());
                }
                continue;
            }
        };
        if local_motion_cursor_regressed(
            runtime.last_acknowledged,
            status
                .transitions
                .first()
                .map(|transition| transition.sequence),
        ) {
            unavailable.insert(camera_id.clone());
            tracing::warn!(camera_id=%camera_id.as_str(), "local motion worker sequence reset while lease was held");
            restart.push(camera_id.clone());
            continue;
        }
        // A new desktop lease can inherit the worker's *remaining* queue
        // after a previous lease durably ACKed its prefix. Starting at zero
        // would mistake that legitimate prefix for a gap and deadlock replay.
        // Only the first pending sequence may establish this initial cursor;
        // subsequent gaps still fail closed inside the loop below.
        let Some(mut persisted_cursor) = local_motion_resume_cursor(
            runtime.last_persisted,
            status
                .transitions
                .first()
                .map(|transition| transition.sequence),
        ) else {
            unavailable.insert(camera_id.clone());
            tracing::warn!(camera_id=%camera_id.as_str(), "local motion transition has invalid zero sequence");
            continue;
        };
        // Only the first lease poll can adopt a worker's already-ACKed
        // prefix. Later polls never relabel missing transitions as history.
        if runtime.last_persisted == 0 && runtime.last_acknowledged == 0 {
            runtime.last_acknowledged = persisted_cursor;
        }
        let mut persisted = true;
        for transition in &status.transitions {
            if transition.sequence <= persisted_cursor {
                continue;
            }
            if transition.sequence != persisted_cursor.saturating_add(1) {
                persisted = false;
                tracing::warn!(camera_id=%camera_id.as_str(), "local motion transition sequence gap");
                break;
            }
            let observed = match DateTime::parse_from_rfc3339(&transition.observed_at_utc) {
                Ok(observed) => observed.with_timezone(&Utc),
                Err(_) => {
                    persisted = false;
                    break;
                }
            };
            if let Err(error) = state.event_controller.persist_local_motion(
                camera_id,
                transition.sequence,
                transition.motion_active,
                observed,
            ) {
                tracing::warn!(camera_id=%camera_id.as_str(), ?error, "local motion event could not persist");
                persisted = false;
                break;
            }
            persisted_cursor = transition.sequence;
        }
        // Persisted and acknowledged are deliberately separate. If an ACK
        // reached the worker but its response was lost, the next status may
        // begin after last_acknowledged; replay still advances from the
        // durable cursor and retries an idempotent ACK without duplicate rows.
        runtime.last_persisted = persisted_cursor;
        if persisted_cursor > runtime.last_acknowledged {
            match runtime.lease.acknowledge(persisted_cursor) {
                Ok(()) => runtime.last_acknowledged = persisted_cursor,
                Err(_) => persisted = false,
            }
        }
        if !persisted || !local_motion_authoritative(&status) {
            unavailable.insert(camera_id.clone());
        }
        if matches!(status.state.as_str(), "failed" | "disabled") && status.transitions.is_empty() {
            restart.push(camera_id.clone());
        }
    }
    for camera_id in restart {
        runtimes.remove(&camera_id);
        retries.insert(camera_id, now + LOCAL_MOTION_RETRY_DELAY);
    }
}

fn local_motion_requires_new_lease(error: &CameraWorkerError) -> bool {
    matches!(
        error,
        CameraWorkerError::Unavailable | CameraWorkerError::Protocol
    )
}

/// An acknowledged transition cannot reappear at the front of this worker's
/// unacknowledged queue. Detect a reset sequence epoch rather than silently
/// dropping fresh events as if they belonged to the previous job.
fn local_motion_cursor_regressed(last_acknowledged: u64, first_pending: Option<u64>) -> bool {
    last_acknowledged != 0 && first_pending.is_some_and(|first| first <= last_acknowledged)
}

/// The worker retains only unacknowledged transitions. A replacement lease
/// has no knowledge of the previous lease's committed/ACKed prefix, so its
/// first pending sequence establishes the cursor once. Subsequent calls must
/// use the durable cursor, not the last confirmed ACK: the ACK response may
/// have been lost after the worker already dequeued its persisted prefix.
fn local_motion_resume_cursor(last_persisted: u64, first_pending: Option<u64>) -> Option<u64> {
    if last_persisted != 0 {
        Some(last_persisted)
    } else {
        first_pending.map_or(Some(0), |sequence| sequence.checked_sub(1))
    }
}

fn local_motion_authoritative(status: &CameraMotionStatus) -> bool {
    status.state == "monitoring"
        && status.motion_active.is_some()
        && status.last_error_code.is_none()
}

fn reconcile_pre_roll(
    state: &DesktopState,
    camera_workers: &CameraWorkerBroker,
    pre_rolls: &mut HashMap<CameraId, PreRollRuntime>,
    retry_after: &mut HashMap<CameraId, Instant>,
    desired_cameras: &HashSet<CameraId>,
) {
    let now = Instant::now();
    let known = pre_rolls.keys().cloned().collect::<Vec<_>>();
    for camera_id in known {
        if !desired_cameras.contains(&camera_id) {
            pre_rolls.remove(&camera_id);
            retry_after.remove(&camera_id);
            continue;
        }
        let unhealthy = pre_rolls.get_mut(&camera_id).is_some_and(|runtime| {
            if now < runtime.next_health_check {
                return false;
            }
            runtime.next_health_check = now + PRE_ROLL_HEALTH_INTERVAL;
            !runtime.lease.is_healthy()
        });
        if unhealthy {
            pre_rolls.remove(&camera_id);
            retry_after.insert(camera_id, now + PRE_ROLL_RETRY_DELAY);
        }
    }

    for camera_id in desired_cameras {
        if pre_rolls.contains_key(camera_id)
            || retry_after
                .get(camera_id)
                .is_some_and(|deadline| now < *deadline)
        {
            continue;
        }
        let prepared = state
            .camera_service
            .lock()
            .ok()
            .and_then(|service| service.prepare_recording(camera_id.as_str()).ok());
        let Some(prepared) = prepared else {
            retry_after.insert(camera_id.clone(), now + PRE_ROLL_RETRY_DELAY);
            continue;
        };
        match camera_workers.retain_pre_roll(camera_id, prepared.source_json) {
            Ok(lease) => {
                pre_rolls.insert(
                    camera_id.clone(),
                    PreRollRuntime {
                        lease,
                        next_health_check: now + PRE_ROLL_HEALTH_INTERVAL,
                    },
                );
                retry_after.remove(camera_id);
                tracing::info!(camera_id = %camera_id.as_str(), "compressed event pre-roll retained");
            }
            Err(error) => {
                retry_after.insert(camera_id.clone(), now + PRE_ROLL_RETRY_DELAY);
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "compressed event pre-roll unavailable");
            }
        }
    }
}

fn reconcile_buffers(
    state: &DesktopState,
    controller: &mut RecordingController,
    buffers: &mut HashMap<CameraId, BufferRuntime>,
    desired_cameras: &HashSet<CameraId>,
) {
    let known = buffers.keys().cloned().collect::<Vec<_>>();
    for camera_id in known {
        let status = match controller.status(&camera_id) {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer status unavailable");
                continue;
            }
        };
        if !desired_cameras.contains(&camera_id) {
            if status.state.is_active() && status.state != RecordingState::Stopping {
                if let Err(error) = controller.stop(&camera_id)
                    && !matches!(error, RecordingControllerError::NotRecording)
                {
                    tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer stop failed");
                }
            } else if !status.state.is_active()
                && let Some(runtime) = buffers.remove(&camera_id)
                && let Err(error) = cleanup_event_buffer_camera(&runtime, &camera_id)
            {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer cleanup failed");
            }
        }
    }

    for camera_id in desired_cameras {
        let now = Instant::now();
        if let Some(runtime) = buffers.get(camera_id) {
            let status = match controller.status(camera_id) {
                Ok(status) => status,
                Err(error) => {
                    tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer status unavailable");
                    continue;
                }
            };
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
            let status = match controller.status(camera_id) {
                Ok(status) => status,
                Err(error) => {
                    tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer status unavailable during storage switch");
                    continue;
                }
            };
            if status.state.is_active() {
                if status.state != RecordingState::Stopping {
                    let _ = controller.stop(camera_id);
                }
                continue;
            }
            if let Some(runtime) = buffers.remove(camera_id)
                && let Err(error) = cleanup_event_buffer_camera(&runtime, camera_id)
            {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "old event buffer cleanup failed during storage switch");
            }
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
    active_episodes: &mut HashMap<CameraId, ActiveEpisode>,
    pending_episodes: &mut Vec<(CameraId, ActiveEpisode)>,
    aggregate_motion: &HashMap<CameraId, bool>,
) {
    let now_utc = Utc::now();
    let now = Instant::now();
    let max =
        TimeDelta::from_std(EVENT_CLIP_MAX_DURATION).unwrap_or_else(|_| TimeDelta::minutes(5));
    let cameras = active_episodes.keys().cloned().collect::<Vec<_>>();

    for camera_id in cameras {
        let Some(episode) = active_episodes.get(&camera_id) else {
            continue;
        };
        let reached_cap = now_utc.signed_duration_since(episode.trigger_utc) >= max;
        let motion_active = aggregate_motion.get(&camera_id).copied().unwrap_or(false);
        if !reached_cap && motion_active {
            continue;
        }

        let Some(mut episode) = active_episodes.remove(&camera_id) else {
            continue;
        };
        if reached_cap {
            episode.ended_utc = episode.trigger_utc.checked_add_signed(max);
            episode.finalize_after = Some(now);
            let continuation = motion_active.then(|| continuation_episode(&episode));
            pending_episodes.push((camera_id.clone(), episode));
            if let Some(continuation) = continuation {
                active_episodes.insert(camera_id, continuation);
            }
        } else {
            episode.ended_utc = Some(now_utc);
            episode.finalize_after = Some(now);
            pending_episodes.push((camera_id, episode));
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
    pending_episodes: &mut Vec<(CameraId, ActiveEpisode)>,
) {
    let now = Instant::now();
    let mut index = 0;
    while index < pending_episodes.len() {
        let ready = pending_episodes[index]
            .1
            .finalize_after
            .is_some_and(|deadline| now >= deadline);
        if !ready {
            index += 1;
            continue;
        }

        let (camera_id, episode) = pending_episodes[index].clone();
        let Some(runtime) = buffers.get(&camera_id) else {
            index += 1;
            continue;
        };
        let buffer_active = controller
            .status(&camera_id)
            .map(|status| status.state.is_active())
            .unwrap_or(false);
        match materialize_episode(runtime, &camera_id, &episode, composer, buffer_active) {
            Ok(true) => {
                pending_episodes.swap_remove(index);
            }
            Ok(false) => {
                index += 1;
            }
            Err(error) => {
                tracing::warn!(
                    camera_id = %camera_id.as_str(),
                    error = %error,
                    "event clip materialization failed"
                );
                index += 1;
            }
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
    pub(crate) root_event_id: u64,
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
        let Some(root_event_id) = manifest.event_ids.first().copied() else {
            continue;
        };
        clips.push(EventClipRef {
            episode_id: manifest.episode_id,
            root_event_id,
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

pub(crate) fn event_clip_usage(storage_root: &Path) -> io::Result<(u64, usize)> {
    let candidates = inventory_event_clip_candidates(storage_root)?;
    let bytes = candidates.iter().fold(0_u64, |total, candidate| {
        total.saturating_add(candidate.size_bytes)
    });
    Ok((bytes, candidates.len()))
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

fn cleanup_event_buffer_camera(runtime: &BufferRuntime, camera_id: &CameraId) -> io::Result<usize> {
    let layout = RecordingsLayout::new(runtime.buffer_root.clone()).map_err(io::Error::other)?;
    let inventory = inventory_recordings(&layout).map_err(io::Error::other)?;
    let mut removed = 0_usize;
    for path in inventory
        .recordings
        .into_iter()
        .filter(|recording| &recording.camera_id == camera_id)
        .map(|recording| recording.path)
        .chain(
            inventory
                .partials
                .into_iter()
                .filter(|partial| &partial.camera_id == camera_id)
                .map(|partial| partial.path),
        )
    {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

fn cleanup_event_buffers(buffers: &HashMap<CameraId, BufferRuntime>) {
    for (camera_id, runtime) in buffers {
        match cleanup_event_buffer_camera(runtime, camera_id) {
            Ok(removed) if removed > 0 => {
                tracing::debug!(camera_id = %camera_id.as_str(), removed, "event buffer temporary media cleaned");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(camera_id = %camera_id.as_str(), ?error, "event buffer cleanup failed");
            }
        }
    }
}

fn prune_buffers(
    buffers: &HashMap<CameraId, BufferRuntime>,
    active_episodes: &HashMap<CameraId, ActiveEpisode>,
    pending_episodes: &[(CameraId, ActiveEpisode)],
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
        let active_keep_from = active_episodes.get(camera_id).map(|episode| {
            episode
                .trigger_utc
                .checked_sub_signed(pre)
                .unwrap_or(episode.trigger_utc)
                .with_timezone(&Local)
                .naive_local()
        });
        let pending_keep_from = pending_episodes
            .iter()
            .filter(|(pending_camera_id, _)| pending_camera_id == camera_id)
            .map(|(_, episode)| {
                episode
                    .trigger_utc
                    .checked_sub_signed(pre)
                    .unwrap_or(episode.trigger_utc)
                    .with_timezone(&Local)
                    .naive_local()
            })
            .min();
        let keep_from = active_keep_from
            .into_iter()
            .chain(pending_keep_from)
            .min()
            .unwrap_or(idle_cutoff)
            .min(idle_cutoff);
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
    fn local_motion_requires_archival_main_preroll_even_with_a_healthy_substream() {
        let front = CameraId::parse("cam-front").unwrap();
        let side = CameraId::parse("cam-side").unwrap();
        let desired = HashSet::from([front.clone(), side.clone()]);

        let (admitted, unavailable) = local_motion_admission(&desired, &HashSet::new());
        assert!(
            admitted.is_empty(),
            "substream alone must not authorize detection"
        );
        assert_eq!(unavailable, desired);

        let (admitted, unavailable) =
            local_motion_admission(&desired, &HashSet::from([front.clone()]));
        assert_eq!(admitted, HashSet::from([front.clone()]));
        assert_eq!(unavailable, HashSet::from([side.clone()]));

        let (admitted, unavailable) =
            local_motion_admission(&desired, &HashSet::from([front, side]));
        assert_eq!(admitted, desired);
        assert!(unavailable.is_empty());
    }

    #[test]
    fn replacement_local_motion_lease_resumes_at_first_unacked_transition() {
        // The previous lease persisted/ACKed sequences 1..=40. A subsequent
        // status reports the outstanding 41/42 pair, not the already ACKed
        // history. Never treat that legitimate prefix as an unrecoverable gap.
        assert_eq!(local_motion_resume_cursor(0, Some(41)), Some(40));
        assert_eq!(local_motion_resume_cursor(40, Some(41)), Some(40));
        assert_eq!(local_motion_resume_cursor(0, None), Some(0));
        assert_eq!(local_motion_resume_cursor(0, Some(0)), None);

        // Once an owner has a cursor, a missing transition cannot silently
        // become a new baseline. The reconciliation sequence check rejects it.
        let cursor = local_motion_resume_cursor(40, Some(42)).unwrap();
        assert_ne!(42, cursor.saturating_add(1));
    }

    #[test]
    fn lost_motion_ack_reply_keeps_the_durable_cursor_and_retries_without_duplicate_events() {
        // After persisting 11 and 12, an ACK for 12 may succeed at the worker
        // but its response can be lost. The desktop has confirmed ACK 10 yet
        // must remember that 11 and 12 are already durable.
        let acknowledged = 10;
        let persisted = 12;
        // ACK was not applied: the worker still offers 11/12 for replay.
        assert!(!local_motion_cursor_regressed(acknowledged, Some(11)));
        let cursor = local_motion_resume_cursor(persisted, Some(11)).unwrap();
        assert_eq!(cursor, 12);
        assert!(12 <= cursor, "already-committed rows are skipped");
        // ACK was applied but the response vanished: only sequence 13 remains.
        assert!(!local_motion_cursor_regressed(acknowledged, Some(13)));
        let cursor = local_motion_resume_cursor(persisted, Some(13)).unwrap();
        assert_eq!(13, cursor + 1, "the next real event is never a false gap");
        // A genuinely missing transition after the durable cursor is an error.
        assert_ne!(14, cursor + 1);
        // An empty queue still permits an idempotent retry of ACK 12.
        assert_eq!(local_motion_resume_cursor(persisted, None), Some(12));
    }

    #[test]
    fn dead_motion_worker_reacquires_but_a_transient_status_timeout_does_not_kill_shared_media() {
        assert!(local_motion_requires_new_lease(
            &CameraWorkerError::Unavailable
        ));
        assert!(local_motion_requires_new_lease(
            &CameraWorkerError::Protocol
        ));
        assert!(!local_motion_requires_new_lease(
            &CameraWorkerError::Timeout
        ));
        assert!(!local_motion_requires_new_lease(
            &CameraWorkerError::Synchronization
        ));
        assert!(!local_motion_requires_new_lease(&CameraWorkerError::Rpc(
            "motion_busy".to_owned(),
        )));
    }

    #[test]
    fn worker_sequence_epoch_regression_cannot_silently_discard_new_motion_events() {
        assert!(!local_motion_cursor_regressed(0, Some(1)));
        assert!(!local_motion_cursor_regressed(0, Some(42)));
        assert!(!local_motion_cursor_regressed(42, None));
        assert!(!local_motion_cursor_regressed(42, Some(43)));
        assert!(local_motion_cursor_regressed(42, Some(1)));
        assert!(local_motion_cursor_regressed(42, Some(42)));
        assert!(local_motion_cursor_regressed(42, Some(0)));
    }

    #[test]
    fn camera_person_events_link_to_one_motion_episode_without_creating_recorders() {
        let at = Utc::now();
        let camera = CameraId::parse("cam-front").unwrap();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();
        let now = Instant::now();
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(1, true, at),
            now,
        );
        let mut person_start = signal(2, true, at + TimeDelta::seconds(1));
        person_start.kind = nian_application::EventHistoryKind::PersonStarted;
        person_start.motion_active = None;
        apply_signal(&mut active, &mut pending, &mut aggregate, person_start, now);
        assert_eq!(
            active.len(),
            1,
            "person detection cannot start another recorder"
        );
        assert_eq!(active[&camera].event_ids, vec![1, 2]);
        assert_eq!(aggregate.get(&camera), Some(&true));
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(3, false, at + TimeDelta::seconds(2)),
            now,
        );
        let mut person_end = signal(4, false, at + TimeDelta::seconds(2));
        person_end.kind = nian_application::EventHistoryKind::PersonEnded;
        person_end.motion_active = None;
        apply_signal(&mut active, &mut pending, &mut aggregate, person_end, now);
        assert!(active.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.event_ids, vec![1, 2, 3, 4]);
        let mut stale = signal(5, true, at - TimeDelta::minutes(1));
        stale.kind = nian_application::EventHistoryKind::PersonStarted;
        stale.motion_active = None;
        apply_signal(&mut active, &mut pending, &mut aggregate, stale, now);
        assert_eq!(pending[0].1.event_ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_new_motion_burst_does_not_merge_into_the_previous_post_roll() {
        let camera = CameraId::parse("cam-front").unwrap();
        let at = Utc::now();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(1, true, at),
            Instant::now(),
        );
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(2, false, at + TimeDelta::seconds(1)),
            Instant::now(),
        );
        assert!(!active.contains_key(&camera));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.event_ids, vec![1, 2]);
        assert!(pending[0].1.finalize_after.is_some());

        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(3, true, at + TimeDelta::seconds(2)),
            Instant::now(),
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.event_ids, vec![1, 2]);
        assert_eq!(active[&camera].event_ids, vec![3]);
    }

    #[test]
    fn nearby_motion_bursts_keep_independent_clip_windows() {
        let at = Utc::now();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();

        for (event_id, active_state, offset) in
            [(1, true, 0), (2, false, 1), (3, true, 2), (4, false, 3)]
        {
            apply_signal(
                &mut active,
                &mut pending,
                &mut aggregate,
                signal(event_id, active_state, at + TimeDelta::seconds(offset)),
                Instant::now(),
            );
        }

        assert!(active.is_empty());
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].1.event_ids, vec![1, 2]);
        assert_eq!(pending[1].1.event_ids, vec![3, 4]);
        let first_window = pending[0].1.requested_window().unwrap();
        let second_window = pending[1].1.requested_window().unwrap();
        assert_eq!(first_window.0, at - TimeDelta::seconds(5));
        assert_eq!(first_window.1, at + TimeDelta::seconds(6));
        assert_eq!(second_window.0, at - TimeDelta::seconds(3));
        assert_eq!(second_window.1, at + TimeDelta::seconds(8));
        assert!(
            first_window.1 > second_window.0,
            "event clips may overlap during pre/post-roll"
        );
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
    fn disabling_events_during_motion_settles_once_with_bounded_post_roll() {
        let camera = CameraId::parse("cam-front").unwrap();
        let at = Utc::now();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(1, true, at),
            Instant::now(),
        );
        assert_eq!(aggregate.get(&camera), Some(&true));

        let settle_at = Instant::now();
        settle_unmonitorable_episodes(
            &mut active,
            &mut pending,
            &mut aggregate,
            &HashSet::new(),
            &HashSet::new(),
            at + TimeDelta::seconds(2),
            settle_at,
        );

        assert!(active.is_empty());
        assert!(!aggregate.contains_key(&camera));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, camera);
        assert_eq!(pending[0].1.ended_utc, Some(at + TimeDelta::seconds(2)));
        assert_eq!(
            pending[0].1.finalize_after,
            Some(settle_at + EVENT_POST_ROLL)
        );

        reconcile_episode_deadlines(&mut active, &mut pending, &aggregate);
        assert!(
            active.is_empty(),
            "Desired Off must never create a continuation"
        );
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn monitoring_loss_during_motion_settles_without_five_minute_continuations() {
        let camera = CameraId::parse("cam-front").unwrap();
        let at = Utc::now();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(1, true, at),
            Instant::now(),
        );

        settle_unmonitorable_episodes(
            &mut active,
            &mut pending,
            &mut aggregate,
            &HashSet::from([camera.clone()]),
            &HashSet::from([camera.clone()]),
            at + TimeDelta::seconds(3),
            Instant::now(),
        );

        assert!(active.is_empty());
        assert!(!aggregate.contains_key(&camera));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, camera);
    }

    #[test]
    fn healthy_desired_monitoring_does_not_settle_an_active_episode() {
        let camera = CameraId::parse("cam-front").unwrap();
        let at = Utc::now();
        let mut active = HashMap::new();
        let mut pending = Vec::new();
        let mut aggregate = HashMap::new();
        apply_signal(
            &mut active,
            &mut pending,
            &mut aggregate,
            signal(1, true, at),
            Instant::now(),
        );

        settle_unmonitorable_episodes(
            &mut active,
            &mut pending,
            &mut aggregate,
            &HashSet::from([camera.clone()]),
            &HashSet::new(),
            at + TimeDelta::seconds(1),
            Instant::now(),
        );

        assert!(active.contains_key(&camera));
        assert!(pending.is_empty());
        assert_eq!(aggregate.get(&camera), Some(&true));
    }

    #[test]
    fn idle_event_buffer_cleanup_removes_owned_temp_media_without_touching_foreign_or_other_camera_files()
     {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        std::fs::create_dir_all(&root).unwrap();
        let buffer_root = ensure_event_subdir(&root, "event-buffer").unwrap();
        let camera = CameraId::parse("cam-front").unwrap();
        let other_camera = CameraId::parse("cam-side").unwrap();
        let camera_day = buffer_root
            .join(camera.as_str())
            .join("2026")
            .join("09")
            .join("16");
        let other_day = buffer_root
            .join(other_camera.as_str())
            .join("2026")
            .join("09")
            .join("16");
        std::fs::create_dir_all(&camera_day).unwrap();
        std::fs::create_dir_all(&other_day).unwrap();
        let finalized = camera_day.join("09-00-00.mkv");
        let partial = camera_day.join("09-00-02.partial.mkv");
        let foreign = camera_day.join("notes.txt");
        let other = other_day.join("09-00-00.mkv");
        std::fs::write(&finalized, b"temporary-event-media").unwrap();
        std::fs::write(&partial, b"partial-event-media").unwrap();
        std::fs::write(&foreign, b"foreign-evidence").unwrap();
        std::fs::write(&other, b"other-camera-media").unwrap();

        let runtime = BufferRuntime {
            manual_storage_root: root,
            buffer_root,
            next_start_attempt: Instant::now(),
        };
        assert_eq!(cleanup_event_buffer_camera(&runtime, &camera).unwrap(), 2);
        assert!(!finalized.exists());
        assert!(!partial.exists());
        assert!(foreign.exists());
        assert!(other.exists());
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
