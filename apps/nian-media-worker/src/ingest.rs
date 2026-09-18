//! Shared RTSP ingest ownership for one media-worker process.
//!
//! Consumers subscribe to compressed FFmpeg packet references. Exactly one
//! `MediaInput` owns a given authenticated RTSP URL generation; source failure
//! terminates that generation for every subscriber so the existing recorder /
//! live retry state machines can establish the next generation without ever
//! opening duplicate camera connections inside one worker.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nian_media::{MediaError, MediaSource, RtspUrl};
use nian_media_ffmpeg::{
    FfmpegPacket, InterruptHandle, MediaInput, MediaStreamTemplate, PacketConsumerCounts,
    PacketConsumerKind, PacketDeliveryPolicy, PacketFanout, PacketQueueLimits, PacketSubscription,
};

const IDLE_GRACE: Duration = Duration::from_secs(3);
const SOURCE_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const SOURCE_READ_TIMEOUT: Duration = Duration::from_secs(15);
const PRE_ROLL_RETENTION: Duration = Duration::from_secs(10);
const PRE_ROLL_MAX_BYTES: usize = 32 * 1024 * 1024;

type RtspInputOpener =
    Arc<dyn Fn(&str, &InterruptHandle) -> Result<MediaInput, MediaError> + Send + Sync>;

fn open_rtsp_input(
    source_url: &str,
    interrupt: &InterruptHandle,
) -> Result<MediaInput, MediaError> {
    MediaInput::open(
        &MediaSource::Rtsp {
            url: RtspUrl::new(source_url.to_owned()),
        },
        interrupt,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestProfile {
    Main,
    Sub,
}

impl IngestProfile {
    pub(crate) fn from_wire(value: Option<&str>) -> Self {
        match value {
            Some("sub") => Self::Sub,
            _ => Self::Main,
        }
    }

    fn merge(self, other: Self) -> Self {
        if matches!(self, Self::Main) || matches!(other, Self::Main) {
            Self::Main
        } else {
            Self::Sub
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Sub => "sub",
        }
    }
}

pub struct PacketSubscriptionOptions {
    pub profile: IngestProfile,
    pub limits: PacketQueueLimits,
    pub policy: PacketDeliveryPolicy,
    pub consumer: PacketConsumerKind,
    pub pre_roll: Duration,
    pub ready_timeout: Duration,
}

#[derive(Debug)]
enum Lifecycle {
    Connecting,
    Ready,
    Failed(MediaError),
    Stopped,
}

struct RingPacket {
    packet: FfmpegPacket,
    arrived_at: Instant,
}

struct CompressedPacketRing {
    packets: VecDeque<RingPacket>,
    payload_bytes: usize,
    primary_video_stream: u32,
    dropped_packets: u64,
}

struct PreRollSnapshot {
    packets: Vec<FfmpegPacket>,
    oldest_age: Duration,
}

impl CompressedPacketRing {
    fn new(primary_video_stream: u32) -> Self {
        Self {
            packets: VecDeque::new(),
            payload_bytes: 0,
            primary_video_stream,
            dropped_packets: 0,
        }
    }

    fn push(&mut self, packet: &FfmpegPacket) {
        let packet_bytes = packet.data().len();
        if packet_bytes > PRE_ROLL_MAX_BYTES {
            self.dropped_packets = self
                .dropped_packets
                .saturating_add(u64::try_from(self.packets.len()).unwrap_or(u64::MAX))
                .saturating_add(1);
            self.packets.clear();
            self.payload_bytes = 0;
            return;
        }
        let Ok(packet) = packet.try_clone_ref() else {
            self.dropped_packets = self.dropped_packets.saturating_add(1);
            return;
        };
        self.payload_bytes = self.payload_bytes.saturating_add(packet_bytes);
        self.packets.push_back(RingPacket {
            packet,
            arrived_at: Instant::now(),
        });
        self.trim_bounds();
        self.trim_to_keyframe();
    }

    fn trim_bounds(&mut self) {
        loop {
            let over_age = self
                .packets
                .front()
                .is_some_and(|packet| packet.arrived_at.elapsed() > PRE_ROLL_RETENTION);
            if !over_age && self.payload_bytes <= PRE_ROLL_MAX_BYTES {
                break;
            }
            self.drop_front();
        }
    }

    fn trim_to_keyframe(&mut self) {
        let first_keyframe = self.packets.iter().position(|queued| {
            let metadata = queued.packet.metadata();
            metadata.stream_index == self.primary_video_stream && metadata.keyframe
        });
        match first_keyframe {
            Some(0) => {}
            Some(position) => {
                for _ in 0..position {
                    self.drop_front();
                }
            }
            None => {
                while !self.packets.is_empty() {
                    self.drop_front();
                }
            }
        }
    }

    fn drop_front(&mut self) {
        if let Some(packet) = self.packets.pop_front() {
            self.payload_bytes = self
                .payload_bytes
                .saturating_sub(packet.packet.data().len());
            self.dropped_packets = self.dropped_packets.saturating_add(1);
        }
    }

    fn snapshot(&mut self, max_age: Duration) -> Result<PreRollSnapshot, MediaError> {
        // A camera can stop sending packets while its RTSP read is still inside
        // the stall deadline. Expire old history on access too, not just on push.
        self.trim_bounds();
        self.trim_to_keyframe();
        if max_age.is_zero() || self.packets.is_empty() {
            return Ok(PreRollSnapshot {
                packets: Vec::new(),
                oldest_age: Duration::ZERO,
            });
        }
        let cutoff = Instant::now()
            .checked_sub(max_age)
            .unwrap_or_else(Instant::now);
        let is_keyframe = |queued: &RingPacket| {
            let metadata = queued.packet.metadata();
            metadata.stream_index == self.primary_video_stream && metadata.keyframe
        };
        let start = self
            .packets
            .iter()
            .enumerate()
            .filter(|(_, queued)| is_keyframe(queued) && queued.arrived_at <= cutoff)
            .map(|(index, _)| index)
            .next_back()
            .or_else(|| {
                self.packets
                    .iter()
                    .enumerate()
                    .find(|(_, queued)| is_keyframe(queued))
                    .map(|(index, _)| index)
            })
            .unwrap_or(0);
        let oldest_age = self
            .packets
            .get(start)
            .map(|queued| queued.arrived_at.elapsed())
            .unwrap_or(Duration::ZERO);
        let packets = self
            .packets
            .iter()
            .skip(start)
            .map(|queued| queued.packet.try_clone_ref())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PreRollSnapshot {
            packets,
            oldest_age,
        })
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

struct IngestState {
    lifecycle: Lifecycle,
    template: Option<MediaStreamTemplate>,
    fanout: PacketFanout,
    pre_roll: Option<CompressedPacketRing>,
    ready_since: Option<Instant>,
}

struct IngestEntry {
    profile: Mutex<IngestProfile>,
    source_url: String,
    state: Mutex<IngestState>,
    changed: Condvar,
    stop: AtomicBool,
    retain_count: AtomicUsize,
    interrupt: Mutex<Option<InterruptHandle>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl IngestEntry {
    fn spawn(
        source_url: String,
        profile: IngestProfile,
        opener: RtspInputOpener,
    ) -> Result<Arc<Self>, MediaError> {
        let entry = Arc::new(Self {
            profile: Mutex::new(profile),
            source_url,
            state: Mutex::new(IngestState {
                lifecycle: Lifecycle::Connecting,
                template: None,
                fanout: PacketFanout::new(),
                pre_roll: None,
                ready_since: None,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            retain_count: AtomicUsize::new(0),
            interrupt: Mutex::new(None),
            thread: Mutex::new(None),
        });
        let run_entry = Arc::clone(&entry);
        let thread = std::thread::Builder::new()
            .name("shared-rtsp-ingest".to_owned())
            .spawn(move || run_ingest(run_entry, opener))
            .map_err(|_| MediaError::OpenFailed {
                message: "shared ingest thread could not start".to_owned(),
            })?;
        *entry
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(thread);
        Ok(entry)
    }

    fn is_reusable(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(state.lifecycle, Lifecycle::Connecting | Lifecycle::Ready)
            && !self.stop.load(Ordering::Acquire)
    }

    fn observe_profile(&self, profile: IngestProfile) {
        let mut current = self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = current.merge(profile);
    }

    fn current_profile(&self) -> IngestProfile {
        *self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn subscribe(
        &self,
        limits: PacketQueueLimits,
        policy: PacketDeliveryPolicy,
        consumer: PacketConsumerKind,
        pre_roll: Duration,
        ready_timeout: Duration,
        consumer_interrupt: Option<&InterruptHandle>,
    ) -> Result<PacketSubscription, MediaError> {
        let deadline = Instant::now() + ready_timeout;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if consumer_interrupt.is_some_and(InterruptHandle::is_cancelled) {
                return Err(MediaError::Interrupted {
                    operation: "open shared ingest subscription",
                });
            }
            match &state.lifecycle {
                Lifecycle::Ready => {
                    let template = state
                        .template
                        .as_ref()
                        .ok_or_else(|| MediaError::OpenFailed {
                            message: "shared ingest became ready without stream metadata"
                                .to_owned(),
                        })?
                        .try_clone()?;
                    let prefill = state
                        .pre_roll
                        .as_mut()
                        .map(|ring| ring.snapshot(pre_roll))
                        .transpose()?
                        .unwrap_or(PreRollSnapshot {
                            packets: Vec::new(),
                            oldest_age: Duration::ZERO,
                        });
                    return state.fanout.subscribe_consumer_with_prefill(
                        template,
                        limits,
                        policy,
                        consumer,
                        &prefill.packets,
                        prefill.oldest_age,
                    );
                }
                Lifecycle::Failed(error) => return Err(error.clone()),
                Lifecycle::Stopped => {
                    return Err(MediaError::Interrupted {
                        operation: "open shared ingest subscription",
                    });
                }
                Lifecycle::Connecting => {}
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(MediaError::TimedOut {
                    operation: "open shared ingest subscription",
                });
            }
            let wait_slice = if consumer_interrupt.is_some() {
                remaining.min(Duration::from_millis(50))
            } else {
                remaining
            };
            let waited = self
                .changed
                .wait_timeout(state, wait_slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = waited.0;
            if waited.1.timed_out()
                && consumer_interrupt.is_none()
                && matches!(state.lifecycle, Lifecycle::Connecting)
            {
                return Err(MediaError::TimedOut {
                    operation: "open shared ingest subscription",
                });
            }
        }
    }

    fn retain(
        self: &Arc<Self>,
        ready_timeout: Duration,
        consumer_interrupt: Option<&InterruptHandle>,
    ) -> Result<SharedIngestLease, MediaError> {
        let deadline = Instant::now() + ready_timeout;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if consumer_interrupt.is_some_and(InterruptHandle::is_cancelled) {
                return Err(MediaError::Interrupted {
                    operation: "retain shared ingest",
                });
            }
            match &state.lifecycle {
                Lifecycle::Ready => {
                    self.retain_count.fetch_add(1, Ordering::AcqRel);
                    return Ok(SharedIngestLease {
                        entry: Arc::clone(self),
                    });
                }
                Lifecycle::Failed(error) => return Err(error.clone()),
                Lifecycle::Stopped => {
                    return Err(MediaError::Interrupted {
                        operation: "retain shared ingest",
                    });
                }
                Lifecycle::Connecting => {}
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(MediaError::TimedOut {
                    operation: "retain shared ingest",
                });
            }
            let wait_slice = if consumer_interrupt.is_some() {
                remaining.min(Duration::from_millis(50))
            } else {
                remaining
            };
            let waited = self
                .changed
                .wait_timeout(state, wait_slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = waited.0;
        }
    }

    fn stop_and_join(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(interrupt) = self
            .interrupt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            interrupt.cancel();
        }
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = thread.join();
        }
    }
}

pub struct SharedIngestLease {
    entry: Arc<IngestEntry>,
}

impl SharedIngestLease {
    pub fn is_ready(&self) -> bool {
        let state = self
            .entry
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(state.lifecycle, Lifecycle::Ready) && !self.entry.stop.load(Ordering::Acquire)
    }
}

impl Drop for SharedIngestLease {
    fn drop(&mut self) {
        self.entry.retain_count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for SharedIngestLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedIngestLease").finish_non_exhaustive()
    }
}

/// Process-local registry of shared RTSP ingest generations.
///
/// The URL key is intentionally never exposed through Debug or status payloads;
/// it may contain camera credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestSourceDiagnostics {
    pub profile: &'static str,
    pub lifecycle: &'static str,
    pub retainers: usize,
    pub subscribers: usize,
    pub reliable_subscribers: usize,
    pub realtime_subscribers: usize,
    pub consumers: PacketConsumerCounts,
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestDiagnostics {
    pub source_count: usize,
    pub main_sources: usize,
    pub sub_sources: usize,
    pub connecting_sources: usize,
    pub ready_sources: usize,
    pub failed_sources: usize,
    pub subscribers: usize,
    pub reliable_subscribers: usize,
    pub realtime_subscribers: usize,
    pub consumers: PacketConsumerCounts,
    pub queued_packets: usize,
    pub queued_bytes: usize,
    pub dropped_packets: u64,
    pub pre_roll_packets: usize,
    pub pre_roll_bytes: usize,
    pub pre_roll_dropped_packets: u64,
    pub generation_starts: u64,
    pub sources: Vec<IngestSourceDiagnostics>,
}

#[derive(Clone)]
pub struct SharedIngestManager {
    entries: Arc<Mutex<HashMap<String, Arc<IngestEntry>>>>,
    opener: RtspInputOpener,
    generation_starts: Arc<AtomicU64>,
}

impl Default for SharedIngestManager {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            opener: Arc::new(open_rtsp_input),
            generation_starts: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl SharedIngestManager {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_opener(
        opener: impl Fn(&str, &InterruptHandle) -> Result<MediaInput, MediaError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            opener: Arc::new(opener),
            generation_starts: Arc::new(AtomicU64::new(0)),
        }
    }

    fn entry_for(
        &self,
        source_url: &str,
        profile: IngestProfile,
    ) -> Result<Arc<IngestEntry>, MediaError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = entries.get(source_url)
            && existing.is_reusable()
        {
            existing.observe_profile(profile);
            return Ok(Arc::clone(existing));
        }
        if let Some(stale) = entries.remove(source_url) {
            stale.stop_and_join();
        }
        let entry = IngestEntry::spawn(source_url.to_owned(), profile, Arc::clone(&self.opener))?;
        self.generation_starts.fetch_add(1, Ordering::Relaxed);
        entries.insert(source_url.to_owned(), Arc::clone(&entry));
        Ok(entry)
    }

    pub fn subscribe_rtsp_with_options(
        &self,
        source_url: &str,
        options: PacketSubscriptionOptions,
        consumer_interrupt: Option<&InterruptHandle>,
    ) -> Result<PacketSubscription, MediaError> {
        let entry = self.entry_for(source_url, options.profile)?;
        entry.subscribe(
            options.limits,
            options.policy,
            options.consumer,
            options.pre_roll,
            options.ready_timeout,
            consumer_interrupt,
        )
    }

    pub fn retain_rtsp(
        &self,
        source_url: &str,
        profile: IngestProfile,
        ready_timeout: Duration,
        consumer_interrupt: Option<&InterruptHandle>,
    ) -> Result<SharedIngestLease, MediaError> {
        let entry = self.entry_for(source_url, profile)?;
        entry.retain(ready_timeout, consumer_interrupt)
    }

    pub fn diagnostics(&self) -> IngestDiagnostics {
        let entries = self
            .entries
            .lock()
            .map(|entries| entries.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut diagnostics = IngestDiagnostics {
            source_count: entries.len(),
            generation_starts: self.generation_starts.load(Ordering::Relaxed),
            ..IngestDiagnostics::default()
        };
        for entry in entries {
            let profile = *entry
                .profile
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match profile {
                IngestProfile::Main => diagnostics.main_sources += 1,
                IngestProfile::Sub => diagnostics.sub_sources += 1,
            }
            let mut state = entry
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let lifecycle = match &state.lifecycle {
                Lifecycle::Connecting => {
                    diagnostics.connecting_sources += 1;
                    "connecting"
                }
                Lifecycle::Ready => {
                    diagnostics.ready_sources += 1;
                    "ready"
                }
                Lifecycle::Failed(_) => {
                    diagnostics.failed_sources += 1;
                    "failed"
                }
                Lifecycle::Stopped => "stopped",
            };
            let fanout = state.fanout.snapshot();
            diagnostics.subscribers = diagnostics.subscribers.saturating_add(fanout.subscribers);
            diagnostics.reliable_subscribers = diagnostics
                .reliable_subscribers
                .saturating_add(fanout.reliable_subscribers);
            diagnostics.realtime_subscribers = diagnostics
                .realtime_subscribers
                .saturating_add(fanout.realtime_subscribers);
            diagnostics.consumers.merge(fanout.consumers);
            diagnostics.queued_packets = diagnostics
                .queued_packets
                .saturating_add(fanout.queued_packets);
            diagnostics.queued_bytes = diagnostics.queued_bytes.saturating_add(fanout.queued_bytes);
            diagnostics.dropped_packets = diagnostics
                .dropped_packets
                .saturating_add(fanout.dropped_packets);

            let (pre_roll_packets, pre_roll_bytes, pre_roll_dropped_packets) = state
                .pre_roll
                .as_mut()
                .map(|pre_roll| {
                    // A stalled source must not report expired history as live.
                    pre_roll.trim_bounds();
                    pre_roll.trim_to_keyframe();
                    (
                        pre_roll.len(),
                        pre_roll.payload_bytes,
                        pre_roll.dropped_packets,
                    )
                })
                .unwrap_or_default();
            diagnostics.pre_roll_packets = diagnostics
                .pre_roll_packets
                .saturating_add(pre_roll_packets);
            diagnostics.pre_roll_bytes = diagnostics.pre_roll_bytes.saturating_add(pre_roll_bytes);
            diagnostics.pre_roll_dropped_packets = diagnostics
                .pre_roll_dropped_packets
                .saturating_add(pre_roll_dropped_packets);

            let video = state.template.as_ref().and_then(|template| {
                template
                    .streams()
                    .into_iter()
                    .find(|stream| stream.media_type == nian_domain::MediaType::Video)
            });
            diagnostics.sources.push(IngestSourceDiagnostics {
                profile: profile.as_str(),
                lifecycle,
                retainers: entry.retain_count.load(Ordering::Relaxed),
                subscribers: fanout.subscribers,
                reliable_subscribers: fanout.reliable_subscribers,
                realtime_subscribers: fanout.realtime_subscribers,
                consumers: fanout.consumers,
                queued_packets: fanout.queued_packets,
                queued_bytes: fanout.queued_bytes,
                dropped_packets: fanout.dropped_packets,
                pre_roll_packets,
                pre_roll_bytes,
                pre_roll_dropped_packets,
                video_frame_rate: video.as_ref().and_then(|stream| stream.frame_rate),
                video_codec: video.as_ref().map(|stream| stream.codec_name.clone()),
                video_width: video.as_ref().and_then(|stream| stream.width),
                video_height: video.as_ref().and_then(|stream| stream.height),
            });
        }
        diagnostics
            .sources
            .sort_by_key(|source| match source.profile {
                "main" => 0_u8,
                _ => 1_u8,
            });
        diagnostics
    }

    pub fn shutdown(&self) {
        let entries = {
            let mut owned = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            owned.drain().map(|(_, entry)| entry).collect::<Vec<_>>()
        };
        for entry in entries {
            entry.stop_and_join();
        }
    }
}

impl std::fmt::Debug for SharedIngestManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or_default();
        f.debug_struct("SharedIngestManager")
            .field("source_count", &count)
            .finish()
    }
}

fn primary_video_stream(template: &MediaStreamTemplate) -> Option<u32> {
    template
        .streams()
        .into_iter()
        .find(|stream| stream.media_type == nian_domain::MediaType::Video)
        .map(|stream| stream.stream_index)
}

fn run_ingest(entry: Arc<IngestEntry>, opener: RtspInputOpener) {
    let interrupt = InterruptHandle::new();
    *entry
        .interrupt
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(interrupt.clone());

    let mut input = {
        let _open_deadline = interrupt.scoped_deadline(SOURCE_OPEN_TIMEOUT);
        match opener(&entry.source_url, &interrupt) {
            Ok(input) => input,
            Err(error) => {
                finish_failed(&entry, error);
                return;
            }
        }
    };
    let template = match MediaStreamTemplate::capture(&input) {
        Ok(template) => template,
        Err(error) => {
            finish_failed(&entry, error);
            return;
        }
    };

    let primary_video_index = primary_video_stream(&template);
    let pre_roll = matches!(entry.current_profile(), IngestProfile::Main)
        .then_some(primary_video_index)
        .flatten()
        .map(CompressedPacketRing::new);

    {
        let mut state = entry
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.template = Some(template);
        state.pre_roll = pre_roll;
        state.lifecycle = Lifecycle::Ready;
        state.ready_since = Some(Instant::now());
        entry.changed.notify_all();
    }

    loop {
        if entry.stop.load(Ordering::Acquire) {
            finish_stopped(&entry);
            return;
        }

        {
            let mut state = entry
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.fanout.active_subscribers() == 0
                && entry.retain_count.load(Ordering::Acquire) == 0
                && state
                    .ready_since
                    .is_some_and(|ready_since| ready_since.elapsed() >= IDLE_GRACE)
            {
                state.fanout.finish_eof();
                state.lifecycle = Lifecycle::Stopped;
                entry.changed.notify_all();
                return;
            }
        }

        let read_result = {
            let _read_deadline = interrupt.scoped_deadline(SOURCE_READ_TIMEOUT);
            input.next_packet()
        };
        match read_result {
            Ok(Some(packet)) => {
                let keep_pre_roll = matches!(entry.current_profile(), IngestProfile::Main);
                let mut state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if keep_pre_roll && state.pre_roll.is_none() {
                    state.pre_roll = state
                        .template
                        .as_ref()
                        .and_then(primary_video_stream)
                        .map(CompressedPacketRing::new);
                }
                if let Some(pre_roll) = state.pre_roll.as_mut() {
                    pre_roll.push(&packet);
                }
                state.fanout.publish(&packet);
            }
            Ok(None) => {
                finish_failed(
                    &entry,
                    MediaError::ReadFailed {
                        message: "shared RTSP ingest ended unexpectedly".to_owned(),
                    },
                );
                return;
            }
            Err(error) if entry.stop.load(Ordering::Acquire) || error.is_interrupted() => {
                finish_stopped(&entry);
                return;
            }
            Err(error) => {
                finish_failed(&entry, error);
                return;
            }
        }
    }
}

fn finish_failed(entry: &IngestEntry, error: MediaError) {
    let mut state = entry
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.fanout.finish_error(error.clone());
    // Failed generations must not pin their old compressed history indefinitely.
    state.pre_roll = None;
    state.lifecycle = Lifecycle::Failed(error);
    entry.changed.notify_all();
}

fn finish_stopped(entry: &IngestEntry) {
    let mut state = entry
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.fanout.finish_eof();
    state.pre_roll = None;
    state.lifecycle = Lifecycle::Stopped;
    entry.changed.notify_all();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.extend([
            "..",
            "..",
            "crates",
            "nian-media-ffmpeg",
            "tests",
            "fixtures",
            "sample.mkv",
        ]);
        path
    }

    fn fixture_input() -> MediaInput {
        MediaInput::open(&MediaSource::file(fixture_path()), &InterruptHandle::new()).unwrap()
    }

    type OpenCounts = Arc<Mutex<HashMap<String, usize>>>;
    type OpenGate = Arc<(Mutex<bool>, Condvar)>;

    fn counting_manager() -> (SharedIngestManager, OpenCounts, OpenGate) {
        let counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let path = fixture_path();
        let counts_for_open = Arc::clone(&counts);
        let gate_for_open = Arc::clone(&gate);
        let manager = SharedIngestManager::with_opener(move |source_url, interrupt| {
            *counts_for_open
                .lock()
                .unwrap()
                .entry(source_url.to_owned())
                .or_default() += 1;
            let (released, changed) = &*gate_for_open;
            let mut released = released.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            drop(released);
            MediaInput::open(&MediaSource::file(path.clone()), interrupt)
        });
        (manager, counts, gate)
    }

    fn wait_for_open_count(counts: &OpenCounts, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let total = counts.lock().unwrap().values().copied().sum::<usize>();
            if total >= expected {
                assert_eq!(total, expected);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for ingest opens"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn release_openers(gate: &OpenGate) {
        let (released, changed) = &**gate;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }

    fn assert_opened_once(counts: &OpenCounts, source_url: &str) {
        assert_eq!(counts.lock().unwrap().get(source_url).copied(), Some(1));
    }

    #[test]
    fn pre_roll_snapshot_starts_at_the_closest_keyframe_not_newer_than_window() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary = primary_video_stream(&template).unwrap();
        let mut packets = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            if packet.metadata().stream_index == primary {
                packets.push(packet);
            }
        }
        let first_keyframe = packets
            .iter()
            .position(|packet| packet.metadata().keyframe)
            .unwrap();
        let second_keyframe = packets
            .iter()
            .enumerate()
            .skip(first_keyframe + 1)
            .find_map(|(index, packet)| packet.metadata().keyframe.then_some(index))
            .unwrap();

        let mut ring = CompressedPacketRing::new(primary);
        for packet in &packets[first_keyframe..=second_keyframe] {
            ring.push(packet);
        }
        let second_relative = second_keyframe - first_keyframe;
        let now = Instant::now();
        for (index, queued) in ring.packets.iter_mut().enumerate() {
            let age = if index < second_relative {
                Duration::from_secs(8)
            } else {
                Duration::from_secs(4)
            };
            queued.arrived_at = now.checked_sub(age).unwrap();
        }

        let snapshot = ring.snapshot(Duration::from_secs(3)).unwrap();
        assert!(!snapshot.packets.is_empty());
        assert!(snapshot.packets[0].metadata().keyframe);
        assert_eq!(snapshot.packets[0].data(), packets[second_keyframe].data());
        assert!(snapshot.oldest_age >= Duration::from_secs(3));
        assert!(snapshot.oldest_age < PRE_ROLL_RETENTION);
    }

    #[test]
    fn stalled_ingest_expires_pre_roll_on_snapshot_without_new_packets() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary = primary_video_stream(&template).unwrap();
        let first_keyframe = loop {
            let packet = input.next_packet().unwrap().unwrap();
            if packet.metadata().stream_index == primary && packet.metadata().keyframe {
                break packet;
            }
        };
        let mut ring = CompressedPacketRing::new(primary);
        ring.push(&first_keyframe);
        assert_eq!(ring.len(), 1);
        assert!(ring.payload_bytes > 0);
        ring.packets.front_mut().unwrap().arrived_at = Instant::now()
            .checked_sub(PRE_ROLL_RETENTION + Duration::from_secs(1))
            .unwrap();

        let snapshot = ring.snapshot(Duration::from_secs(5)).unwrap();
        assert!(
            snapshot.packets.is_empty(),
            "stale packets must not seed a new Event clip"
        );
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.payload_bytes, 0);
        assert_eq!(ring.dropped_packets, 1);
    }

    #[test]
    fn failed_ingest_releases_pre_roll_before_any_reconnect() {
        let mut input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let primary = primary_video_stream(&template).unwrap();
        let first_keyframe = loop {
            let packet = input.next_packet().unwrap().unwrap();
            if packet.metadata().stream_index == primary && packet.metadata().keyframe {
                break packet;
            }
        };
        let mut ring = CompressedPacketRing::new(primary);
        ring.push(&first_keyframe);
        assert!(ring.payload_bytes > 0);
        let entry = IngestEntry {
            profile: Mutex::new(IngestProfile::Main),
            source_url: "rtsp://camera.invalid/main".to_owned(),
            state: Mutex::new(IngestState {
                lifecycle: Lifecycle::Ready,
                template: Some(template),
                fanout: PacketFanout::new(),
                pre_roll: Some(ring),
                ready_since: Some(Instant::now()),
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            retain_count: AtomicUsize::new(0),
            interrupt: Mutex::new(None),
            thread: Mutex::new(None),
        };

        finish_failed(
            &entry,
            MediaError::ReadFailed {
                message: "fixture source disconnected".to_owned(),
            },
        );
        let state = entry.state.lock().unwrap();
        assert!(matches!(state.lifecycle, Lifecycle::Failed(_)));
        assert!(
            state.pre_roll.is_none(),
            "failed ingest must release the full compressed ring"
        );
    }

    #[test]
    fn pre_roll_lease_health_tracks_only_its_owned_ingest_generation() {
        let entry = Arc::new(IngestEntry {
            profile: Mutex::new(IngestProfile::Main),
            source_url: "rtsp://redacted.invalid/main".to_owned(),
            state: Mutex::new(IngestState {
                lifecycle: Lifecycle::Ready,
                template: None,
                fanout: PacketFanout::new(),
                pre_roll: None,
                ready_since: Some(Instant::now()),
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            retain_count: AtomicUsize::new(1),
            interrupt: Mutex::new(None),
            thread: Mutex::new(None),
        });
        let lease = SharedIngestLease {
            entry: Arc::clone(&entry),
        };
        assert!(lease.is_ready());

        entry.state.lock().unwrap().lifecycle = Lifecycle::Failed(MediaError::ReadFailed {
            message: "fixture source failed".to_owned(),
        });
        assert!(!lease.is_ready());
        drop(lease);
        assert_eq!(entry.retain_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn per_source_diagnostics_report_safe_media_shape_without_source_identity() {
        let input = fixture_input();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let secret_url = "rtsp://user:super-secret@camera.invalid/main";
        let mut fanout = PacketFanout::new();
        let limits = PacketQueueLimits::new(
            std::num::NonZeroUsize::new(8).unwrap(),
            std::num::NonZeroUsize::new(1024 * 1024).unwrap(),
            Duration::from_secs(2),
        );
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
                        limits,
                        policy,
                        consumer,
                        &[],
                        Duration::ZERO,
                    )
                    .unwrap(),
            );
        }
        let entry = Arc::new(IngestEntry {
            profile: Mutex::new(IngestProfile::Main),
            source_url: secret_url.to_owned(),
            state: Mutex::new(IngestState {
                lifecycle: Lifecycle::Ready,
                template: Some(template),
                fanout,
                pre_roll: None,
                ready_since: Some(Instant::now()),
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            retain_count: AtomicUsize::new(2),
            interrupt: Mutex::new(None),
            thread: Mutex::new(None),
        });
        let manager = SharedIngestManager::new();
        manager
            .entries
            .lock()
            .unwrap()
            .insert(secret_url.to_owned(), entry);

        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.sources.len(), 1);
        let source = &diagnostics.sources[0];
        assert_eq!(source.profile, "main");
        assert_eq!(source.lifecycle, "ready");
        assert_eq!(source.retainers, 2);
        assert_eq!(source.subscribers, 4);
        assert_eq!(source.consumers.recording, 1);
        assert_eq!(source.consumers.live, 1);
        assert_eq!(source.consumers.motion, 1);
        assert_eq!(source.consumers.event, 1);
        assert_eq!(source.consumers.total(), 4);
        assert_eq!(diagnostics.consumers, source.consumers);
        assert_eq!(diagnostics.subscribers, diagnostics.consumers.total());
        drop(subscriptions);
        let released = manager.diagnostics();
        assert_eq!(released.subscribers, 0);
        assert_eq!(released.consumers.total(), 0);
        assert_eq!(
            source.video_frame_rate,
            nian_domain::MediaRational::new(10, 1).ok()
        );
        assert_eq!(source.video_codec.as_deref(), Some("mpeg4"));
        assert_eq!(source.video_width, Some(160));
        assert_eq!(source.video_height, Some(120));
        let rendered = format!("{diagnostics:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("camera.invalid"));
        assert!(!rendered.contains("rtsp://"));
    }

    #[test]
    fn recording_focus_and_event_preroll_resolve_to_one_main_ingest_generation() {
        let (manager, counts, gate) = counting_manager();
        let main = "rtsp://camera.invalid/main";

        let recording = manager.entry_for(main, IngestProfile::Main).unwrap();
        let focus = manager.entry_for(main, IngestProfile::Main).unwrap();
        let event_pre_roll = manager.entry_for(main, IngestProfile::Main).unwrap();

        assert!(Arc::ptr_eq(&recording, &focus));
        assert!(Arc::ptr_eq(&recording, &event_pre_roll));
        wait_for_open_count(&counts, 1);
        assert_opened_once(&counts, main);
        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.source_count, 1);
        assert_eq!(diagnostics.generation_starts, 1);

        release_openers(&gate);
        manager.shutdown();
    }

    #[test]
    fn multiple_sub_profile_consumers_share_exactly_one_sub_ingest_open() {
        let (manager, counts, gate) = counting_manager();
        let sub = "rtsp://camera.invalid/sub";

        let grid = manager.entry_for(sub, IngestProfile::Sub).unwrap();
        let second_subscriber = manager.entry_for(sub, IngestProfile::Sub).unwrap();

        assert!(Arc::ptr_eq(&grid, &second_subscriber));
        wait_for_open_count(&counts, 1);
        assert_opened_once(&counts, sub);
        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.source_count, 1);
        assert_eq!(diagnostics.sub_sources, 1);
        assert_eq!(diagnostics.generation_starts, 1);

        release_openers(&gate);
        manager.shutdown();
    }

    #[test]
    fn main_consumers_plus_multiple_sub_consumers_open_at_most_main_plus_sub() {
        let (manager, counts, gate) = counting_manager();
        let main = "rtsp://camera.invalid/main";
        let sub = "rtsp://camera.invalid/sub";

        let recording = manager.entry_for(main, IngestProfile::Main).unwrap();
        let focus = manager.entry_for(main, IngestProfile::Main).unwrap();
        let event_pre_roll = manager.entry_for(main, IngestProfile::Main).unwrap();
        let grid = manager.entry_for(sub, IngestProfile::Sub).unwrap();
        let second_subscriber = manager.entry_for(sub, IngestProfile::Sub).unwrap();

        assert!(Arc::ptr_eq(&recording, &focus));
        assert!(Arc::ptr_eq(&recording, &event_pre_roll));
        assert!(Arc::ptr_eq(&grid, &second_subscriber));
        assert!(!Arc::ptr_eq(&recording, &grid));
        wait_for_open_count(&counts, 2);
        assert_opened_once(&counts, main);
        assert_opened_once(&counts, sub);
        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.source_count, 2);
        assert_eq!(diagnostics.main_sources, 1);
        assert_eq!(diagnostics.sub_sources, 1);
        assert_eq!(diagnostics.generation_starts, 2);

        release_openers(&gate);
        manager.shutdown();
    }

    #[test]
    fn identical_main_and_sub_endpoint_dedupes_to_one_ingest_and_promotes_profile() {
        let (manager, counts, gate) = counting_manager();
        let shared = "rtsp://camera.invalid/shared";

        let grid = manager.entry_for(shared, IngestProfile::Sub).unwrap();
        let recording = manager.entry_for(shared, IngestProfile::Main).unwrap();

        assert!(Arc::ptr_eq(&grid, &recording));
        assert_eq!(recording.current_profile(), IngestProfile::Main);
        wait_for_open_count(&counts, 1);
        assert_opened_once(&counts, shared);
        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.source_count, 1);
        assert_eq!(diagnostics.main_sources, 1);
        assert_eq!(diagnostics.sub_sources, 0);
        assert_eq!(diagnostics.generation_starts, 1);

        release_openers(&gate);
        manager.shutdown();
    }

    #[test]
    fn failed_generation_is_replaced_once_and_concurrent_reusers_share_the_reconnect() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let path = fixture_path();
        let attempts_for_open = Arc::clone(&attempts);
        let gate_for_open = Arc::clone(&gate);
        let manager = SharedIngestManager::with_opener(move |_source_url, interrupt| {
            let attempt = attempts_for_open.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                return Err(MediaError::OpenFailed {
                    message: "fixture first generation failed".to_owned(),
                });
            }
            assert_eq!(attempt, 2, "reconnect must remain single-flight");
            let (released, changed) = &*gate_for_open;
            let mut released = released.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            drop(released);
            MediaInput::open(&MediaSource::file(path.clone()), interrupt)
        });
        let source = "rtsp://camera.invalid/main";

        let failed = manager.entry_for(source, IngestProfile::Main).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let is_failed = matches!(failed.state.lock().unwrap().lifecycle, Lifecycle::Failed(_));
            if is_failed {
                break;
            }
            assert!(Instant::now() < deadline, "first generation did not fail");
            std::thread::sleep(Duration::from_millis(5));
        }

        let reconnect = manager.entry_for(source, IngestProfile::Main).unwrap();
        let concurrent_reuser = manager.entry_for(source, IngestProfile::Main).unwrap();
        assert!(!Arc::ptr_eq(&failed, &reconnect));
        assert!(Arc::ptr_eq(&reconnect, &concurrent_reuser));

        let deadline = Instant::now() + Duration::from_secs(1);
        while attempts.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "reconnect opener did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(manager.diagnostics().source_count, 1);
        assert_eq!(manager.diagnostics().generation_starts, 2);

        release_openers(&gate);
        manager.shutdown();
    }
}
