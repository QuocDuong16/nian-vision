//! Optional local video-motion consumer. A single worker-owned job subscribes
//! to the existing main/sub ingest; it never opens an RTSP socket itself.
//! Status exposes only normalized transitions, not URLs, images or credentials.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Utc;
use nian_media_ffmpeg::{
    InterruptHandle, LumaDecoder, PacketConsumerKind, PacketDeliveryPolicy, PacketQueueLimits,
};
use serde::Serialize;

use crate::ingest::{IngestProfile, PacketSubscriptionOptions, SharedIngestManager};
use crate::motion::MotionDetector;

const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(20);
const PACKET_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RECONNECTS: usize = 5;
const MAX_TRANSITIONS: usize = 256;

pub(crate) struct MotionSpec {
    source_url: String,
    profile: IngestProfile,
}

impl MotionSpec {
    pub fn from_params(params: &serde_json::Value) -> Result<Self, &'static str> {
        let source = params.get("source").ok_or("missing source")?;
        if source.get("kind").and_then(serde_json::Value::as_str) != Some("rtsp") {
            return Err("motion requires an rtsp source");
        }
        let source_url = source
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing source.url")?;
        if !(source_url.starts_with("rtsp://") || source_url.starts_with("rtsps://")) {
            return Err("invalid rtsp source");
        }
        let profile = match source.get("profile").and_then(serde_json::Value::as_str) {
            Some("main") => IngestProfile::Main,
            Some("sub") => IngestProfile::Sub,
            _ => return Err("motion source profile must be main or sub"),
        };
        Ok(Self {
            source_url: source_url.to_owned(),
            profile,
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct MotionTransition {
    pub sequence: u64,
    pub motion_active: bool,
    pub observed_at_utc: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MotionStatus {
    state: &'static str,
    motion_active: Option<bool>,
    sampled_frames: u64,
    dropped_packets: u64,
    reconnect_attempt: usize,
    last_error_code: Option<&'static str>,
    /// Replay until explicitly acknowledged in a later consumer phase. Never
    /// silently erase a transition before durable event-index persistence.
    transitions: VecDeque<MotionTransition>,
    next_sequence: u64,
}

impl Default for MotionStatus {
    fn default() -> Self {
        Self {
            state: "disabled",
            motion_active: None,
            sampled_frames: 0,
            dropped_packets: 0,
            reconnect_attempt: 0,
            last_error_code: None,
            transitions: VecDeque::new(),
            next_sequence: 1,
        }
    }
}

impl MotionStatus {
    fn transition(&mut self, motion_active: bool) -> bool {
        if self.transitions.len() >= MAX_TRANSITIONS || self.next_sequence == u64::MAX {
            self.state = "failed";
            self.motion_active = None;
            self.last_error_code = Some("motion_transition_capacity");
            return false; // Missing history is a failure, not silently dropped evidence.
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.motion_active = Some(motion_active);
        self.transitions.push_back(MotionTransition {
            sequence,
            motion_active,
            observed_at_utc: Utc::now().to_rfc3339(),
        });
        true
    }

    fn acknowledge(&mut self, sequence: u64) -> Result<usize, &'static str> {
        if sequence == 0 || sequence >= self.next_sequence {
            return Err("invalid_motion_ack");
        }
        let mut removed = 0;
        while self
            .transitions
            .front()
            .is_some_and(|event| event.sequence <= sequence)
        {
            self.transitions.pop_front();
            removed += 1;
        }
        Ok(removed)
    }
}

pub(crate) struct MotionJobManager {
    ingests: SharedIngestManager,
    status: Arc<Mutex<MotionStatus>>,
    stop: Arc<AtomicBool>,
    interrupt: InterruptHandle,
    handle: Option<JoinHandle<()>>,
}

impl MotionJobManager {
    pub fn with_ingest_manager(ingests: SharedIngestManager) -> Self {
        Self {
            ingests,
            status: Arc::new(Mutex::new(MotionStatus::default())),
            stop: Arc::new(AtomicBool::new(false)),
            interrupt: InterruptHandle::new(),
            handle: None,
        }
    }

    pub fn start(&mut self, spec: MotionSpec) -> Result<(), &'static str> {
        self.reap_finished();
        if self.handle.is_some() {
            return Err("motion_busy");
        }
        if !self
            .status
            .lock()
            .map_err(|_| "motion_worker_unavailable")?
            .transitions
            .is_empty()
        {
            // Never discard transitions before the durable consumer has
            // explicitly acknowledged them, even during a restart.
            return Err("motion_unacknowledged_transitions");
        }
        self.stop.store(false, Ordering::Release);
        self.interrupt = InterruptHandle::new();
        if let Ok(mut status) = self.status.lock() {
            *status = MotionStatus {
                state: "connecting",
                ..MotionStatus::default()
            };
        }
        let ingests = self.ingests.clone();
        let status = self.status.clone();
        let stop = self.stop.clone();
        let interrupt = self.interrupt.clone();
        match std::thread::Builder::new()
            .name("local-video-motion".to_owned())
            .spawn(move || run_motion(spec, ingests, status, stop, interrupt))
        {
            Ok(handle) => self.handle = Some(handle),
            Err(_) => return Err("motion_worker_unavailable"),
        }
        Ok(())
    }

    pub fn status_json(&mut self) -> serde_json::Value {
        self.reap_finished();
        self.status.lock().ok().and_then(|status| serde_json::to_value(&*status).ok())
            .unwrap_or_else(|| serde_json::json!({"state":"failed","last_error_code":"motion_worker_unavailable"}))
    }

    /// Acknowledge only after the event index has durably committed these rows.
    pub fn acknowledge(&self, sequence: u64) -> Result<usize, &'static str> {
        self.status
            .lock()
            .map_err(|_| "motion_worker_unavailable")?
            .acknowledge(sequence)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.interrupt.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if let Ok(mut status) = self.status.lock() {
            status.state = "disabled";
            status.motion_active = None;
        }
    }

    fn reap_finished(&mut self) {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for MotionJobManager {
    fn drop(&mut self) {
        self.stop();
    }
}

fn queue_limits() -> PacketQueueLimits {
    PacketQueueLimits::new(
        NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::new(4 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
        Duration::from_secs(2),
    )
}

fn run_motion(
    spec: MotionSpec,
    ingests: SharedIngestManager,
    status: Arc<Mutex<MotionStatus>>,
    stop: Arc<AtomicBool>,
    interrupt: InterruptHandle,
) {
    for attempt in 0..=MAX_RECONNECTS {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut status) = status.lock() {
            status.state = "connecting";
            status.motion_active = None;
            status.reconnect_attempt = attempt;
        }
        let subscription = ingests.subscribe_rtsp_with_options(
            &spec.source_url,
            PacketSubscriptionOptions {
                profile: spec.profile,
                limits: queue_limits(),
                policy: PacketDeliveryPolicy::Realtime,
                consumer: PacketConsumerKind::Motion,
                pre_roll: Duration::ZERO,
                ready_timeout: SUBSCRIBE_TIMEOUT,
            },
            Some(&interrupt),
        );
        let subscription = match subscription {
            Ok(value) => value,
            Err(error) if error.is_interrupted() || stop.load(Ordering::Acquire) => return,
            Err(_) => {
                if !backoff(attempt, &status, &stop) {
                    return;
                }
                continue;
            }
        };
        let mut decoder = match LumaDecoder::open(subscription.stream_template()) {
            Ok(value) => value,
            Err(_) => {
                if let Ok(mut status) = status.lock() {
                    status.state = "failed";
                    status.last_error_code = Some("motion_unsupported_video");
                }
                return;
            }
        };
        let mut detector = MotionDetector::new();
        let mut previous_drops = subscription.dropped_packets();
        let mut last_packet = Instant::now();
        if let Ok(mut status) = status.lock() {
            status.state = "monitoring";
            status.motion_active = Some(false);
            status.last_error_code = None;
        }
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match subscription.receive_interruptible(PACKET_TIMEOUT, &interrupt) {
                Ok(Some(packet)) => {
                    last_packet = Instant::now();
                    let drops = subscription.dropped_packets();
                    if drops != previous_drops {
                        let newly_dropped = drops.saturating_sub(previous_drops);
                        previous_drops = drops;
                        if let Ok(mut state) = status.lock() {
                            state.dropped_packets =
                                state.dropped_packets.saturating_add(newly_dropped);
                            state.motion_active = None;
                        }
                        // A realtime overrun discards dependent packets. Rebuild
                        // decoder/detector from the next independent keyframe.
                        decoder = match LumaDecoder::open(subscription.stream_template()) {
                            Ok(value) => value,
                            Err(_) => break,
                        };
                        detector = MotionDetector::new();
                        if !packet.metadata().keyframe {
                            continue;
                        }
                    }
                    let thumbnails = match decoder.push(&packet) {
                        Ok(value) => value,
                        Err(_) => break,
                    };
                    for thumbnail in thumbnails {
                        if let Ok(mut state) = status.lock() {
                            state.sampled_frames = state.sampled_frames.saturating_add(1);
                            if let Some(motion_active) = detector.observe(thumbnail) {
                                if !state.transition(motion_active) {
                                    return;
                                }
                            } else if state.motion_active.is_none() {
                                state.motion_active = Some(detector.is_active());
                            }
                        } else {
                            return;
                        }
                    }
                }
                Ok(None) => break,
                Err(error) if error.is_interrupted() || stop.load(Ordering::Acquire) => return,
                Err(error)
                    if error.is_timed_out() && last_packet.elapsed() < Duration::from_secs(15) => {}
                Err(_) => break,
            }
        }
        if !backoff(attempt, &status, &stop) {
            return;
        }
    }
    if let Ok(mut status) = status.lock() {
        status.state = "failed";
        status.motion_active = None;
        status.last_error_code = Some("motion_reconnect_exhausted");
    }
}

fn backoff(attempt: usize, status: &Mutex<MotionStatus>, stop: &AtomicBool) -> bool {
    if let Ok(mut status) = status.lock() {
        status.state = "backoff";
        status.motion_active = None;
        status.last_error_code = Some("motion_source_unavailable");
    }
    let delay = Duration::from_millis(250 * 2_u64.pow(attempt.min(4) as u32));
    let until = Instant::now() + delay;
    while !stop.load(Ordering::Acquire) && Instant::now() < until {
        std::thread::sleep(
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(25)),
        );
    }
    !stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn invalid_motion_profile_refuses_before_any_camera_connection() {
        let invalid = serde_json::json!({"source":{"kind":"rtsp","url":"rtsp://secret:password@camera/stream", "profile":"invalid"}});
        assert!(MotionSpec::from_params(&invalid).is_err());
        let valid = serde_json::json!({"source":{"kind":"rtsp","url":"rtsp://secret:password@camera/stream", "profile":"sub"}});
        let spec = MotionSpec::from_params(&valid).unwrap();
        assert_eq!(spec.profile, IngestProfile::Sub);
        assert!(
            !format!(
                "{:?}",
                MotionJobManager::with_ingest_manager(SharedIngestManager::new()).status_json()
            )
            .contains("password")
        );
    }

    #[test]
    fn acknowledgement_never_removes_future_or_uncommitted_transitions() {
        let mut status = MotionStatus::default();
        assert!(status.transition(true));
        assert!(status.transition(false));
        assert_eq!(status.acknowledge(0), Err("invalid_motion_ack"));
        assert_eq!(status.acknowledge(3), Err("invalid_motion_ack"));
        assert_eq!(status.transitions.len(), 2);
        assert_eq!(status.acknowledge(1), Ok(1));
        assert_eq!(
            status.transitions.front().map(|event| event.sequence),
            Some(2)
        );
        assert_eq!(status.acknowledge(1), Ok(0));
        assert_eq!(status.acknowledge(2), Ok(1));
        assert!(status.transitions.is_empty());
    }

    #[test]
    fn transitions_are_bounded_and_never_silently_discarded() {
        let mut status = MotionStatus::default();
        for index in 0..MAX_TRANSITIONS {
            assert!(status.transition(index % 2 == 0));
        }
        assert!(!status.transition(true));
        assert_eq!(status.state, "failed");
        assert_eq!(status.transitions.len(), MAX_TRANSITIONS);
        assert_eq!(status.last_error_code, Some("motion_transition_capacity"));
    }
}
