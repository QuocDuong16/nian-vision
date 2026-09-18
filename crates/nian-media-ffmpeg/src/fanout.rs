//! Bounded compressed-packet fan-out for shared camera ingests.
//!
//! The RTSP owner must never block on a slow consumer. Each subscriber owns a
//! bounded queue with an explicit delivery policy:
//!
//! * reliable subscribers (recording) fail closed on overrun rather than
//!   dropping evidence or stalling the camera ingest;
//! * realtime subscribers (grid/focus live view) discard stale backlog and
//!   wait for the next primary-video keyframe before resuming.
//!
//! Packet payloads are reference-cloned through FFmpeg, so subscriber count
//! does not multiply compressed-frame payload memory.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use nian_media::MediaError;

use crate::{FfmpegPacket, InterruptHandle, MediaStreamTemplate};

/// Capacity limits for one subscriber queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketQueueLimits {
    max_packets: NonZeroUsize,
    max_bytes: NonZeroUsize,
    max_age: Duration,
}

impl PacketQueueLimits {
    /// Creates packet, byte and wall-clock backlog caps.
    pub const fn new(
        max_packets: NonZeroUsize,
        max_bytes: NonZeroUsize,
        max_age: Duration,
    ) -> Self {
        Self {
            max_packets,
            max_bytes,
            max_age,
        }
    }

    /// Maximum number of queued compressed packets.
    pub const fn max_packets(self) -> usize {
        self.max_packets.get()
    }

    /// Maximum aggregate compressed payload bytes.
    pub const fn max_bytes(self) -> usize {
        self.max_bytes.get()
    }

    /// Maximum wall-clock age of the oldest queued packet before the consumer
    /// is considered too far behind the realtime source.
    pub const fn max_age(self) -> Duration {
        self.max_age
    }
}

/// Overflow semantics for a subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketDeliveryPolicy {
    /// Never drop packets. Queue overflow terminates this subscriber with an
    /// explicit backpressure error while the source ingest keeps running.
    Reliable,
    /// Prefer the newest realtime media. Overflow discards the stale queue and
    /// resumes only at the next keyframe for the specified primary video
    /// stream, so downstream muxers never restart on a dependent frame.
    Realtime,
}

/// Non-sensitive subscriber purpose for per-camera media diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketConsumerKind {
    Recording,
    Live,
    Motion,
    Event,
    /// Compatibility for generic callers; production RTSP jobs label themselves.
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PacketConsumerCounts {
    pub recording: usize,
    pub live: usize,
    pub motion: usize,
    pub event: usize,
    pub other: usize,
}

impl PacketConsumerCounts {
    fn register(&mut self, consumer: PacketConsumerKind) {
        let count = match consumer {
            PacketConsumerKind::Recording => &mut self.recording,
            PacketConsumerKind::Live => &mut self.live,
            PacketConsumerKind::Motion => &mut self.motion,
            PacketConsumerKind::Event => &mut self.event,
            PacketConsumerKind::Other => &mut self.other,
        };
        *count = count.saturating_add(1);
    }

    pub fn merge(&mut self, other: Self) {
        self.recording = self.recording.saturating_add(other.recording);
        self.live = self.live.saturating_add(other.live);
        self.motion = self.motion.saturating_add(other.motion);
        self.event = self.event.saturating_add(other.event);
        self.other = self.other.saturating_add(other.other);
    }

    pub fn total(self) -> usize {
        self.recording
            .saturating_add(self.live)
            .saturating_add(self.motion)
            .saturating_add(self.event)
            .saturating_add(self.other)
    }
}

/// Per-publish accounting used by ingest telemetry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PacketFanoutReport {
    /// Subscribers that accepted this packet.
    pub delivered: usize,
    /// Subscribers that deliberately dropped this packet for realtime resync.
    pub dropped: usize,
    /// Subscribers terminated because their reliable queue overran or packet
    /// reference cloning failed.
    pub failed: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PacketFanoutSnapshot {
    pub subscribers: usize,
    pub reliable_subscribers: usize,
    pub realtime_subscribers: usize,
    pub consumers: PacketConsumerCounts,
    pub queued_packets: usize,
    pub queued_bytes: usize,
    pub dropped_packets: u64,
}

#[derive(Debug, Clone)]
enum QueueTerminal {
    Eof,
    Error(MediaError),
}

struct QueueState {
    packets: VecDeque<QueuedPacket>,
    payload_bytes: usize,
    terminal: Option<QueueTerminal>,
    awaiting_keyframe: bool,
    dropped_packets: u64,
}

struct QueuedPacket {
    packet: FfmpegPacket,
    queued_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePolicy {
    Reliable,
    Realtime { primary_video_stream: u32 },
}

struct PacketQueue {
    limits: PacketQueueLimits,
    policy: QueuePolicy,
    consumer: PacketConsumerKind,
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushOutcome {
    Delivered,
    Dropped,
    Failed,
}

impl PacketQueue {
    fn new(limits: PacketQueueLimits, policy: QueuePolicy, consumer: PacketConsumerKind) -> Self {
        Self {
            limits,
            policy,
            consumer,
            state: Mutex::new(QueueState {
                packets: VecDeque::new(),
                payload_bytes: 0,
                terminal: None,
                awaiting_keyframe: false,
                dropped_packets: 0,
            }),
            ready: Condvar::new(),
        }
    }

    fn push(&self, packet: &FfmpegPacket) -> PushOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.terminal.is_some() {
            return PushOutcome::Failed;
        }

        let packet_bytes = packet.data().len();
        let metadata = packet.metadata();
        let stale_backlog = state
            .packets
            .front()
            .is_some_and(|queued| queued.queued_at.elapsed() >= self.limits.max_age());
        match self.policy {
            QueuePolicy::Reliable => {
                if stale_backlog
                    || packet_bytes > self.limits.max_bytes()
                    || state.packets.len() >= self.limits.max_packets()
                    || state.payload_bytes.saturating_add(packet_bytes) > self.limits.max_bytes()
                {
                    state.terminal = Some(QueueTerminal::Error(MediaError::ReadFailed {
                        message: if stale_backlog {
                            "shared ingest recording queue exceeded its bounded time capacity"
                                .to_owned()
                        } else {
                            "shared ingest recording queue exceeded its bounded capacity".to_owned()
                        },
                    }));
                    self.ready.notify_all();
                    return PushOutcome::Failed;
                }
            }
            QueuePolicy::Realtime {
                primary_video_stream,
            } => {
                if stale_backlog {
                    let discarded = u64::try_from(state.packets.len()).unwrap_or(u64::MAX);
                    state.dropped_packets = state.dropped_packets.saturating_add(discarded);
                    state.packets.clear();
                    state.payload_bytes = 0;
                    state.awaiting_keyframe = true;
                }
                if state.awaiting_keyframe {
                    if metadata.stream_index != primary_video_stream || !metadata.keyframe {
                        state.dropped_packets = state.dropped_packets.saturating_add(1);
                        return PushOutcome::Dropped;
                    }
                    state.awaiting_keyframe = false;
                }

                if packet_bytes > self.limits.max_bytes()
                    || state.packets.len() >= self.limits.max_packets()
                    || state.payload_bytes.saturating_add(packet_bytes) > self.limits.max_bytes()
                {
                    let discarded = u64::try_from(state.packets.len()).unwrap_or(u64::MAX);
                    state.dropped_packets = state.dropped_packets.saturating_add(discarded);
                    state.packets.clear();
                    state.payload_bytes = 0;
                    if packet_bytes <= self.limits.max_bytes()
                        && metadata.stream_index == primary_video_stream
                        && metadata.keyframe
                    {
                        state.awaiting_keyframe = false;
                    } else {
                        state.dropped_packets = state.dropped_packets.saturating_add(1);
                        state.awaiting_keyframe = true;
                        return PushOutcome::Dropped;
                    }
                }
            }
        }

        let cloned = match packet.try_clone_ref() {
            Ok(packet) => packet,
            Err(error) => {
                state.terminal = Some(QueueTerminal::Error(error));
                self.ready.notify_all();
                return PushOutcome::Failed;
            }
        };
        state.payload_bytes = state.payload_bytes.saturating_add(cloned.data().len());
        state.packets.push_back(QueuedPacket {
            packet: cloned,
            queued_at: Instant::now(),
        });
        self.ready.notify_one();
        PushOutcome::Delivered
    }

    fn finish(&self, terminal: QueueTerminal) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
            self.ready.notify_all();
        }
    }

    fn receive_timeout(&self, timeout: Duration) -> Result<Option<FfmpegPacket>, MediaError> {
        self.receive_interruptible(timeout, None)
    }

    fn receive_interruptible(
        &self,
        timeout: Duration,
        interrupt: Option<&InterruptHandle>,
    ) -> Result<Option<FfmpegPacket>, MediaError> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if interrupt.is_some_and(InterruptHandle::is_cancelled) {
                return Err(MediaError::Interrupted {
                    operation: "wait for shared ingest packet",
                });
            }
            if let Some(queued) = state.packets.pop_front() {
                state.payload_bytes = state
                    .payload_bytes
                    .saturating_sub(queued.packet.data().len());
                return Ok(Some(queued.packet));
            }
            if let Some(terminal) = &state.terminal {
                return match terminal {
                    QueueTerminal::Eof => Ok(None),
                    QueueTerminal::Error(error) => Err(error.clone()),
                };
            }

            let remaining = deadline
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                return Err(MediaError::TimedOut {
                    operation: "wait for shared ingest packet",
                });
            }
            let wait_slice = if interrupt.is_some() {
                remaining.min(Duration::from_millis(50))
            } else {
                remaining
            };
            let waited = self
                .ready
                .wait_timeout(state, wait_slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = waited.0;
            if waited.1.timed_out()
                && interrupt.is_none()
                && state.packets.is_empty()
                && state.terminal.is_none()
            {
                return Err(MediaError::TimedOut {
                    operation: "wait for shared ingest packet",
                });
            }
        }
    }

    fn dropped_packets(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .dropped_packets
    }
}

/// Consumer handle for one bounded shared-ingest packet queue.
pub struct PacketSubscription {
    template: MediaStreamTemplate,
    queue: Arc<PacketQueue>,
    initial_history_age: Duration,
}

impl PacketSubscription {
    /// Source stream metadata/codec parameters used when opening output muxers.
    pub fn stream_template(&self) -> &MediaStreamTemplate {
        &self.template
    }

    /// Receives one packet, EOF, or a typed source/backpressure error.
    pub fn receive_timeout(&self, timeout: Duration) -> Result<Option<FfmpegPacket>, MediaError> {
        self.queue.receive_timeout(timeout)
    }

    /// Like [`Self::receive_timeout`], but a consumer-local interrupt can abort
    /// the wait without affecting the shared source ingest or other subscribers.
    pub fn receive_interruptible(
        &self,
        timeout: Duration,
        interrupt: &InterruptHandle,
    ) -> Result<Option<FfmpegPacket>, MediaError> {
        self.queue.receive_interruptible(timeout, Some(interrupt))
    }

    /// Number of packets deliberately discarded by realtime overflow policy.
    pub fn dropped_packets(&self) -> u64 {
        self.queue.dropped_packets()
    }

    /// Wall-clock age of the oldest packet atomically seeded into this
    /// subscription before live delivery began.
    pub fn initial_history_age(&self) -> Duration {
        self.initial_history_age
    }
}

impl std::fmt::Debug for PacketSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketSubscription")
            .field("template", &self.template)
            .field("dropped_packets", &self.dropped_packets())
            .finish_non_exhaustive()
    }
}

/// Publisher side of a shared compressed-packet ingest.
#[derive(Default)]
pub struct PacketFanout {
    subscribers: Vec<Weak<PacketQueue>>,
}

impl PacketFanout {
    /// Creates an empty fan-out.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one independent bounded subscriber.
    pub fn subscribe(
        &mut self,
        template: MediaStreamTemplate,
        limits: PacketQueueLimits,
        policy: PacketDeliveryPolicy,
    ) -> Result<PacketSubscription, MediaError> {
        self.subscribe_with_prefill(template, limits, policy, &[], Duration::ZERO)
    }

    /// Registers a subscriber and atomically seeds its queue with an existing
    /// compressed packet history before future fan-out packets can arrive.
    /// Payloads remain reference-counted through FFmpeg; prefill does not deep
    /// copy encoded frame data.
    pub fn subscribe_with_prefill(
        &mut self,
        template: MediaStreamTemplate,
        limits: PacketQueueLimits,
        policy: PacketDeliveryPolicy,
        prefill: &[FfmpegPacket],
        initial_history_age: Duration,
    ) -> Result<PacketSubscription, MediaError> {
        self.subscribe_consumer_with_prefill(
            template,
            limits,
            policy,
            PacketConsumerKind::Other,
            prefill,
            initial_history_age,
        )
    }

    /// Registers a named consumer with an atomic compressed pre-roll handoff.
    pub fn subscribe_consumer_with_prefill(
        &mut self,
        template: MediaStreamTemplate,
        limits: PacketQueueLimits,
        policy: PacketDeliveryPolicy,
        consumer: PacketConsumerKind,
        prefill: &[FfmpegPacket],
        initial_history_age: Duration,
    ) -> Result<PacketSubscription, MediaError> {
        let queue_policy = match policy {
            PacketDeliveryPolicy::Reliable => QueuePolicy::Reliable,
            PacketDeliveryPolicy::Realtime => {
                let primary_video_stream = template
                    .streams()
                    .into_iter()
                    .find(|stream| stream.media_type == nian_domain::MediaType::Video)
                    .ok_or_else(|| MediaError::ReadFailed {
                        message: "realtime shared ingest exposes no video stream".to_owned(),
                    })?
                    .stream_index;
                QueuePolicy::Realtime {
                    primary_video_stream,
                }
            }
        };
        let queue = Arc::new(PacketQueue::new(limits, queue_policy, consumer));
        for packet in prefill {
            if matches!(queue.push(packet), PushOutcome::Failed) {
                return Err(MediaError::ReadFailed {
                    message: "shared ingest pre-roll exceeded subscriber queue capacity".to_owned(),
                });
            }
        }
        self.subscribers.push(Arc::downgrade(&queue));
        Ok(PacketSubscription {
            template,
            queue,
            initial_history_age,
        })
    }

    /// Publishes one compressed packet to every currently-live subscriber.
    pub fn publish(&mut self, packet: &FfmpegPacket) -> PacketFanoutReport {
        let mut report = PacketFanoutReport::default();
        self.subscribers.retain(|subscriber| {
            let Some(queue) = subscriber.upgrade() else {
                return false;
            };
            match queue.push(packet) {
                PushOutcome::Delivered => report.delivered += 1,
                PushOutcome::Dropped => report.dropped += 1,
                PushOutcome::Failed => report.failed += 1,
            }
            true
        });
        report
    }

    pub fn snapshot(&mut self) -> PacketFanoutSnapshot {
        let mut snapshot = PacketFanoutSnapshot::default();
        self.subscribers.retain(|subscriber| {
            let Some(queue) = subscriber.upgrade() else {
                return false;
            };
            let state = queue
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.subscribers += 1;
            snapshot.consumers.register(queue.consumer);
            match queue.policy {
                QueuePolicy::Reliable => snapshot.reliable_subscribers += 1,
                QueuePolicy::Realtime { .. } => snapshot.realtime_subscribers += 1,
            }
            snapshot.queued_packets = snapshot.queued_packets.saturating_add(state.packets.len());
            snapshot.queued_bytes = snapshot.queued_bytes.saturating_add(state.payload_bytes);
            snapshot.dropped_packets = snapshot
                .dropped_packets
                .saturating_add(state.dropped_packets);
            true
        });
        snapshot
    }

    /// Number of currently-live subscriber queues. Dead weak slots are pruned.
    pub fn active_subscribers(&mut self) -> usize {
        self.subscribers
            .retain(|subscriber| subscriber.strong_count() > 0);
        self.subscribers.len()
    }

    /// Signals clean source EOF after all packets already queued are consumed.
    pub fn finish_eof(&mut self) {
        self.finish_all(QueueTerminal::Eof);
    }

    /// Signals a source-side failure after queued healthy packets are drained.
    pub fn finish_error(&mut self, error: MediaError) {
        self.finish_all(QueueTerminal::Error(error));
    }

    fn finish_all(&mut self, terminal: QueueTerminal) {
        self.subscribers.retain(|subscriber| {
            let Some(queue) = subscriber.upgrade() else {
                return false;
            };
            queue.finish(terminal.clone());
            true
        });
    }
}

impl std::fmt::Debug for PacketFanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketFanout")
            .field("subscriber_slots", &self.subscribers.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InterruptHandle, MediaInput};
    use nian_domain::MediaType;
    use nian_media::MediaSource;

    fn fixture_input() -> MediaInput {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.extend(["tests", "fixtures", "sample.mkv"]);
        MediaInput::open(&MediaSource::file(path), &InterruptHandle::new()).unwrap()
    }

    fn limits(max_packets: usize) -> PacketQueueLimits {
        PacketQueueLimits::new(
            NonZeroUsize::new(max_packets).unwrap(),
            NonZeroUsize::new(8 * 1024 * 1024).unwrap(),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn reliable_overflow_drains_healthy_prefix_before_backpressure_error() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let first = input.next_packet().unwrap().unwrap();
        let second = input.next_packet().unwrap().unwrap();
        let first_payload = first.data().to_vec();

        let mut fanout = PacketFanout::new();
        let subscriber = fanout
            .subscribe(template, limits(1), PacketDeliveryPolicy::Reliable)
            .unwrap();

        assert_eq!(
            fanout.publish(&first),
            PacketFanoutReport {
                delivered: 1,
                dropped: 0,
                failed: 0,
            }
        );
        assert_eq!(fanout.publish(&second).failed, 1);

        let drained = subscriber
            .receive_timeout(Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert_eq!(drained.data(), first_payload);
        assert!(matches!(
            subscriber.receive_timeout(Duration::from_millis(50)),
            Err(MediaError::ReadFailed { message }) if message.contains("bounded capacity")
        ));
    }

    #[test]
    fn realtime_overflow_discards_backlog_and_resumes_on_next_video_keyframe() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary_video_stream = template
            .streams()
            .into_iter()
            .find(|stream| stream.media_type == MediaType::Video)
            .unwrap()
            .stream_index;
        let mut packets = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            if packet.metadata().stream_index == primary_video_stream {
                packets.push(packet);
            }
        }

        let first_keyframe = packets
            .iter()
            .position(|packet| packet.metadata().keyframe)
            .unwrap();
        let dependent = packets
            .iter()
            .enumerate()
            .skip(first_keyframe + 1)
            .find_map(|(index, packet)| (!packet.metadata().keyframe).then_some(index))
            .unwrap();
        let next_keyframe = packets
            .iter()
            .enumerate()
            .skip(dependent + 1)
            .find_map(|(index, packet)| packet.metadata().keyframe.then_some(index))
            .unwrap();

        let mut fanout = PacketFanout::new();
        let subscriber = fanout
            .subscribe(template, limits(1), PacketDeliveryPolicy::Realtime)
            .unwrap();

        assert_eq!(fanout.publish(&packets[first_keyframe]).delivered, 1);
        assert_eq!(fanout.publish(&packets[dependent]).dropped, 1);
        assert_eq!(subscriber.dropped_packets(), 2);
        assert_eq!(fanout.publish(&packets[next_keyframe]).delivered, 1);

        let resumed = subscriber
            .receive_timeout(Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert!(resumed.metadata().keyframe);
        assert_eq!(resumed.metadata().stream_index, primary_video_stream);
    }

    #[test]
    fn reliable_backlog_time_limit_fails_closed() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let first = input.next_packet().unwrap().unwrap();
        let second = input.next_packet().unwrap().unwrap();
        let mut fanout = PacketFanout::new();
        let subscriber = fanout
            .subscribe(
                template,
                PacketQueueLimits::new(
                    NonZeroUsize::new(64).unwrap(),
                    NonZeroUsize::new(8 * 1024 * 1024).unwrap(),
                    Duration::ZERO,
                ),
                PacketDeliveryPolicy::Reliable,
            )
            .unwrap();

        assert_eq!(fanout.publish(&first).delivered, 1);
        assert_eq!(fanout.publish(&second).failed, 1);
        assert!(
            subscriber
                .receive_timeout(Duration::from_millis(50))
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            subscriber.receive_timeout(Duration::from_millis(50)),
            Err(MediaError::ReadFailed { message }) if message.contains("time capacity")
        ));
    }

    #[test]
    fn realtime_backlog_time_limit_resyncs_at_next_keyframe() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary_video_stream = template
            .streams()
            .into_iter()
            .find(|stream| stream.media_type == MediaType::Video)
            .unwrap()
            .stream_index;
        let mut packets = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            if packet.metadata().stream_index == primary_video_stream {
                packets.push(packet);
            }
        }
        let first_keyframe = packets
            .iter()
            .position(|packet| packet.metadata().keyframe)
            .unwrap();
        let dependent = packets
            .iter()
            .enumerate()
            .skip(first_keyframe + 1)
            .find_map(|(index, packet)| (!packet.metadata().keyframe).then_some(index))
            .unwrap();
        let next_keyframe = packets
            .iter()
            .enumerate()
            .skip(dependent + 1)
            .find_map(|(index, packet)| packet.metadata().keyframe.then_some(index))
            .unwrap();
        let mut fanout = PacketFanout::new();
        let subscriber = fanout
            .subscribe(
                template,
                PacketQueueLimits::new(
                    NonZeroUsize::new(64).unwrap(),
                    NonZeroUsize::new(8 * 1024 * 1024).unwrap(),
                    Duration::ZERO,
                ),
                PacketDeliveryPolicy::Realtime,
            )
            .unwrap();

        assert_eq!(fanout.publish(&packets[first_keyframe]).delivered, 1);
        assert_eq!(fanout.publish(&packets[dependent]).dropped, 1);
        assert_eq!(subscriber.dropped_packets(), 2);
        assert_eq!(fanout.publish(&packets[next_keyframe]).delivered, 1);
        let resumed = subscriber
            .receive_timeout(Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert!(resumed.metadata().keyframe);
        assert_eq!(resumed.metadata().stream_index, primary_video_stream);
    }

    #[test]
    fn slow_realtime_drop_and_resync_never_disturbs_reliable_recorder_delivery() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary_video_stream = template
            .streams()
            .into_iter()
            .find(|stream| stream.media_type == MediaType::Video)
            .unwrap()
            .stream_index;
        let mut packets = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            if packet.metadata().stream_index == primary_video_stream {
                packets.push(packet);
            }
        }
        let first_keyframe = packets
            .iter()
            .position(|packet| packet.metadata().keyframe)
            .unwrap();
        let dependent = packets
            .iter()
            .enumerate()
            .skip(first_keyframe + 1)
            .find_map(|(index, packet)| (!packet.metadata().keyframe).then_some(index))
            .unwrap();
        let next_keyframe = packets
            .iter()
            .enumerate()
            .skip(dependent + 1)
            .find_map(|(index, packet)| packet.metadata().keyframe.then_some(index))
            .unwrap();

        let expected_reliable = [
            packets[first_keyframe].data().to_vec(),
            packets[dependent].data().to_vec(),
            packets[next_keyframe].data().to_vec(),
        ];
        let mut fanout = PacketFanout::new();
        let reliable = fanout
            .subscribe(
                template.try_clone().unwrap(),
                limits(8),
                PacketDeliveryPolicy::Reliable,
            )
            .unwrap();
        let realtime = fanout
            .subscribe(template, limits(1), PacketDeliveryPolicy::Realtime)
            .unwrap();

        assert_eq!(fanout.publish(&packets[first_keyframe]).delivered, 2);
        let overflow = fanout.publish(&packets[dependent]);
        assert_eq!(overflow.delivered, 1);
        assert_eq!(overflow.dropped, 1);
        assert_eq!(overflow.failed, 0);
        assert_eq!(fanout.publish(&packets[next_keyframe]).delivered, 2);

        for expected in expected_reliable {
            let packet = reliable
                .receive_timeout(Duration::from_millis(50))
                .unwrap()
                .unwrap();
            assert_eq!(packet.data(), expected);
        }
        let resumed = realtime
            .receive_timeout(Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert!(resumed.metadata().keyframe);
        assert_eq!(resumed.data(), packets[next_keyframe].data());
        assert_eq!(realtime.dropped_packets(), 2);
    }

    #[test]
    fn prefilled_subscription_drains_history_before_future_live_packets() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let first = input.next_packet().unwrap().unwrap();
        let second = input.next_packet().unwrap().unwrap();
        let third = input.next_packet().unwrap().unwrap();
        let expected = [
            first.data().to_vec(),
            second.data().to_vec(),
            third.data().to_vec(),
        ];

        let mut fanout = PacketFanout::new();
        let subscriber = fanout
            .subscribe_with_prefill(
                template,
                limits(16),
                PacketDeliveryPolicy::Reliable,
                &[first, second],
                Duration::from_millis(250),
            )
            .unwrap();
        assert_eq!(subscriber.initial_history_age(), Duration::from_millis(250));
        assert_eq!(fanout.publish(&third).delivered, 1);

        for expected_payload in expected {
            let packet = subscriber
                .receive_timeout(Duration::from_millis(50))
                .unwrap()
                .unwrap();
            assert_eq!(packet.data(), expected_payload);
        }
    }

    #[test]
    fn named_subscriber_diagnostics_track_each_live_queue_and_drop() {
        let input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let mut fanout = PacketFanout::new();
        let mut subscriptions = Vec::new();
        for (consumer, policy) in [
            (
                PacketConsumerKind::Recording,
                PacketDeliveryPolicy::Reliable,
            ),
            (PacketConsumerKind::Live, PacketDeliveryPolicy::Realtime),
            (PacketConsumerKind::Motion, PacketDeliveryPolicy::Realtime),
            (PacketConsumerKind::Event, PacketDeliveryPolicy::Reliable),
        ] {
            subscriptions.push(
                fanout
                    .subscribe_consumer_with_prefill(
                        template.try_clone().unwrap(),
                        limits(8),
                        policy,
                        consumer,
                        &[],
                        Duration::ZERO,
                    )
                    .unwrap(),
            );
        }
        let snapshot = fanout.snapshot();
        assert_eq!(snapshot.subscribers, 4);
        assert_eq!(snapshot.reliable_subscribers, 2);
        assert_eq!(snapshot.realtime_subscribers, 2);
        assert_eq!(snapshot.consumers.total(), snapshot.subscribers);
        assert_eq!(snapshot.consumers.recording, 1);
        assert_eq!(snapshot.consumers.live, 1);
        assert_eq!(snapshot.consumers.motion, 1);
        assert_eq!(snapshot.consumers.event, 1);
        assert_eq!(snapshot.consumers.other, 0);

        drop(subscriptions.remove(2));
        let snapshot = fanout.snapshot();
        assert_eq!(snapshot.subscribers, 3);
        assert_eq!(snapshot.consumers.motion, 0);
        assert_eq!(snapshot.realtime_subscribers, 1);
        assert_eq!(snapshot.consumers.total(), snapshot.subscribers);

        drop(subscriptions);
        let snapshot = fanout.snapshot();
        assert_eq!(snapshot.subscribers, 0);
        assert_eq!(snapshot.consumers.total(), 0);
    }
}
