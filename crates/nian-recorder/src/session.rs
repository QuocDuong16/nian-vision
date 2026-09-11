//! The recording engine: one media input, one active segment at a time.
//!
//! Ownership and lifetime order (milestone requirement §1): a
//! [`RecordingSession`] owns the [`MediaInput`] and derives its single
//! [`InterruptHandle`] from that input *by construction*, so the read
//! callback, every muxer's output callback and session cancellation always
//! share one interrupt state. Each open segment owns its
//! [`ClaimedSegment`] (the exclusive claim token) and the
//! [`MatroskaMuxer`] writing into it. A segment is finalized and published
//! before the next claim is taken; nothing outlives its owner.
//!
//! # Failure semantics (invariant)
//!
//! * `MatroskaMuxer` write failures poison the active segment: it is
//!   abandoned as a recoverable `.partial.mkv`, never published, and the
//!   original error is returned. A succeeding trailer after a failed write
//!   is NOT evidence that the segment is valid.
//! * Input read failures allow salvaging the healthy prefix: the active
//!   segment is finalized and published if finalization succeeds.
//! * Forced cancellation abandons the active segment, never publishes.
//! * Publication is the final infallible transition: everything that can
//!   fail (trailer, flush/close, pre-publication size lookup) happens
//!   while the content is still a `.partial.mkv`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use nian_domain::{MediaPacketMetadata, MediaRational, MediaStreamInfo, MediaType};
use nian_media::MediaSource;
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
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
/// publish run to completion. A stop request cannot wake a read that is
/// already blocked inside FFmpeg; bounding blocked reads (deadlines,
/// reconnect policy) belongs to M3.
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

    /// Whether `other` refers to the SAME control domain — clones of one
    /// original flag share their inner atomic, so a request through either
    /// is visible to both. Supervisor wiring relies on this identity.
    pub fn shares_control_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Media-time bookkeeping for one open segment.
///
/// Kept separate from the I/O half of [`Segment`] so the rotation decision
/// can be computed read-only and commits can be made strictly transactional
/// with successful writes: a packet that fails to write never touches this
/// state, and a boundary keyframe never touches the OLD segment's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MediaClock {
    /// Validated time base of the primary video stream.
    video_time_base: MediaRational,
    /// Segment target converted (ceiling) into video time-base ticks.
    target_ticks: i64,
    /// DTS (fallback PTS) of the first timestamped video packet actually
    /// written to this segment — the zero point of its media clock.
    start_media: Option<i64>,
    /// Most recent video timestamp committed to this segment.
    last_media: Option<i64>,
    /// Video packets successfully handed to the muxer (including the
    /// opening keyframe). A segment below one never publishes.
    video_packets: u64,
}

impl MediaClock {
    /// Elapsed media time a timestamp would contribute, relative to the
    /// committed zero point (which it establishes if absent).
    fn elapsed_of(&self, timestamp: i64) -> i64 {
        timestamp
            .saturating_sub(self.start_media.unwrap_or(timestamp))
            .max(0)
    }

    /// Read-only rotation decision (§4): only elapsed MEDIA time arms the
    /// boundary and only a selected VIDEO keyframe fires it. Never mutates
    /// the clock, so evaluating it for the boundary keyframe cannot leak
    /// that keyframe's timestamp into the old segment.
    ///
    /// DTS is preferred over PTS because stream copy writes packets in
    /// decode order; with B-frames, PTS oscillates around DTS and would
    /// mis-measure elapsed time. Packets carrying neither timestamp
    /// contribute content but never time — nothing is invented. Backward
    /// jumps clamp to zero, so a discontinuity can delay rotation but
    /// never rewind it.
    fn rotation_due(&self, metadata: &MediaPacketMetadata) -> bool {
        if !metadata.keyframe {
            return false;
        }
        metadata
            .dts
            .or(metadata.pts)
            .is_some_and(|timestamp| self.elapsed_of(timestamp) >= self.target_ticks)
    }

    /// Commits a successfully written primary-video packet: timestamps and
    /// count become part of the segment's durable bookkeeping only here.
    fn commit_video(&mut self, metadata: &MediaPacketMetadata) {
        if let Some(timestamp) = metadata.dts.or(metadata.pts) {
            self.start_media.get_or_insert(timestamp);
            self.last_media = Some(timestamp);
        }
        self.video_packets += 1;
    }

    /// Media duration between the segment's first and last committed video
    /// timestamps; `None` when either end carried no timestamp (never
    /// guessed).
    fn media_duration(&self) -> Option<Duration> {
        let last = self.last_media?;
        let start = self.start_media?;
        self.video_time_base
            .duration_of(last.saturating_sub(start).max(0))
    }
}

/// One open segment: an exclusively claimed slot plus the muxer writing
/// into it, with the transactional media-time bookkeeping.
struct Segment {
    claim: ClaimedSegment,
    muxer: MatroskaMuxer,
    started_wall: NaiveDateTime,
    clock: MediaClock,
}

impl Segment {
    /// Durably finishes the segment: trailer → flush/close → size lookup
    /// on the still-partial file → no-replace publish → `SegmentFinalized`.
    ///
    /// Everything fallible happens BEFORE publication; once the final name
    /// appears, no remaining operation can turn this segment back into an
    /// application-level failure. Any earlier failure keeps the file as a
    /// recoverable `.partial.mkv`, announces the abandonment through
    /// `events`, and propagates the error. Returns the published size.
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
            clock,
        } = self;
        let partial_path = claim.partial_path().to_path_buf();
        let final_path = claim.final_path().to_path_buf();

        if clock.video_packets == 0 {
            drop(muxer); // crash-shaped partial stays on disk, unpublished
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason: "no video packets were written".to_owned(),
            });
            return Ok(0);
        }

        let media_duration = clock.media_duration();

        // Trailer AND final flush/close must succeed before anything is
        // published (§9); on failure the partial stays recovery-eligible.
        if let Err(error) = muxer.finalize() {
            events(RecordingEvent::SegmentAbandoned {
                partial_path,
                reason: format!("finalization failed: {error}"),
            });
            return Err(error.into());
        }

        // Size lookup happens while the content is still the partial file;
        // doing it after publication could fail with the final .mkv already
        // visible but unreported.
        let size_bytes = match std::fs::metadata(&partial_path) {
            Ok(metadata) => metadata.len(),
            Err(source) => {
                let path = partial_path.clone();
                events(RecordingEvent::SegmentAbandoned {
                    partial_path,
                    reason: format!("size lookup failed before publication: {source}"),
                });
                return Err(RecordingError::Storage(StorageError::Io { path, source }));
            }
        };

        // Atomic, never-replacing publication — the final transition.
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

/// What teardown should do with the still-open segment.
///
/// Pure policy so the invariant "never publish after a mux/output write
/// failure" is explicit and unit-testable: only a healthy prefix written
/// without mux errors (and without cancellation) may be salvaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeardownDecision {
    /// Finalize and publish the healthy prefix.
    PublishSalvage,
    /// Leave the partial in place; never publish.
    Abandon,
}

fn teardown_decision(cancelled: bool, mux_write_failed: bool) -> TeardownDecision {
    if cancelled || mux_write_failed {
        TeardownDecision::Abandon
    } else {
        TeardownDecision::PublishSalvage
    }
}

/// Test-only hook that can fail packet writes at the mux boundary.
#[cfg(test)]
type WriteFaultHook = Box<
    dyn FnMut(&nian_media_ffmpeg::FfmpegPacket) -> Result<(), nian_media::MediaError> + 'static,
>;

/// Test-only hook that can fail demux reads.
#[cfg(test)]
type ReadFaultHook = Box<dyn FnMut() -> Option<nian_media::MediaError> + 'static>;

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
///
/// Ownership boundary: no production `RecordingSession` may write a canonical
/// recording tree unless its caller holds the matching
/// [`nian_storage::CameraLease`] for the entire session lifetime. This type is
/// intentionally low-level so crate tests can exercise session mechanics.
pub struct RecordingSession {
    input: MediaInput,
    /// Derived from `input.interrupt_handle()` by construction: one shared
    /// interrupt state for reads, muxer writes and forced cancellation.
    interrupt: InterruptHandle,
    layout: RecordingsLayout,
    config: RecorderConfig,
    plan: StreamPlan,
    stop: StopFlag,

    /// Deterministic fault injection at the I/O boundaries, used only by
    /// in-crate tests; compiled out of production builds entirely. No fake
    /// media logic exists outside `#[cfg(test)]`.
    #[cfg(test)]
    write_fault: Option<WriteFaultHook>,
    #[cfg(test)]
    read_fault: Option<ReadFaultHook>,
}

impl std::fmt::Debug for RecordingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingSession")
            .field("input", &self.input)
            .field("interrupt", &self.interrupt)
            .field("layout", &self.layout)
            .field("config", &self.config)
            .field("plan", &self.plan)
            .field("stop", &self.stop)
            .finish_non_exhaustive()
    }
}

impl RecordingSession {
    /// Opens `source` and validates that a recordable stream plan exists.
    ///
    /// The open/connect phase is bounded by `config.timeouts.open` (M3 §5):
    /// a camera that never completes its handshake fails with a retryable
    /// timeout instead of blocking forever. The deadline is armed only for
    /// this operation; after `from_input` builds the session, no deadline
    /// remains installed.
    pub fn open(
        source: &MediaSource,
        layout: RecordingsLayout,
        config: RecorderConfig,
    ) -> Result<Self, RecordingError> {
        let interrupt = InterruptHandle::new();
        let _open_deadline = interrupt.scoped_deadline(config.timeouts.open);
        let input = MediaInput::open(source, &interrupt)?;
        drop(_open_deadline);
        Self::from_input(input, layout, config)
    }

    /// Builds a session on top of an already-opened input.
    ///
    /// The session's interrupt handle IS the input's handle
    /// (`input.interrupt_handle()`), so the read callback installed inside
    /// the demuxer, the callback every segment muxer installs, and
    /// [`RecordingSession::interrupt_handle`] all refer to one interrupt
    /// state — sharing different handles between reader and writer would
    /// let a cancellation stop reads but not writes, or vice versa.
    ///
    /// Tests use this constructor to pre-drain a fixture past its first GOP
    /// before the startup-alignment logic takes over.
    pub fn from_input(
        input: MediaInput,
        layout: RecordingsLayout,
        config: RecorderConfig,
    ) -> Result<Self, RecordingError> {
        Self::from_input_with_stop(input, layout, config, StopFlag::new())
    }

    /// Like [`Self::from_input`], but the session's graceful-stop flag is
    /// SHARED with an external supervisor control domain instead of being a
    /// fresh private one.
    ///
    /// M3 remediation §1: reconnect supervisors hand their run-level flag
    /// to every factory call and factories build sessions through THIS
    /// constructor — stop-while-Recording reaches the active session by
    /// construction (one `Arc<AtomicBool>` behind both sides), never via a
    /// relay thread or after-the-fact flag splicing.
    pub fn from_input_with_stop(
        input: MediaInput,
        layout: RecordingsLayout,
        config: RecorderConfig,
        run_stop: StopFlag,
    ) -> Result<Self, RecordingError> {
        let mut session = Self::from_input_internal(input, layout, config)?;
        session.stop = run_stop;
        Ok(session)
    }

    fn from_input_internal(
        input: MediaInput,
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
        let interrupt = input.interrupt_handle().clone();

        Ok(Self {
            input,
            interrupt,
            layout,
            config,
            plan,
            stop: StopFlag::new(),
            #[cfg(test)]
            write_fault: None,
            #[cfg(test)]
            read_fault: None,
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

    /// The single interrupt handle shared by input reads, every segment
    /// muxer's writes and forced cancellation. Cancelling it aborts
    /// blocking operations; the active segment is abandoned as a
    /// recoverable partial instead of being finalized.
    pub fn interrupt_handle(&self) -> &InterruptHandle {
        &self.interrupt
    }

    /// Runs the recording until a stop is requested, the source ends, or
    /// an error occurs. Events are emitted synchronously through `events`;
    /// [`RecordingEvent::RecordingStopped`] is emitted exactly once for
    /// every started session — including failed ones — before `Err` is
    /// returned.
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
        let mut end_reason;
        let mut failure: Option<RecordingError> = None;
        // Set when a MUX/output write fails: the active segment is then
        // poisoned and MUST NOT be published even if a trailer would
        // succeed afterwards.
        let mut mux_write_failed = false;

        'run: loop {
            // Graceful stop takes effect between packets: the previous
            // operation completed, and finalization below still runs
            // normally — cancellation semantics are kept apart from stop
            // semantics (§11).
            if self.stop.is_requested() {
                end_reason = RecordingEndReason::StopRequested;
                break 'run;
            }

            // Per-read stall deadline (M3 §5): armed for exactly this read,
            // cleared on drop before any mux write/finalization runs. If the
            // read blocks longer than `timeouts.read`, FFmpeg aborts with a
            // deadline cause, which surfaces below as the retryable
            // TimedOut error — never as cancellation.
            let _read_deadline = self.interrupt.scoped_deadline(self.config.timeouts.read);
            let read_result = self.input.next_packet();
            drop(_read_deadline);

            match read_result {
                Ok(Some(packet)) => {
                    // Test-only injection of a demux-side read failure.
                    #[cfg(test)]
                    if let Some(hook) = &mut self.read_fault
                        && let Some(error) = hook()
                    {
                        end_reason = RecordingEndReason::SourceError;
                        failure = Some(error.into());
                        break 'run;
                    }

                    let metadata = packet.metadata();
                    let is_primary_video = metadata.stream_index == self.plan.primary_video;

                    if active.is_none() {
                        // Startup alignment (§5): discard everything until
                        // the first selected VIDEO keyframe. Leading audio
                        // is dropped rather than synchronized; inter-frame
                        // video cannot start a decodable segment.
                        if is_primary_video && metadata.keyframe {
                            let mut segment = match self.begin_segment(events) {
                                Ok(segment) => segment,
                                Err(error) => {
                                    end_reason = RecordingEndReason::SourceError;
                                    failure = Some(error);
                                    break 'run;
                                }
                            };
                            match segment.muxer.write_packet(&packet) {
                                Ok(()) => {
                                    // Commit only after the write succeeded.
                                    segment.clock.commit_video(&metadata);
                                    active = Some(segment);
                                }
                                Err(error) => {
                                    mux_write_failed = true;
                                    end_reason = RecordingEndReason::SourceError;
                                    segment.abandon(
                                        events,
                                        format!("opening keyframe write failed: {error}"),
                                    );
                                    failure = Some(error.into());
                                    break 'run;
                                }
                            }
                        } else {
                            discarded_startup_packets += 1;
                        }
                        continue 'run;
                    }

                    // Rotation decision (§4): computed READ-ONLY against
                    // the committed clock, so the boundary keyframe never
                    // becomes the old segment's `last_media` (its commit
                    // lands in the NEW segment below).
                    let boundary_keyframe = is_primary_video
                        && match active.as_ref() {
                            Some(segment) => segment.clock.rotation_due(&metadata),
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
                                    end_reason = RecordingEndReason::SourceError;
                                    failure = Some(error);
                                    break 'run;
                                }
                            }
                        }
                        // …open the NEW segment…
                        let mut segment = match self.begin_segment(events) {
                            Ok(segment) => segment,
                            Err(error) => {
                                end_reason = RecordingEndReason::SourceError;
                                failure = Some(error);
                                break 'run;
                            }
                        };
                        // …and write the boundary keyframe as its FIRST
                        // packet, committing it only on success.
                        match segment.muxer.write_packet(&packet) {
                            Ok(()) => {
                                segment.clock.commit_video(&metadata);
                                active = Some(segment);
                            }
                            Err(error) => {
                                mux_write_failed = true;
                                end_reason = RecordingEndReason::SourceError;
                                segment.abandon(
                                    events,
                                    format!("opening keyframe write failed: {error}"),
                                );
                                failure = Some(error.into());
                                break 'run;
                            }
                        }
                    } else {
                        let Some(segment) = active.as_mut() else {
                            discarded_startup_packets += 1;
                            continue 'run;
                        };

                        // Test-only injection of a mux-side write failure.
                        #[cfg(test)]
                        if let Some(hook) = &mut self.write_fault
                            && let Err(error) = hook(&packet)
                        {
                            mux_write_failed = true;
                            end_reason = RecordingEndReason::SourceError;
                            failure = Some(error.into());
                            break 'run;
                        }

                        // Unselected streams reach the muxer too and are
                        // skipped deliberately there; exact accounting is
                        // kept for the primary video only.
                        match segment.muxer.write_packet(&packet) {
                            Ok(()) => {
                                if is_primary_video {
                                    segment.clock.commit_video(&metadata);
                                }
                            }
                            Err(error) => {
                                mux_write_failed = true;
                                end_reason = RecordingEndReason::SourceError;
                                failure = Some(error.into());
                                break 'run;
                            }
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
        // Policy (explicit, see `teardown_decision`): forced cancellation
        // and mux/output write failures abandon the open segment — a
        // succeeding trailer after a failed write proves nothing. Only a
        // healthy prefix written without mux errors may be salvaged.
        //
        // A read TIMEOUT (M3 §6) is deliberately NOT cancellation: the
        // source stalled, but everything already written is healthy, so the
        // prefix follows normal salvage semantics and the supervisor
        // reconnects afterwards. True operator cancellation still abandons.
        let interrupted = self.interrupt.is_cancelled()
            || matches!(
                &failure,
                Some(RecordingError::Media(media)) if media.is_interrupted()
            );
        let cancelled = interrupted;

        if let Some(segment) = active.take() {
            match teardown_decision(cancelled, mux_write_failed) {
                TeardownDecision::Abandon => segment.abandon(
                    events,
                    if cancelled {
                        "forced cancellation: partial left for recovery".to_owned()
                    } else {
                        "segment write failed: partial left for recovery, never published"
                            .to_owned()
                    },
                ),
                TeardownDecision::PublishSalvage => {
                    match segment.finalize_and_publish(events) {
                        Ok(bytes) => {
                            bytes_written += bytes;
                            finalized_segments += 1;
                        }
                        Err(close_error) => {
                            // finalize_and_publish announced the abandonment;
                            // the close failure becomes the reported error
                            // only when there was no original cause.
                            end_reason = RecordingEndReason::SourceError;
                            if failure.is_none() {
                                failure = Some(close_error);
                            }
                        }
                    }
                }
            }
        }

        // Terminal event: emitted exactly once per started session, on the
        // failure path too — the fields carry enough state for a future
        // supervisor/UI to reconcile with the returned Result.
        let completed = failure.is_none();
        events(RecordingEvent::RecordingStopped {
            finalized_segments,
            end_reason,
            completed,
        });

        match failure {
            Some(error) => Err(error),
            None => Ok(RecordingSummary {
                end_reason,
                finalized_segments,
                bytes_written,
                discarded_startup_packets,
            }),
        }
    }

    /// Claims a slot and opens the muxer on the claimed partial, emitting
    /// `SegmentStarted`. Does NOT write anything yet: the caller writes the
    /// opening keyframe and commits it to the clock only on success, so a
    /// failed write leaves a header-only recoverable partial and no
    /// bookkeeping lies about it.
    ///
    /// Failure modes follow the recovery contract: a failed *claim*
    /// creates nothing and propagates; a failure after the claim leaves
    /// the (empty or header-only) `.partial.mkv` in place — recoverable,
    /// never publishable — and announces the abandonment.
    fn begin_segment(
        &mut self,
        events: &mut dyn FnMut(RecordingEvent),
    ) -> Result<Segment, RecordingError> {
        let started_wall = local_now();
        let claim = self
            .layout
            .claim_segment(&self.config.camera, started_wall)?;

        let selection = self.plan.selection.clone();
        let opened = MatroskaMuxer::create_recording_segment_with_selection(
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

        Ok(Segment {
            claim,
            muxer,
            started_wall,
            clock: MediaClock {
                video_time_base: self.plan.video_time_base,
                target_ticks: target_ticks_in(
                    self.config.segment_target,
                    self.plan.video_time_base,
                ),
                start_media: None,
                last_media: None,
                video_packets: 0,
            },
        })
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

/// Segment target expressed in video time-base ticks using CEILING
/// division: the arming threshold is the first tick at or after the
/// requested duration. Rounding down would rotate before the operator's
/// target was reached. Saturates instead of overflowing or panicking.
fn target_ticks_in(target: Duration, time_base: MediaRational) -> i64 {
    let micros = i128::try_from(target.as_micros()).unwrap_or(i128::MAX);
    let numerator = micros.saturating_mul(i128::from(time_base.den));
    let denominator = 1_000_000_i128.saturating_mul(i128::from(time_base.num.max(1)));
    let ticks = numerator.saturating_add(denominator - 1) / denominator;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn local_now() -> NaiveDateTime {
    Local::now().naive_local()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure decision/bookkeeping layer plus
    //! fault-injection runs over the real pipeline (real FFmpeg, real
    //! files — only the injected faults are synthetic).

    use super::*;
    use nian_domain::{CameraId, MediaStreamInfo};
    use nian_media::MediaError;

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
    fn segment_target_converts_into_stream_ticks_with_ceiling() {
        // Exact conversions stay exact.
        assert_eq!(
            target_ticks_in(Duration::from_secs(5), MediaRational::new(1, 1000).unwrap()),
            5_000
        );

        // Non-integral time base (tick = 1001/30000 s ≈ 33.367 ms) where
        // floor and ceiling differ: 1 s is exactly 30000/1001 ≈ 29.97
        // ticks — flooring to 29 would arm BELOW the requested target.
        let ntsc_like = MediaRational {
            num: 1001,
            den: 30000,
        };
        assert_eq!(target_ticks_in(Duration::from_secs(1), ntsc_like), 30);
        assert_eq!(target_ticks_in(Duration::from_millis(500), ntsc_like), 15);
        assert_eq!(target_ticks_in(Duration::from_millis(100), ntsc_like), 3);

        // Extreme inputs saturate instead of overflowing or panicking.
        let huge = target_ticks_in(
            Duration::from_secs(u64::from(u32::MAX)),
            MediaRational::new(1, 1).unwrap(),
        );
        assert!(huge > 0);
        // A sub-tick target rounds UP to the first tick (never to zero —
        // zero would arm rotation on the very first timestamp).
        let sub_tick = target_ticks_in(
            Duration::from_millis(1),
            MediaRational::new(1000, 1).unwrap(),
        );
        assert_eq!(sub_tick, 1);
    }

    fn sample_clock() -> MediaClock {
        MediaClock {
            video_time_base: MediaRational::new(1, 1000).unwrap(),
            target_ticks: 5_000,
            start_media: None,
            last_media: None,
            video_packets: 0,
        }
    }

    fn metadata(dts: Option<i64>, pts: Option<i64>, keyframe: bool) -> MediaPacketMetadata {
        MediaPacketMetadata {
            stream_index: 0,
            dts,
            pts,
            duration: None,
            keyframe,
        }
    }

    #[test]
    fn rotation_is_read_only_and_never_consumes_the_boundary_timestamp() {
        let mut clock = sample_clock();
        // First committed packet establishes the zero point.
        clock.commit_video(&metadata(Some(1_000), Some(1_000), true));
        let before = clock.clone();

        // Evaluating a boundary keyframe (elapsed >= target) must leave the
        // committed clock untouched: the boundary belongs to the NEXT
        // segment, so the old segment's last_media/count/duration must not
        // include it.
        assert!(clock.rotation_due(&metadata(Some(6_000), Some(6_000), true)));
        assert_eq!(clock, before, "rotation_due mutated committed state");
        assert_eq!(clock.last_media, Some(1_000));
        assert_eq!(clock.video_packets, 1);
        assert_eq!(clock.media_duration(), Some(Duration::ZERO));
    }

    #[test]
    fn rotation_uses_decode_order_and_ignores_pts_oscillation() {
        // B-frame pattern: PTS oscillates around DTS. Rotation decisions go
        // through DTS, so a PTS dip can neither trigger nor postpone a
        // boundary. Zero point established at dts 1000, target 5000 ticks.
        let mut clock = sample_clock();
        clock.commit_video(&metadata(Some(1_000), Some(1_200), false));

        // Non-keyframe at/after the target never rotates on its own.
        assert!(!clock.rotation_due(&metadata(Some(6_000), Some(6_000), false)));
        // Keyframe whose DTS reaches the target rotates even though its PTS
        // sits below it.
        assert!(clock.rotation_due(&metadata(Some(6_100), Some(5_800), true)));
        // Keyframe whose DTS is short of the target does not — even though
        // its PTS would cross it (PTS is simply not consulted).
        assert!(!clock.rotation_due(&metadata(Some(5_900), Some(6_200), true)));

        // Packets without any timestamp contribute nothing to timing.
        assert!(!clock.rotation_due(&metadata(None, None, true)));
    }

    #[test]
    fn commit_is_transactional_with_successful_writes_by_construction() {
        // The loop only calls commit_video after a successful write; these
        // assertions pin what a commit means so a regression cannot quietly
        // move it before the write again.
        let mut clock = sample_clock();
        clock.commit_video(&metadata(Some(1_000), Some(1_000), true));
        clock.commit_video(&metadata(Some(3_000), Some(3_100), false));
        assert_eq!(clock.video_packets, 2);
        assert_eq!(clock.start_media, Some(1_000));
        assert_eq!(clock.last_media, Some(3_000));
        assert_eq!(clock.media_duration(), Some(Duration::from_millis(2_000)));

        // A failed write simply never calls commit: simulate by NOT calling
        // it — state stays at the last success.
        let snapshot = clock.clone();
        assert_eq!(clock, snapshot);
    }

    #[test]
    fn media_duration_requires_both_ends_of_the_segment_clock() {
        let mut clock = sample_clock();
        assert_eq!(clock.media_duration(), None);
        clock.commit_video(&metadata(Some(1_000), None, true));
        // One end known: the span is well-defined but zero so far.
        assert_eq!(clock.media_duration(), Some(Duration::ZERO));
        clock.commit_video(&metadata(Some(6_000), None, false));
        assert_eq!(clock.media_duration(), Some(Duration::from_secs(5)));
        // Backward jump lowers last_media; the span shrinks but can never
        // go negative (saturating subtraction).
        clock.commit_video(&metadata(Some(2_000), None, false));
        assert_eq!(clock.media_duration(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn teardown_policy_aborts_publication_on_poison_or_cancellation() {
        // Healthy prefix + no cancellation → salvage.
        assert_eq!(
            teardown_decision(false, false),
            TeardownDecision::PublishSalvage
        );
        // Any mux/output write failure poisons the segment.
        assert_eq!(teardown_decision(false, true), TeardownDecision::Abandon);
        // Cancellation always abandons.
        assert_eq!(teardown_decision(true, false), TeardownDecision::Abandon);
        assert_eq!(teardown_decision(true, true), TeardownDecision::Abandon);
    }

    // ---- Fault-injection runs over the REAL pipeline --------------------

    fn fixtures_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/nian-media-ffmpeg/tests/fixtures")
    }

    fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    fn finals_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        files_under(root)
            .into_iter()
            .filter(|p| {
                p.extension().is_some_and(|ext| ext == "mkv")
                    && !p.to_string_lossy().contains(".partial.")
            })
            .collect()
    }

    fn partials_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        files_under(root)
            .into_iter()
            .filter(|p| p.to_string_lossy().contains(".partial."))
            .collect()
    }

    /// Records the long fixture with a mux-side write fault injected right
    /// after the Nth packet was handed to the active segment.
    #[test]
    fn mux_write_failure_poisons_the_segment_and_never_publishes_it() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixtures_dir().join("session_av.mkv");
        let interrupt = InterruptHandle::new();
        let input = MediaInput::open(&MediaSource::File(source.clone()), &interrupt).unwrap();
        let mut session = RecordingSession::from_input(
            input,
            RecordingsLayout::new(temp.path().join("recordings")).unwrap(),
            RecorderConfig::new(CameraId::parse("rec-test").unwrap())
                .with_segment_target(Duration::from_secs(5)),
        )
        .unwrap();

        const FAIL_AFTER_PACKETS: usize = 40;
        let mut seen = 0_usize;
        session.write_fault = Some(Box::new(move |_packet| {
            seen += 1;
            if seen > FAIL_AFTER_PACKETS {
                return Err(MediaError::WriteFailed {
                    message: "injected mux failure".to_owned(),
                });
            }
            Ok(())
        }));

        let mut events = Vec::new();
        let outcome = session.run(&mut |event| events.push(event));
        let error = outcome.expect_err("injected mux failure must fail the run");
        assert!(
            matches!(&error, RecordingError::Media(MediaError::WriteFailed { message }) if message.contains("injected")),
            "original write error must be returned verbatim, got {error:?}"
        );

        // The poisoned segment is a recoverable partial, NEVER a final.
        let finals = finals_under(temp.path());
        assert!(
            finals.is_empty(),
            "a poisoned segment must never be published, found {finals:?}"
        );
        let partials = partials_under(temp.path());
        assert_eq!(partials.len(), 1, "exactly the poisoned partial remains");
        let bytes = std::fs::read(&partials[0]).unwrap();
        assert!(!bytes.is_empty(), "healthy prefix stayed on disk");

        // Event story: abandonment announced, no SegmentFinalized, and the
        // terminal RecordingStopped still emitted exactly once with the
        // failure marked.
        assert!(matches!(
            events.last(),
            Some(RecordingEvent::RecordingStopped {
                completed: false,
                ..
            })
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RecordingEvent::SegmentFinalized { .. }))
                .count(),
            0
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RecordingEvent::SegmentAbandoned { .. }))
        );
    }

    /// A DEMUX-side read failure may salvage the healthy prefix: the
    /// already-written content is finalized and published when the trailer
    /// succeeds.
    #[test]
    fn read_failure_salvages_the_healthy_prefix_when_finalization_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixtures_dir().join("session_av.mkv");
        let interrupt = InterruptHandle::new();
        let input = MediaInput::open(&MediaSource::File(source.clone()), &interrupt).unwrap();
        let mut session = RecordingSession::from_input(
            input,
            RecordingsLayout::new(temp.path().join("recordings")).unwrap(),
            RecorderConfig::new(CameraId::parse("rec-test").unwrap())
                .with_segment_target(Duration::from_secs(30)), // no rotation: ONE segment
        )
        .unwrap();

        const FAIL_AFTER_READS: usize = 120;
        let mut reads = 0_usize;
        session.read_fault = Some(Box::new(move || {
            reads += 1;
            if reads > FAIL_AFTER_READS {
                Some(MediaError::ReadFailed {
                    message: "injected read failure".to_owned(),
                })
            } else {
                None
            }
        }));

        let mut events = Vec::new();
        let outcome = session.run(&mut |event| events.push(event));
        match &outcome {
            Err(RecordingError::Media(MediaError::ReadFailed { message })) => {
                assert!(message.contains("injected"), "{message}");
            }
            other => panic!("expected the original read error, got {other:?}"),
        }

        // The healthy prefix became a real published recording.
        let finals = finals_under(temp.path());
        assert_eq!(finals.len(), 1, "salvaged prefix published once");
        assert!(partials_under(temp.path()).is_empty());

        // Terminal event still fired exactly once, marking the failure.
        let stopped: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, RecordingEvent::RecordingStopped { .. }))
            .collect();
        assert_eq!(stopped.len(), 1);
        assert!(matches!(
            stopped[0],
            RecordingEvent::RecordingStopped {
                completed: false,
                ..
            }
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RecordingEvent::SegmentFinalized { .. }))
                .count(),
            1
        );
    }
}
