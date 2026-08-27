//! Continuous segmented recording (M2).
//!
//! Turns one healthy media source into durable, independently playable
//! Matroska segments:
//!
//! ```text
//! RTSP / local MediaInput
//!         ↓
//! FfmpegPacket (packet-faithful stream copy)
//!         ↓
//! Recorder — keyframe-aware segmentation
//!         ↓
//! <HH-MM-SS[-N]>.partial.mkv   (exclusively claimed via nian-storage)
//!         ↓
//! durable Matroska finalize (trailer + flush, both must succeed)
//!         ↓
//! no-replace publication (renameat2/MoveFileExW/hard-link fallback)
//!         ↓
//! <HH-MM-SS[-N]>.mkv
//! ```
//!
//! The recorder is deliberately independent of any camera vendor and of the
//! desktop/UI layer: it consumes [`MediaSource`]/[`MediaInput`] and a
//! [`RecordingsLayout`], reports progress through plain [`RecordingEvent`]s,
//! and never exposes FFmpeg types.
//!
//! # Pinned behaviors
//!
//! * **Acquisition** happens exclusively through
//!   `RecordingsLayout::claim_segment` (exclusive creation + claim token);
//!   an existing partial or final recording is never overwritten.
//! * **Startup alignment**: packets are discarded until the first selected
//!   *video* keyframe; audio received before that keyframe is dropped
//!   rather than synchronized. Every published segment therefore starts on
//!   a video keyframe and is independently decodable.
//! * **Rotation** is keyframe-aware: once the segment's *media time*
//!   (packet DTS in the video stream's validated time base) reaches
//!   [`RecorderConfig::segment_target`], the recorder waits for the next
//!   video keyframe, finalizes the old segment **before** it, and writes
//!   that keyframe as the first packet of the new segment. Audio packet
//!   boundaries never rotate a segment.
//! * **Timestamps** of copied packets are preserved from the source
//!   (rescaled between time bases only); see ADR-0004 for why rebasing was
//!   rejected for M2.
//! * **Publication** is atomic no-replace per platform; a failed or empty
//!   segment stays behind as a recoverable `.partial.mkv` and can never
//!   surface as a completed recording.
//!
//! # Stop semantics
//!
//! Graceful stop ([`StopFlag`]) takes effect between packets: the current
//! operation finishes, then the active segment is finalized and published.
//! Forced cancellation (the [`InterruptHandle`]) aborts blocking FFmpeg
//! I/O; the active segment is then abandoned as recoverable partial instead
//! of being finalized, because further FFmpeg calls would fail anyway.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod session;

use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDateTime;
use nian_domain::CameraId;
use nian_media::MediaError;
use nian_storage::StorageError;

pub use session::{RecordingSession, RecordingSummary, StopFlag};

/// Default target duration of one recorded segment (5 minutes).
///
/// Rotation is keyframe-aware: the actual segment duration lands between
/// the target and the target plus one GOP of the camera.
pub const DEFAULT_SEGMENT_TARGET: Duration = Duration::from_secs(300);

/// Recorder configuration.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Camera identity used for the recordings directory layout.
    pub camera: CameraId,
    /// Target *media* duration of one segment. Rotation happens at the
    /// first selected video keyframe at/after this much elapsed media time
    /// — never purely on wall-clock time.
    pub segment_target: Duration,
    /// How audio streams are handled relative to the primary video stream.
    pub audio: AudioPolicy,
}

impl RecorderConfig {
    /// Default configuration for `camera`: 300 s segments with all audio
    /// streams copied alongside the video.
    pub fn new(camera: CameraId) -> Self {
        Self {
            camera,
            segment_target: DEFAULT_SEGMENT_TARGET,
            audio: AudioPolicy::CopyAll,
        }
    }

    /// Overrides the segment target duration.
    pub fn with_segment_target(mut self, segment_target: Duration) -> Self {
        self.segment_target = segment_target;
        self
    }

    /// Overrides the audio policy.
    pub fn with_audio(mut self, audio: AudioPolicy) -> Self {
        self.audio = audio;
        self
    }
}

/// Which streams accompany the primary video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPolicy {
    /// Stream-copy every audio stream found next to the video stream.
    CopyAll,
    /// Record video only.
    Exclude,
}

/// How a recording loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingEndReason {
    /// [`StopFlag::request`] was honored before the source ended.
    StopRequested,
    /// The source delivered clean end-of-stream.
    EndOfStream,
    /// A media or storage error ended the session (the error itself is
    /// returned by `run`).
    SourceError,
}

/// Stream-selection plan derived from probing the input.
///
/// The primary video stream is the **first video stream by container
/// order** — never assumed to be index 0. Data/subtitle/metadata streams
/// and any additional video streams are ignored deliberately.
#[derive(Debug, Clone)]
pub(crate) struct StreamPlan {
    /// Container index of the primary video stream; drives rotation timing
    /// and startup alignment.
    pub(crate) primary_video: u32,
    /// Validated time base of the primary video stream (positive numerator
    /// and denominator); the rotation clock is expressed in it.
    pub(crate) video_time_base: nian_domain::MediaRational,
    /// Streams to map into every segment (primary video first, then the
    /// selected audio streams). Passed to `MatroskaMuxer`'s explicit
    /// input→output mapping.
    pub(crate) selection: Vec<nian_domain::MediaStreamInfo>,
}

/// Progress events emitted synchronously while a recording runs.
///
/// The callback receives these in pipeline order; no event bus is built
/// around them (a plain callback is sufficient for four-plus events).
#[derive(Debug, Clone)]
pub enum RecordingEvent {
    /// The input opened and a recordable stream plan was validated.
    RecordingStarted {
        /// Local wall-clock time the recording session started (also the
        /// naming anchor for the first segment).
        started_at: NaiveDateTime,
    },
    /// A new segment slot was claimed and its muxer opened; the first
    /// packet written to it is a video keyframe.
    SegmentStarted {
        /// Path being written (`…/08-30-00.partial.mkv`).
        partial_path: PathBuf,
        /// Publication target after successful finalization
        /// (`…/08-30-00.mkv`).
        final_path: PathBuf,
        /// Local wall-clock time this segment started.
        started_at: NaiveDateTime,
    },
    /// A segment was finalized durably (trailer written, file flushed and
    /// closed) and published without replacing anything.
    SegmentFinalized {
        /// The completed recording.
        final_path: PathBuf,
        /// Local wall-clock time this segment started.
        started_at: NaiveDateTime,
        /// Media duration derived from packet timestamps when both ends of
        /// the segment carried timestamps; `None` otherwise (never guessed).
        media_duration: Option<Duration>,
        /// Size of the published file.
        size_bytes: u64,
    },
    /// A claimed segment will not become a recording. The partial file is
    /// left in place, still eligible for recovery; nothing was published.
    SegmentAbandoned {
        /// The leftover `.partial.mkv`.
        partial_path: PathBuf,
        /// Human-readable cause.
        reason: String,
    },
    /// The recording loop ended. Emitted **exactly once for every started
    /// session** — on the failure path too, after every segment has reached
    /// its terminal state (published or abandoned). `completed` mirrors
    /// whether `run` returns `Ok`; `end_reason` says what ended the loop,
    /// giving a future supervisor/UI enough state to reconcile without a
    /// second event channel.
    RecordingStopped {
        /// Number of segments published during the session.
        finalized_segments: usize,
        /// What ended the loop.
        end_reason: RecordingEndReason,
        /// Whether the session finished without an error (`run` returns
        /// `Ok` exactly when this is `true`).
        completed: bool,
    },
}

/// Errors that can end a recording abnormally.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    /// The source carries no video stream at all; NVR recording requires
    /// exactly one primary video stream.
    #[error("source has no video stream ({stream_count} stream(s) found)")]
    NoVideoStream {
        /// How many streams were present instead.
        stream_count: usize,
    },

    /// The primary video stream has no usable (strictly positive) time
    /// base; guessing one would corrupt segment durations, so the session
    /// refuses to start.
    #[error("video stream {stream_index} has no usable time base")]
    UnusableTimeBase {
        /// The offending stream's container index.
        stream_index: u32,
    },

    /// The configuration itself is invalid (e.g. zero segment target).
    #[error("invalid recorder configuration: {reason}")]
    InvalidConfig {
        /// What is wrong with the configuration.
        reason: String,
    },

    /// A media-backend failure (open/read/write/finalize).
    #[error(transparent)]
    Media(#[from] MediaError),

    /// A storage failure (claim/publication/filesystem).
    #[error(transparent)]
    Storage(#[from] StorageError),
}
