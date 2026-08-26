//! Storage retention policy model.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// Deletion policy for old recordings.
///
/// Both limits are optional and combined with OR semantics when both are
/// set: material becomes eligible for deletion as soon as it exceeds
/// **either** limit (too old OR too much total storage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Delete recordings older than this many days.
    pub max_age_days: Option<u32>,
    /// Delete oldest recordings once total storage exceeds this size.
    pub max_storage_bytes: Option<u64>,
}

impl RetentionPolicy {
    /// Validates the policy values.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.max_age_days == Some(0) {
            return Err(DomainError::InvalidRetentionPolicy {
                reason: "max_age_days must be at least 1".to_owned(),
            });
        }
        if self.max_storage_bytes.is_some_and(|v| v == 0) {
            return Err(DomainError::InvalidRetentionPolicy {
                reason: "max_storage_bytes must be greater than zero".to_owned(),
            });
        }
        Ok(())
    }
}

/// High/low watermark quota controlling cleanup hysteresis.
///
/// When usage rises above [`StorageQuota::max_bytes`], finalized recordings
/// are deleted oldest-first until usage falls to or below
/// [`StorageQuota::cleanup_target_bytes`]. Cleanup then stays idle until the
/// high watermark is crossed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageQuota {
    /// Upper bound that triggers cleanup.
    pub max_bytes: u64,
    /// Usage target after a cleanup pass.
    pub cleanup_target_bytes: u64,
}

impl StorageQuota {
    /// Minimum accepted value for [`StorageQuota::max_bytes`] (1 MiB) to make
    /// degenerate configurations fail loudly instead of deleting everything.
    pub const MIN_MAX_BYTES: u64 = 1024 * 1024;

    /// Validates watermark ordering and magnitude.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.max_bytes < Self::MIN_MAX_BYTES {
            return Err(DomainError::InvalidRetentionPolicy {
                reason: format!("max_bytes must be at least {} bytes", Self::MIN_MAX_BYTES),
            });
        }
        if self.cleanup_target_bytes >= self.max_bytes {
            return Err(DomainError::InvalidRetentionPolicy {
                reason: "cleanup_target_bytes must be lower than max_bytes".to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_is_valid() {
        assert!(RetentionPolicy::default().validate().is_ok());
    }

    #[test]
    fn policy_rejects_zero_limits() {
        let policy = RetentionPolicy {
            max_age_days: Some(0),
            max_storage_bytes: None,
        };
        assert!(policy.validate().is_err());

        let policy = RetentionPolicy {
            max_age_days: None,
            max_storage_bytes: Some(0),
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn quota_requires_target_below_max() {
        let quota = StorageQuota {
            max_bytes: 200 * 1024 * 1024 * 1024,
            cleanup_target_bytes: 180 * 1024 * 1024 * 1024,
        };
        assert!(quota.validate().is_ok());

        let inverted = StorageQuota {
            max_bytes: 1024 * 1024,
            cleanup_target_bytes: 2048 * 1024,
        };
        assert!(inverted.validate().is_err());

        let tiny = StorageQuota {
            max_bytes: 1023 * 1024,
            cleanup_target_bytes: 0,
        };
        assert!(tiny.validate().is_err());
    }
}
