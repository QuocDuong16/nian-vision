//! The recording engine: one media input, one active segment at a time.
//!
//! Ownership and lifetime order (milestone requirement §1): a
//! [`RecordingSession`] owns the [`MediaInput`], its [`InterruptHandle`]
//! and the shared [`StopFlag`]; each open segment owns its
//! [`ClaimedSegment`] (the exclusive claim token) and the
//! [`MatroskaMuxer`] writing into it. A segment is finalized and published
//! before the next claim is taken; nothing outlives its owner.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use nian_domain::{MediaPacketMetadata, MediaRational, MediaStreamInfo, MediaType};
use nian_media::MediaSource;
use nian_media_ffmpeg::{FfmpegPacket, InterruptHandle, MatroskaMuxer, MediaInput};
use nian_storage::paths::{ClaimedSegment, publish_no_replace};
use nian_storage::{RecordingsLayout, StorageError};

use crate::{
    AudioPolicy, RecorderConfig, RecordingEndReason, RecordingError, RecordingEvent, StreamPlan,
};

/// Cooperative graceful-stop flag.
///
/// Settable from any thread ([`StopFlag::request`]); the recording loop
/// checks it **between packets**, so the current packet operation always
/// finishes and normal finalization is never cut short. This is
/// deliberately separate from the FFmpeg [`InterruptHandle`]: cancellation
/// aborts blocking I/O mid-flight and therefore forces an *abandon* of the
/// active segment, while a stop request still lets trailer + flush +
/// publish run to completion.
#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    /// Creates a flag in the not-stopped state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests a graceful stop at the next loop-iteration boundary.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a stop has been requested.
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One open segment: an exclusively claimed slot plus the muxer writing
/// into it, with the media-time bookkeeping that drives rotation.
struct Segment {
    claim: ClaimedSegment,
    muxer: MatroskaMuxer,
    started_wall: NaiveDateTime,
    /// Validated time base of the primary video stream.
    video_time_base: MediaRational,
    /// Segment target converted into video time-base ticks.
    target_ticks: i64,
    /// DTS (fallback PTS) of the first timestamped video packet — the zero
    /// point of this segment's media clock. `None` until such a packet
    /// arrives; no timestamp is ever invented for it.
    start_media: Option<i64>,
    /// Most recent video timestamp seen, for duration reporting.
    last_media: Option<i64>,
    /// Video packets successfully handed to the muxer (including the
    /// opening keyframe). A segment below one never publishes.
    video_packets: u64,
    /// Set once elapsed media time reached the target: the *next* selected
    /// video keyframe closes this segment.
    rotation_pending: bool,
}

impl Segment {
    /// Feeds a video packet's timestamps into the segment's media clock.
    ///
    /// DTS is preferred over PTS because stream copy writes packets in
    /// decode order; with B-frames, PTS oscillates around DTS and would
    /// mis-measure elapsed time. Packets carrying neither timestamp
    /// (`AV_NOPTS_VALUE`) contribute content but not time — nothing is
    /// invented. Backward jumps are clamped so a discontinuity can only
    /// delay rotation, never rewind it.
    fn observe_video(&mut self, metadata: &MediaPacketMetadata) {
        let Some(timestamp) = metadata.dts.or(metadata.pts) else {
            return;
        };
        let start = *self.start_media.get_or_insert(timestamp);
        self.last_media = Some(timestamp);
        let elapsed = timestamp.saturating_sub(start).max(0);
        if !self.rotation_pending && elapsed >= self.target_ticks {
            self.rotation_pending = true;
        }
    }

    /// Durably finishes the segment: trailer → flush/close → no-replace
    /// publish → `SegmentFinalized`. Any failure keeps the partial file in
    /// recovery shape, announces the abandonment through `events`, and
    /// propagates the error. Returns the published size in bytes.
    ///
    /// Empty/tiny-segment rule (§12): a segment that never received a
    /// video packet is never published — it is abandoned exactly like a
    /// failed one.
    fn finalize_and_publish(
        self,
        events: &mut dyn FnMut(RecordingEvent),
    ) -> Result<u64, RecordingError> {
        // Destructure up front: `muxer.finalize` consumes the muxer.
        let Segment {
            claim,
            muxer,
            started_wall,
            video_time_base,
            start_media,
            last_media,
            video_packets,
            ..
        } = self;
        let partial_path = claim.partial_path().to_path_buf();
        let final_path = claim.final_path().to_path_buf();

        if video_packets == 0 {
            drop(muxer); // crash-shaped partial stays on disk, unpublished
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason: "no video packets were written".to_owned(),
            });
            return Ok(0);
        }

        let media_duration = media_duration_of(video_time_base, start_media, last_media);

        // Trailer AND final flush/close must succeed before anything is
        // published (§9); on failure the partial stays recovery-eligible.
        if let Err(error) = muxer.finalize() {
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason: format!("finalization failed: {error}"),
            });
            return Err(error.into());
        }

        // Atomic, never-replacing publication.
        if let Err(error) = publish_no_replace(&partial_path, &final_path) {
            // The media content is complete at this point but keeps the
            // `.partial` name — reconciliation may still recover it; it is
            // definitely NOT a finished recording.
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason: format!("publication refused: {error}"),
            });
            return Err(error.into());
        }

        let size_bytes = std::fs::metadata(&final_path)
            .map_err(|source| {
                RecordingError::Storage(StorageError::Io {
                    path: final_path.clone(),
                    source,
                })
            })?
            .len();

        events(RecordingEvent::SegmentFinalized {
            final_path,
            started_at: started_wall,
            media_duration,
            size_bytes,
        });
        Ok(size_bytes)
    }

    /// Leaves the segment as a recoverable partial and announces it.
    fn abandon(self, events: &mut dyn FnMut(RecordingEvent), reason: String) {
        let partial_path = self.claim.partial_path().to_path_buf();
        drop(self);
        events(RecordingEvent::SegmentAbandoned {
            partial_path,
            reason,
        });
    }
}

/// Media duration between the segment's first and last video timestamps;
/// `None` when either end carried no timestamp (never guessed).
fn media_duration_of(
    time_base: MediaRational,
    start_media: Option<i64>,
    last_media: Option<i64>,
) -> Option<Duration> {
    let elapsed = last_media?.saturating_sub(start_media?).max(0);
    time_base.duration_of(elapsed)
}

/// Result payload returned by [`RecordingSession::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSummary {
    /// Why the loop ended.
    pub end_reason: RecordingEndReason,
    /// Segments durably finalized **and** published.
    pub finalized_segments: usize,
    /// Sum of published file sizes.
    pub bytes_written: u64,
    /// Packets discarded during startup alignment (before the first
    /// selected video keyframe), across all streams.
    pub discarded_startup_packets: u64,
}

/// A running recorder bound to one source and one recordings layout.
#[derive(Debug)]
pub struct RecordingSession {
    input: MediaInput,
    interrupt: InterruptHandle,
    layout: RecordingsLayout,
    config: RecorderConfig,
    plan: StreamPlan,
    stop: StopFlag,
}

impl RecordingSession {
    /// Opens `source` and validates that a recordable stream plan exists.
    ///
    /// Equivalent to [`RecordingSession::from_input`] with a fresh
    /// interrupt handle owned by the session.
    pub fn open(
        source: &MediaSource,
        layout: RecordingsLayout,
        config: RecorderConfig,
    ) -> Result<Self, RecordingError> {
        let interrupt = InterruptHandle::new();
        let input = MediaInput::open(source, &interrupt)?;
        Self::from_input(input, interrupt, layout, config)
    }

    /// Builds a session on top of an already-opened input.
    ///
    /// Tests use this to pre-drain a fixture past its first GOP before the
    /// startup-alignment logic takes over; production callers normally use
    /// [`RecordingSession::open`].
    pub fn from_input(
        input: MediaInput,
        interrupt: InterruptHandle,
        layout: RecordingsLayout,
        config: RecorderConfig,
    ) -> Result<Self, RecordingError> {
        if config.segment_target.is_zero() {
            return Err(RecordingError::InvalidConfig {
                reason: "segment_target must be greater than zero".to_owned(),
            });
        }
        let streams = input.streams();
        let plan = plan_selection(&streams, config.audio)?;

        Ok(Self {
            input,
            interrupt,
            layout,
            config,
            plan,
            stop: StopFlag::new(),
        })
    }

    /// Container index of the primary video stream (first video stream by
    /// container order — index 0 is never assumed).
    pub fn primary_video_stream_index(&self) -> u32 {
        self.plan.primary_video
    }

    /// The streams mapped into every recorded segment, in output order.
    pub fn selected_streams(&self) -> &[MediaStreamInfo] {
        &self.plan.selection
    }

    /// The session's graceful-stop flag; clone it and call
    /// [`StopFlag::request`] from another thread or from inside the event
    /// callback.
    pub fn stop_flag(&self) -> StopFlag {
        self.stop.clone()
    }

    /// Interrupt handle backing every blocking FFmpeg call of this
    /// session. Cancelling it forces an abort: blocking operations fail
    /// and the active segment is abandoned as a recoverable partial
    /// instead of being finalized.
    pub fn interrupt_handle(&self) -> &InterruptHandle {
        &self.interrupt
    }

    /// Runs the recording until a stop is requested, the source ends, or
    /// an error occurs. Events are emitted synchronously through `events`.
    pub fn run(
        mut self,
        events: &mut dyn FnMut(RecordingEvent),
    ) -> Result<RecordingSummary, RecordingError> {
        events(RecordingEvent::RecordingStarted {
            started_at: local_now(),
        });

        let mut finalized_segments: usize = 0;
        let mut bytes_written: u64 = 0;
        let mut discarded_startup_packets: u64 = 0;
        // `None` while waiting for the opening keyframe.
        let mut active: Option<Segment> = None;
        let mut end_reason = RecordingEndReason::StopRequested;
        let mut failure: Option<RecordingError> = None;

        'run: loop {
            // Graceful stop takes effect between packets: the previous
            // operation completed, and finalization below still runs
            // normally — cancellation semantics are kept apart from stop
            // semantics (§11).
            if self.stop.is_requested() {
                end_reason = RecordingEndReason::StopRequested;
                break 'run;
            }

            match self.input.next_packet() {
                Ok(Some(packet)) => {
                    let metadata = packet.metadata();
                    let is_primary_video = metadata.stream_index == self.plan.primary_video;

                    if active.is_none() {
                        // Startup alignment (§5): discard everything until
                        // the first selected VIDEO keyframe. Leading audio
                        // is dropped rather than synchronized; inter-frame
                        // video cannot start a decodable segment.
                        if is_primary_video && metadata.keyframe {
                            match self.open_segment(&packet, events) {
                                Ok(segment) => active = Some(segment),
                                Err(error) => {
                                    failure = Some(error);
                                    break 'run;
                                }
                            }
                        } else {
                            discarded_startup_packets += 1;
                        }
                        continue 'run;
                    }

                    // Rotation decision (§4): only elapsed MEDIA time arms
                    // the boundary; only a selected VIDEO keyframe fires
                    // it. Audio boundaries never rotate a segment.
                    // (Short-circuit keeps non-video packets from touching
                    // the media clock.)
                    let boundary_keyframe = is_primary_video
                        && match active.as_mut() {
                            Some(segment) => {
                                segment.observe_video(&metadata);
                                segment.rotation_pending && metadata.keyframe
                            }
                            None => false, // unreachable: handled above
                        };

                    if boundary_keyframe {
                        // Close the old segment BEFORE the keyframe…
                        if let Some(finished) = active.take() {
                            match finished.finalize_and_publish(events) {
                                Ok(bytes) => {
                                    bytes_written += bytes;
                                    finalized_segments += 1;
                                }
                                Err(error) => {
                                    failure = Some(error);
                                    break 'run;
                                }
                            }
                        }
                        // …and write the boundary keyframe as the FIRST
                        // packet of the NEW segment.
                        match self.open_segment(&packet, events) {
                            Ok(segment) => active = Some(segment),
                            Err(error) => {
                                failure = Some(error);
                                break 'run;
                            }
                        }
                    } else if let Some(segment) = active.as_mut() {
                        // Unselected streams reach the muxer too and are
                        // skipped deliberately there; exact accounting is
                        // kept for the primary video only.
                        if let Err(error) = segment.muxer.write_packet(&packet) {
                            failure = Some(error.into());
                            break 'run;
                        }
                        if is_primary_video {
                            segment.video_packets += 1;
                        }
                    }
                }
                Ok(None) => {
                    end_reason = RecordingEndReason::EndOfStream;
                    break 'run;
                }
                Err(error) => {
                    end_reason = RecordingEndReason::SourceError;
                    failure = Some(error.into());
                    break 'run;
                }
            }
        }

        // ---- Teardown ---------------------------------------------------
        //
        // Forced cancellation must not attempt finalization: the interrupt
        // fired because blocking I/O had to stop, so trailer/flush would
        // fail identically. Everything else gets a best-effort durable
        // finalize of whatever healthy content the active segment holds.
        let cancelled = self.interrupt.is_cancelled()
            || matches!(
                &failure,
                Some(RecordingError::Media(media)) if media.is_interrupted()
            );

        if let Some(segment) = active.take() {
            if cancelled {
                segment.abandon(
                    events,
                    "forced cancellation: partial left for recovery".to_owned(),
                );
            } else {
                match segment.finalize_and_publish(events) {
                    Ok(bytes) => {
                        bytes_written += bytes;
                        finalized_segments += 1;
                    }
                    Err(close_error) => {
                        // finalize_and_publish announced the abandonment;
                        // the close failure becomes the reported error only
                        // when there was no original cause.
                        if failure.is_none() {
                            failure = Some(close_error);
                        }
                    }
                }
            }
        }

        if let Some(error) = failure {
            return Err(error);
        }

        events(RecordingEvent::RecordingStopped { finalized_segments });
        Ok(RecordingSummary {
            end_reason,
            finalized_segments,
            bytes_written,
            discarded_startup_packets,
        })
    }

    /// Claims a slot, opens the muxer on the claimed partial, and writes
    /// `first_packet` (always a selected video keyframe) as its first
    /// packet.
    ///
    /// Failure modes follow the recovery contract: a failed *claim*
    /// creates nothing and propagates; a failure after the claim leaves
    /// the (empty or header-only) `.partial.mkv` in place — recoverable,
    /// never publishable — and announces the abandonment.
    fn open_segment(
        &mut self,
        first_packet: &FfmpegPacket,
        events: &mut dyn FnMut(RecordingEvent),
    ) -> Result<Segment, RecordingError> {
        let started_wall = local_now();
        let claim = self
            .layout
            .claim_segment(&self.config.camera, started_wall)?;

        let selection = self.plan.selection.clone();
        let opened = MatroskaMuxer::create_with_selection(
            &mut self.input,
            claim.partial_path(),
            &self.interrupt,
            |info| {
                selection
                    .iter()
                    .any(|s| s.stream_index == info.stream_index)
            },
        );

        let muxer = match opened {
            Ok(muxer) => muxer,
            Err(error) => {
                events(RecordingEvent::SegmentAbandoned {
                    partial_path: claim.partial_path().to_path_buf(),
                    reason: format!("muxer could not be opened: {error}"),
                });
                return Err(error.into());
            }
        };

        events(RecordingEvent::SegmentStarted {
            partial_path: claim.partial_path().to_path_buf(),
            final_path: claim.final_path().to_path_buf(),
            started_at: started_wall,
        });

        let mut segment = Segment {
            claim,
            muxer,
            started_wall,
            video_time_base: self.plan.video_time_base,
            target_ticks: target_ticks_in(self.config.segment_target, self.plan.video_time_base),
            start_media: None,
            last_media: None,
            video_packets: 0,
            rotation_pending: false,
        };

        // The startup/boundary keyframe belongs to THIS segment (§4).
        let metadata = first_packet.metadata();
        segment.observe_video(&metadata);
        if let Err(error) = segment.muxer.write_packet(first_packet) {
            let reason = format!("opening keyframe write failed: {error}");
            let partial_path = segment.claim.partial_path().to_path_buf();
            drop(segment);
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason,
            });
            return Err(error.into());
        }
        segment.video_packets = 1;

        Ok(segment)
    }
}

/// Picks the primary video stream and the muxed selection.
///
/// Deliberate policy (§8): the first video stream by container order is
/// primary (index 0 is never assumed); under [`AudioPolicy::CopyAll`] all
/// audio streams are copied alongside; additional video, data, subtitle
/// and unknown streams are ignored — their packets are skipped explicitly
/// by the muxer's mapping.
fn plan_selection(
    streams: &[MediaStreamInfo],
    audio: AudioPolicy,
) -> Result<StreamPlan, RecordingError> {
    let video = streams
        .iter()
        .find(|stream| stream.media_type == MediaType::Video)
        .ok_or(RecordingError::NoVideoStream {
            stream_count: streams.len(),
        })?;

    let time_base = video
        .time_base
        .filter(|tb| tb.num > 0 && tb.den > 0)
        .ok_or(RecordingError::UnusableTimeBase {
            stream_index: video.stream_index,
        })?;

    let mut selection = vec![video.clone()];
    if audio == AudioPolicy::CopyAll {
        selection.extend(
            streams
                .iter()
                .filter(|stream| stream.media_type == MediaType::Audio)
                .cloned(),
        );
    }

    Ok(StreamPlan {
        primary_video: video.stream_index,
        video_time_base: time_base,
        selection,
    })
}

/// Segment target expressed in video time-base ticks (i128 intermediate;
/// saturates instead of overflowing).
fn target_ticks_in(target: Duration, time_base: MediaRational) -> i64 {
    let micros = i128::try_from(target.as_micros()).unwrap_or(i128::MAX);
    let ticks = micros * i128::from(time_base.den) / (1_000_000 * i128::from(time_base.num.max(1)));
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn local_now() -> NaiveDateTime {
    Local::now().naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nian_domain::MediaStreamInfo;

    fn stream(
        index: u32,
        media_type: MediaType,
        time_base: Option<MediaRational>,
    ) -> MediaStreamInfo {
        MediaStreamInfo {
            stream_index: index,
            media_type,
            codec_name: "test".to_owned(),
            width: None,
            height: None,
            sample_rate: None,
            time_base,
        }
    }

    fn tb(num: i32, den: i32) -> Option<MediaRational> {
        // Struct literal on purpose: the negative cases must be constructible
        // even though `MediaRational::new` would reject them.
        Some(MediaRational { num, den })
    }

    #[test]
    fn plan_picks_first_video_stream_without_assuming_index_zero() {
        // Audio first, two video streams, data last — the FIRST video
        // stream (index 1) is primary; the second video and the data
        // stream are ignored deliberately.
        let streams = vec![
            stream(0, MediaType::Audio, tb(1, 48000)),
            stream(1, MediaType::Video, tb(1, 90000)),
            stream(2, MediaType::Video, tb(1, 90000)),
            stream(3, MediaType::Data, None),
        ];

        let plan = plan_selection(&streams, AudioPolicy::CopyAll).unwrap();
        assert_eq!(plan.primary_video, 1);
        assert_eq!(plan.video_time_base, MediaRational { num: 1, den: 90000 });
        let selected_indices: Vec<u32> = plan.selection.iter().map(|s| s.stream_index).collect();
        assert_eq!(selected_indices, vec![1, 0], "video first, then audio");
    }

    #[test]
    fn audio_policy_exclude_selects_video_only() {
        let streams = vec![
            stream(0, MediaType::Video, tb(1, 1000)),
            stream(1, MediaType::Audio, tb(1, 48000)),
        ];
        let plan = plan_selection(&streams, AudioPolicy::Exclude).unwrap();
        assert_eq!(plan.selection.len(), 1);
        assert_eq!(plan.selection[0].media_type, MediaType::Video);
    }

    #[test]
    fn missing_or_invalid_video_time_base_is_rejected_not_guessed() {
        for bad in [None, tb(0, 1000), tb(1, 0), tb(-1, 1000)] {
            let streams = vec![stream(0, MediaType::Video, bad)];
            match plan_selection(&streams, AudioPolicy::CopyAll) {
                Err(RecordingError::UnusableTimeBase { stream_index }) => {
                    assert_eq!(stream_index, 0);
                }
                other => panic!("expected UnusableTimeBase for {bad:?}, got {other:?}"),
            }
        }

        // No video at all stays a distinct error.
        let streams = vec![stream(0, MediaType::Audio, tb(1, 48000))];
        match plan_selection(&streams, AudioPolicy::CopyAll) {
            Err(RecordingError::NoVideoStream { stream_count }) => assert_eq!(stream_count, 1),
            other => panic!("expected NoVideoStream, got {other:?}"),
        }
    }

    #[test]
    fn segment_target_converts_into_stream_ticks_and_saturates() {
        let ticks = target_ticks_in(Duration::from_secs(5), MediaRational::new(1, 1000).unwrap());
        assert_eq!(ticks, 5_000);

        // Extreme time bases saturate instead of overflowing or panicking.
        let huge = target_ticks_in(
            Duration::from_secs(u64::from(u32::MAX)),
            MediaRational::new(1, 1).unwrap(),
        );
        assert!(huge > 0);
        let precise = target_ticks_in(
            Duration::from_millis(1500),
            MediaRational::new(1, 30_000).unwrap(),
        );
        assert_eq!(precise, 45_000);
        // A sub-tick target degenerates to zero ticks (rotation armed at
        // the first timestamp) instead of inventing resolution.
        let sub_tick = target_ticks_in(
            Duration::from_millis(1),
            MediaRational::new(1000, 1).unwrap(),
        );
        assert_eq!(sub_tick, 0);
    }

    #[test]
    fn media_duration_requires_both_ends_of_the_segment_clock() {
        let time_base = MediaRational::new(1, 1000).unwrap();
        assert_eq!(media_duration_of(time_base, None, Some(5_000)), None);
        assert_eq!(media_duration_of(time_base, Some(1_000), None), None);
        assert_eq!(
            media_duration_of(time_base, Some(1_000), Some(6_000)),
            Some(Duration::from_secs(5))
        );
        // Backward jump never produces a negative duration.
        assert_eq!(
            media_duration_of(time_base, Some(6_000), Some(1_000)),
            Some(Duration::ZERO)
        );
    }
}
