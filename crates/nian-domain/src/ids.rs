//! Stable identifiers used across Nian Vision.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// Stable identifier for a camera.
///
/// The value is restricted to `[a-z0-9_][a-z0-9_-]{0,63}` so it can be used
/// directly as a directory name without further escaping. Camera display
/// names must never be used for paths; use this identifier instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CameraId(String);

impl CameraId {
    /// Maximum length of a camera identifier.
    pub const MAX_LEN: usize = 64;

    /// Validates and creates a camera identifier.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, DomainError> {
        let value = value.as_ref();

        if value.is_empty() {
            return Err(DomainError::InvalidCameraId {
                reason: "must not be empty".to_owned(),
            });
        }
        if value.len() > Self::MAX_LEN {
            return Err(DomainError::InvalidCameraId {
                reason: format!("must be at most {} bytes", Self::MAX_LEN),
            });
        }

        let first = value.as_bytes()[0];
        let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
        let rest_ok = value.as_bytes()[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-');

        if !first_ok || !rest_ok {
            return Err(DomainError::InvalidCameraId {
                reason: "must match [a-z0-9][a-z0-9_-]*".to_owned(),
            });
        }

        Ok(Self(value.to_owned()))
    }

    /// Borrows the identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CameraId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for CameraId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Database-assigned identifier of a single recording segment.
///
/// Recordings are identified by a monotonically increasing number assigned by
/// the storage index; the identifier alone carries no meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordingId(NonZeroU64);

impl RecordingId {
    /// Creates an identifier from a database row id.
    ///
    /// Returns an error for zero, which databases never assign.
    pub fn new(value: u64) -> Result<Self, DomainError> {
        let value = NonZeroU64::new(value).ok_or(DomainError::InvalidRecordingId)?;
        Ok(Self(value))
    }

    /// Returns the raw database value.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for RecordingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_camera_ids() {
        for valid in ["cam-1", "front_door", "0", "a", "tapo-c200-livingroom"] {
            let id = CameraId::parse(valid).unwrap_or_else(|e| panic!("{valid}: {e}"));
            assert_eq!(id.as_str(), valid);
        }
    }

    #[test]
    fn rejects_camera_ids_unsafe_for_paths_or_logs() {
        for invalid in [
            "",
            "-lead",
            "Upper",
            "has space",
            "slash/slash",
            "dot/../dot",
            "@at",
            "very-long-id-that-exceeds-the-sixty-four-byte-limit-aaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                CameraId::parse(invalid).is_err(),
                "expected rejection of {invalid:?}"
            );
        }
    }

    #[test]
    fn recording_id_rejects_zero() {
        assert!(RecordingId::new(0).is_err());
        assert_eq!(RecordingId::new(42).unwrap().get(), 42);
    }
}
