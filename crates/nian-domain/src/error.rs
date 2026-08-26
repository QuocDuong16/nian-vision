//! Error type shared by domain operations.

use std::num::TryFromIntError;

/// Errors raised while constructing or validating domain values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// Camera identifier violated the path-safe charset/length rules.
    #[error("invalid camera id: {reason}")]
    InvalidCameraId { reason: String },

    /// Recording identifier was zero or out of range.
    #[error("invalid recording id")]
    InvalidRecordingId,

    /// Endpoint (host/port/path) failed validation.
    #[error("invalid camera endpoint: {reason}")]
    InvalidEndpoint { reason: String },

    /// Retention/quota configuration failed validation.
    #[error("invalid retention policy: {reason}")]
    InvalidRetentionPolicy { reason: String },

    /// A media time base had a non-usable denominator.
    #[error("invalid media time base: denominator {den}")]
    InvalidMediaTimeBase { den: i32 },

    /// Numeric conversion failed while validating a value.
    #[error("value out of range: {0}")]
    OutOfRange(#[from] TryFromIntError),
}
