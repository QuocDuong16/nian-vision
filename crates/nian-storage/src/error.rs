//! Storage-layer errors.

use std::path::PathBuf;

/// Errors raised while building or interpreting storage paths.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A path component failed the traversal-safety check.
    #[error("unsafe path component rejected: {component:?}")]
    UnsafeComponent {
        /// The offending component value.
        component: String,
    },

    /// The storage root itself is not acceptable.
    #[error("invalid storage root: {reason}")]
    InvalidRoot {
        /// Why the root was rejected.
        reason: String,
    },

    /// A file name did not match the recording segment naming scheme.
    #[error("unrecognized segment file name: {name:?}")]
    UnrecognizedSegmentName {
        /// The offending file name.
        name: String,
    },

    /// Listing a recordings directory failed during segment allocation or
    /// reconciliation (other than "does not exist yet", which is handled).
    #[error("cannot list recordings directory {path:?}: {source}")]
    Io {
        /// The directory that could not be read.
        path: PathBuf,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// Publishing a finalized segment failed because the destination name
    /// already exists. The finalized content is never allowed to replace an
    /// existing recording.
    #[error("recording destination already exists: {destination:?}")]
    DestinationExists {
        /// The colliding final path.
        destination: PathBuf,
    },

    /// The post-claim identity fence could not VALIDATE a freshly created
    /// candidate, and removing that unreturned candidate also failed. The
    /// claim is never returned, so the leftover ownership is ambiguous —
    /// both contexts are carried here so that ambiguity is observable
    /// instead of the cleanup failure being silently discarded.
    #[error(
        "post-claim identity fence failed for candidate {candidate:?}: {fence_error}; \
         removing the unreturned candidate also failed: {cleanup}"
    )]
    ClaimFenceCleanup {
        /// The candidate partial whose relinquish cleanup failed.
        candidate: PathBuf,
        /// Why the fence could not validate the identity.
        #[source]
        fence_error: Box<StorageError>,
        /// Why the candidate could not be removed.
        cleanup: std::io::Error,
    },
}
