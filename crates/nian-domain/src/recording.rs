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
