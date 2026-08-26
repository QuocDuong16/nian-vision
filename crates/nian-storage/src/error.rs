//! Storage-layer errors.

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
}
