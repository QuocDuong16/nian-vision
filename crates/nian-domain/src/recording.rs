//! Recording lifecycle concepts.

use serde::{Deserialize, Serialize};

/// State of a single recording segment as tracked by the storage index.
///
/// The filesystem remains the source of survival: startup reconciliation
/// compares these states against what is actually on disk (see milestone M4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RecordingState {
    /// Segment is being written right now.
    #[default]
    Active,
    /// Segment was finalized cleanly.
    Complete,
    /// Found as a leftover partial file at startup; being inspected.
    Recovering,
    /// File exists but failed inspection (truncated/corrupt).
    Corrupted,
    /// Index references a file that no longer exists on disk.
    Missing,
}

impl RecordingState {
    /// Stable lowercase name used in logs and IPC payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Recovering => "recovering",
            Self::Corrupted => "corrupted",
            Self::Missing => "missing",
        }
    }
}

/// Coarse, typed classification of how and why a recording attempt ended
/// (M3 §2).
///
/// This is the shared wire vocabulary between the media worker's
/// `recording.status` and the parent supervisor (final remediation §9,
/// final safety remediation §7): both binaries validate against THIS
/// definition, so they can never disagree silently about a category's
/// meaning. It lives in the FFmpeg-free domain crate precisely so the
/// parent can parse it without depending on any media implementation.
/// Variants deliberately answer exactly one question each: *who* ended the
/// session (operator), *what* failed (source, mux/output, storage,
/// configuration), or whether the end was even a failure at all (clean
/// EOF).
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

impl FailureCategory {
    /// Stable wire representation for IPC payloads (final remediation §9).
    ///
    /// This — NOT Rust `Debug` output — is the protocol contract between the
    /// worker's `recording.status` and the parent's classification; Rust
    /// enum-variant naming must never leak into the wire format.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorStop => "operator_stop",
            Self::OperatorCancellation => "operator_cancellation",
            Self::CleanEof => "clean_eof",
            Self::SourceOpenFailed => "source_open_failed",
            Self::SourceReadFailed => "source_read_failed",
            Self::SourceTimedOut => "source_timed_out",
            Self::OutputWriteFailed => "output_write_failed",
            Self::StorageFailed => "storage_failed",
            Self::PermanentConfiguration => "permanent_configuration",
        }
    }

    /// Parses the stable wire representation back into the typed category
    /// (parent-side consumption of the protocol strings). Unknown strings
    /// (newer workers) yield `None` instead of guessing.
    pub fn from_wire(value: &str) -> Option<Self> {
        let category = match value {
            "operator_stop" => Self::OperatorStop,
            "operator_cancellation" => Self::OperatorCancellation,
            "clean_eof" => Self::CleanEof,
            "source_open_failed" => Self::SourceOpenFailed,
            "source_read_failed" => Self::SourceReadFailed,
            "source_timed_out" => Self::SourceTimedOut,
            "output_write_failed" => Self::OutputWriteFailed,
            "storage_failed" => Self::StorageFailed,
            "permanent_configuration" => Self::PermanentConfiguration,
            _ => return None,
        };
        Some(category)
    }
}

#[cfg(test)]
mod failure_category_tests {
    use super::*;

    /// Every category must round-trip through the stable wire values, and
    /// the wire vocabulary must stay EXACTLY these nine strings (final
    /// safety remediation §7: the parent rejects anything else as a
    /// protocol violation).
    #[test]
    fn wire_values_round_trip_and_are_exhaustive() {
        let all = [
            FailureCategory::OperatorStop,
            FailureCategory::OperatorCancellation,
            FailureCategory::CleanEof,
            FailureCategory::SourceOpenFailed,
            FailureCategory::SourceReadFailed,
            FailureCategory::SourceTimedOut,
            FailureCategory::OutputWriteFailed,
            FailureCategory::StorageFailed,
            FailureCategory::PermanentConfiguration,
        ];
        assert_eq!(all.len(), 9);
        for category in all {
            assert_eq!(
                FailureCategory::from_wire(category.as_str()),
                Some(category)
            );
            assert!(!category.as_str().contains(char::is_uppercase));
        }
        assert_eq!(FailureCategory::from_wire("Unknown"), None);
        assert_eq!(FailureCategory::from_wire(""), None);
        assert_eq!(FailureCategory::from_wire("storage_failed "), None);
    }
}
