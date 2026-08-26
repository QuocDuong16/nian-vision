//! IPC errors.

/// Errors raised by encoding/decoding or during request dispatch.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Underlying transport failed.
    #[error("ipc transport failure: {0}")]
    Io(#[from] std::io::Error),

    /// Payload was not valid JSON or did not match the envelope schema.
    #[error("malformed ipc message: {0}")]
    MalformedMessage(#[from] serde_json::Error),

    /// A single line exceeded [`crate::MAX_MESSAGE_BYTES`].
    #[error("ipc message too large: {actual} bytes (limit {limit})")]
    MessageTooLarge {
        /// Configured limit in bytes.
        limit: usize,
        /// Offending size in bytes.
        actual: usize,
    },

    /// Envelope carried a protocol version this build cannot speak.
    #[error("unsupported ipc protocol version: {found} (expected {expected})")]
    UnsupportedProtocolVersion {
        /// Version found on the wire.
        found: u32,
        /// Version implemented by this crate.
        expected: u32,
    },
}
