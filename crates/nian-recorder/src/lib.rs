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

mod recovery;
mod session;
pub mod supervisor;

use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDateTime;
use nian_domain::CameraId;
use nian_media::MediaError;
use nian_storage::StorageError;

pub use recovery::{RecoveryError, RecoveryFailure, RecoveryOutcome, recover_camera_partials};
pub use session::{RecordingSession, RecordingSummary, StopFlag};
pub use supervisor::{
    AttemptOutcome, CameraRecordingSupervisor, EofInterpretation, Jitter, NoJitter, SeededJitter,
    SessionFactory, SleepWaiter, SourceKind, SupervisorConfig, SupervisorEnd, SupervisorEvent,
    SupervisorState, Waiter,
};

/// Default target duration of one recorded segment (5 minutes).
///
/// Rotation is keyframe-aware: the actual segment duration lands between
/// the target and the target plus one GOP of the camera.
pub const DEFAULT_SEGMENT_TARGET: Duration = Duration::from_secs(300);

/// Operation-scoped source deadlines for one recording session (M3 §5).
///
/// These bound FFmpeg's blocking operations so a dead camera or a stalled
/// network can never wedge the worker:
///
/// * `open` bounds connect + RTSP handshake;
/// * `read` bounds each individual packet read (the stall detector — no
///   packets for this long means the source is treated as dead).
///
/// Stream analysis/probe inherits the open budget when `open` is set.
/// Deadlines are armed and cleared around exactly their operation via RAII;
/// none is ever left installed during local mux writes or finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTimeouts {
    /// Deadline for opening/connecting the source (connect + handshake +
    /// stream analysis).
    pub open: Duration,
    /// Per-read stall deadline: a single packet read taking longer than
    /// this fails with a retryable timeout.
    pub read: Duration,
}

impl SourceTimeouts {
    /// Production defaults: 15 s to connect (slow Wi-Fi cameras exist),
    /// 15 s of packet silence before declaring a stall. The RTSP-TCP
    /// transport keeps per-packet gaps small, so anything beyond this means
    /// the stream is effectively gone.
    pub const DEFAULT: Self = Self {
        open: Duration::from_secs(15),
        read: Duration::from_secs(15),
    };
}

impl Default for SourceTimeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Recorder configuration.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Camera identity used for the recordings directory layout.
    pub camera: CameraId,
    /// Target *media* duration of one segment. Rotation happens at the
    /// first selected video keyframe at or after this much elapsed media time
    /// — never purely on wall-clock time.
    pub segment_target: Duration,
    /// How audio streams are handled relative to the primary video stream.
    pub audio: AudioPolicy,
    /// Operation-scoped source deadlines; see [`SourceTimeouts`].
    pub timeouts: SourceTimeouts,
}

impl RecorderConfig {
    /// Default configuration for `camera`: 300 s segments with all audio
    /// streams copied alongside the video, default source deadlines.
    pub fn new(camera: CameraId) -> Self {
        Self {
            camera,
            segment_target: DEFAULT_SEGMENT_TARGET,
            audio: AudioPolicy::CopyAll,
            timeouts: SourceTimeouts::DEFAULT,
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

    /// Overrides the operation-scoped source deadlines.
    pub fn with_timeouts(mut self, timeouts: SourceTimeouts) -> Self {
        self.timeouts = timeouts;
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

/// Coarse, typed classification of how and why a recording attempt ended
/// (M3 §2).
///
/// This is the vocabulary the reconnect supervisor reasons over; it must
/// never parse error strings. Variants deliberately answer exactly one
/// question each: *who* ended the session (operator), *what* failed (source,
/// mux/output, storage, configuration), or whether the end was even a
/// failure at all (clean EOF). Retryability is centralized in
/// [`RecordingError::category`], not re-derived ad hoc by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// Operator requested a graceful stop (StopFlag honored).
    OperatorStop,
    /// Operator forced cancellation of blocking media I/O. The active
    /// segment is abandoned; supervisors must NOT reconnect — shutdown was
    /// requested.
    OperatorCancellation,
    /// The source delivered clean end-of-stream. For a local finite file
    /// this is normal completion, never reconnectable; for RTSP the
    /// supervisor decides its operational meaning (transient disconnect vs
    /// camera gone) from the source kind.
    CleanEof,
    /// Opening/connecting to the source failed (unreachable host, refused
    /// connection, missing local file). Retryable for network sources.
    SourceOpenFailed,
    /// An established source read failed or died mid-stream (disconnect,
    /// unexpected EOF, network reset). Retryable for live sources.
    SourceReadFailed,
    /// A blocking source operation exceeded its deadline (connect timeout,
    /// read stall). Retryable: salvage healthy segments and reconnect.
    SourceTimedOut,
    /// Writing/finalizing the Matroska output failed. NOT retried
    /// automatically — output failures indicate disk/mux trouble that
    /// reconnecting to the camera cannot fix.
    OutputWriteFailed,
    /// The storage layer failed (claim, publication, filesystem I/O). NOT
    /// retried automatically: a broken storage root requires operator
    /// attention, and retrying could churn the filesystem forever.
    StorageFailed,
    /// Permanent, non-retryable failure: invalid recorder configuration, no
    /// usable video stream/time base, FFmpeg ABI mismatch or media
    /// initialization failure. Retrying cannot succeed.
    PermanentConfiguration,
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

impl RecordingError {
    /// Typed failure classification (M3 §2/§8) — the retryability decision
    /// the reconnect supervisor consumes. Never derived by parsing strings.
    ///
    /// The mapping encodes the M3 policy:
    ///
    /// * operator intent (stop flag, cancellation) is never an error to
    ///   recover from — it ends supervision;
    /// * source-side problems (open, read, timeout) are the retryable set;
    /// * local output/storage/configuration problems are permanent: a
    ///   reconnect cannot fix a full disk or a broken config, and looping
    ///   forever on them would churn the machine without ever recording.
    pub fn category(&self) -> FailureCategory {
        match self {
            Self::NoVideoStream { .. }
            | Self::UnusableTimeBase { .. }
            | Self::InvalidConfig { .. } => FailureCategory::PermanentConfiguration,
            // Media-side classification.
            Self::Media(media) => match media {
                MediaError::OpenFailed { .. } => FailureCategory::SourceOpenFailed,
                MediaError::ReadFailed { .. } => FailureCategory::SourceReadFailed,
                MediaError::TimedOut { .. } => FailureCategory::SourceTimedOut,
                MediaError::Interrupted { .. } => FailureCategory::OperatorCancellation,
                MediaError::WriteFailed { .. } => FailureCategory::OutputWriteFailed,
                MediaError::AbiMismatch { .. } | MediaError::InitFailed { .. } => {
                    FailureCategory::PermanentConfiguration
                }
            },
            Self::Storage(_) => FailureCategory::StorageFailed,
        }
    }

    /// Whether the reconnect supervisor may automatically try this failure
    /// again (M3 §8). Deliberately conservative: everything not attributable
    /// to a live source is permanent from the supervisor's perspective.
    ///
    /// Note on [`FailureCategory::CleanEof`]: it never appears here because
    /// a clean end-of-stream does not produce an error at all (`run`
    /// returns `Ok` with [`RecordingEndReason::EndOfStream`]); whether an
    /// RTSP EOF should reconnect is decided where the SOURCE KIND is known
    /// (the supervisor), not inside the recorder.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.category(),
            FailureCategory::SourceOpenFailed
                | FailureCategory::SourceReadFailed
                | FailureCategory::SourceTimedOut
        )
    }
}
#[cfg(test)]
mod classification_tests {
    //! Retryability matrix unit tests (M3 §2): every error kind maps to
    //! exactly one typed category and the retryable set stays deliberate.

    use super::*;
    use nian_media::MediaError;
    use nian_storage::StorageError;

    fn media(operation: &'static str) -> MediaError {
        MediaError::Interrupted { operation }
    }

    #[test]
    fn source_failures_are_retryable() {
        for error in [
            RecordingError::Media(MediaError::OpenFailed {
                message: "connection refused".to_owned(),
            }),
            RecordingError::Media(MediaError::ReadFailed {
                message: "connection reset".to_owned(),
            }),
            RecordingError::Media(MediaError::TimedOut {
                operation: "read packet",
            }),
        ] {
            assert!(
                error.is_retryable(),
                "{error:?} must be retryable for live sources"
            );
        }
        // Specifically NOT classified as cancellation.
        assert_eq!(
            RecordingError::Media(MediaError::TimedOut {
                operation: "open media source",
            })
            .category(),
            FailureCategory::SourceTimedOut
        );
    }

    #[test]
    fn operator_intent_is_never_retryable_and_never_string_parsed() {
        let stopped = RecordingError::Media(media("read packet"));
        // An Interrupted media error IS the cancellation carrier.
        assert_eq!(stopped.category(), FailureCategory::OperatorCancellation);
        assert!(!stopped.is_retryable());
    }

    #[test]
    fn local_output_and_storage_failures_are_permanent() {
        let write = RecordingError::Media(MediaError::WriteFailed {
            message: "no space left on device".to_owned(),
        });
        assert_eq!(write.category(), FailureCategory::OutputWriteFailed);
        assert!(!write.is_retryable());

        let storage = RecordingError::Storage(StorageError::InvalidRoot {
            reason: "gone".to_owned(),
        });
        assert_eq!(storage.category(), FailureCategory::StorageFailed);
        assert!(!storage.is_retryable());
    }

    #[test]
    fn configuration_and_runtime_problems_are_permanent() {
        let cases = [
            RecordingError::NoVideoStream { stream_count: 0 },
            RecordingError::UnusableTimeBase { stream_index: 3 },
            RecordingError::InvalidConfig {
                reason: "zero target".to_owned(),
            },
            RecordingError::Media(MediaError::AbiMismatch {
                library: "libavformat",
                expected: 62,
                found: 60,
            }),
            RecordingError::Media(MediaError::InitFailed {
                message: "no avcodec".to_owned(),
            }),
        ];
        for error in cases {
            assert_eq!(
                error.category(),
                FailureCategory::PermanentConfiguration,
                "{error:?} misclassified"
            );
            assert!(!error.is_retryable(), "{error:?} must never loop");
        }
    }

    #[test]
    fn timeout_is_distinct_from_cancellation_in_category_terms() {
        // The distinction M3 §6 demands, asserted at the type level: same
        // operation string, different abort cause, different category —
        // supervisors choose reconnect vs stop without string parsing.
        let timeout = RecordingError::Media(MediaError::TimedOut {
            operation: "read packet",
        });
        let cancelled = RecordingError::Media(MediaError::Interrupted {
            operation: "read packet",
        });
        assert_eq!(timeout.category(), FailureCategory::SourceTimedOut);
        assert_eq!(cancelled.category(), FailureCategory::OperatorCancellation);
        assert_ne!(timeout.category(), cancelled.category());
    }
}
