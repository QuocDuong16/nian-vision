//! Media-layer errors.

/// Errors surfaced by media backends.
///
/// Messages are written to be safe for logs and UI: they describe the
/// operation and never include credentials or raw URLs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The source could not be opened (unreachable host, missing file,
    /// rejected credentials).
    #[error("cannot open media source: {message}")]
    OpenFailed {
        /// Human-safe description.
        message: String,
    },

    /// Reading from an open source failed mid-stream.
    #[error("media read failure: {message}")]
    ReadFailed {
        /// Human-safe description.
        message: String,
    },

    /// Writing a muxed output failed.
    #[error("media write failure: {message}")]
    WriteFailed {
        /// Human-safe description.
        message: String,
    },

    /// A blocking media operation was cancelled by the operator.
    ///
    /// This is *pure cancellation* — since M3 it never covers deadline
    /// expiry, which has its own [`MediaError::TimedOut`] variant. The
    /// distinction is load-bearing: cancellation means "stop everything",
    /// while a timeout means "the source stalled; salvage what is healthy
    /// and reconnect" (M3 §6).
    #[error("{operation} was cancelled")]
    Interrupted {
        /// Which operation was in flight, e.g. `open media source`.
        operation: &'static str,
    },

    /// A blocking media operation exceeded its operation-scoped deadline.
    ///
    /// For a packet read this typically means the live source stopped
    /// delivering data (stall/network loss). It is retryable: callers may
    /// salvage the healthy segment prefix and reconnect.
    #[error("{operation} timed out (operation deadline exceeded)")]
    TimedOut {
        /// Which operation was in flight, e.g. `read packet`.
        operation: &'static str,
    },

    /// The loaded FFmpeg runtime does not match the ABI this build targets.
    #[error(
        "FFmpeg ABI mismatch for {library}: build targets major {expected}, runtime reports {found}"
    )]
    AbiMismatch {
        /// Which library mismatched (`libavformat`, ...).
        library: &'static str,
        /// Major version the bindings were generated for.
        expected: u32,
        /// Major version detected at runtime.
        found: u32,
    },

    /// One-time media subsystem initialization failed.
    #[error("media subsystem initialization failed: {message}")]
    InitFailed {
        /// Human-safe description.
        message: String,
    },
}

impl MediaError {
    /// Returns `true` when the error was caused by explicit cancellation,
    /// which callers treat as normal control flow ("stop requested").
    ///
    /// Deliberately `false` for [`MediaError::TimedOut`]: a deadline expiry
    /// is a retryable source failure, not a stop request.
    pub fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted { .. })
    }

    /// Returns `true` when the error was caused by an operation-scoped
    /// deadline rather than cancellation.
    pub fn is_timed_out(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }
}
