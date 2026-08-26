//! Stable identifiers used across Nian Vision.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// Stable identifier for a camera.
///
/// The value matches `[a-z0-9][a-z0-9_-]{0,63}` (first character must be a
/// lowercase ASCII letter or digit; at most 64 bytes in total) so it can be
/// used directly as a directory name without further escaping. Windows
/// reserved device names (CON, PRN, AUX, NUL, COM1–9, LPT1–9) are rejected
/// case-insensitively because Windows is the primary target and these names
/// are special in path resolution regardless of directory. Camera display
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
                reason: "must match [a-z0-9][a-z0-9_-]{0,63}".to_owned(),
            });
        }

        // Windows reserves these device names in path resolution
        // case-insensitively (and historically even with extensions); reject
        // them outright so a camera id can never collide with a device.
        if WINDOWS_RESERVED_DEVICE_NAMES
            .iter()
            .any(|reserved| value.eq_ignore_ascii_case(reserved))
        {
            return Err(DomainError::InvalidCameraId {
                reason: format!("{value:?} is a Windows reserved device name"),
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

/// Windows reserved device names, rejected case-insensitively for
/// [`CameraId`] (Windows is the primary target and resolves these as
/// devices in any directory).
const WINDOWS_RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

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
    fn rejects_windows_reserved_device_names_case_insensitively() {
        for reserved in [
            "con", "CON", "Con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9", "Com4",
            "LPT7",
        ] {
            assert!(
                CameraId::parse(reserved).is_err(),
                "expected rejection of Windows reserved name {reserved:?}"
            );
        }
    }

    #[test]
    fn accepts_names_merely_containing_reserved_prefixes() {
        // Only exact matches are reserved; these are safe directory names.
        for valid in [
            "console",
            "com10",
            "com0",
            "nullify",
            "auxiliary",
            "lpt10",
            "control",
        ] {
            assert!(
                CameraId::parse(valid).is_ok(),
                "expected acceptance of {valid:?}"
            );
        }
    }

    #[test]
    fn recording_id_rejects_zero() {
        assert!(RecordingId::new(0).is_err());
        assert_eq!(RecordingId::new(42).unwrap().get(), 42);
    }
}
