//! Application configuration with validation.

use std::path::PathBuf;
use std::time::Duration;

use nian_domain::RetentionPolicy;

/// Target duration of one recording segment.
///
/// The recorder rotates segments at the first video keyframe at or after this
/// deadline, so real segment duration is `target .. target + keyframe
/// interval` (see master spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentTargetDuration(Duration);

impl SegmentTargetDuration {
    /// Smallest accepted segment target (5 seconds).
    pub const MIN: Duration = Duration::from_secs(5);
    /// Largest accepted segment target (1 hour).
    pub const MAX: Duration = Duration::from_secs(3600);
    /// Factory default from the master spec (~5 minute segments).
    pub const DEFAULT: Self = Self(Duration::from_secs(300));

    /// Validates and wraps a target duration.
    pub fn new(value: Duration) -> Result<Self, crate::error::ApplicationError> {
        if value < Self::MIN || value > Self::MAX {
            return Err(crate::error::ApplicationError::ConfigValidation(format!(
                "segment target duration must be between {:?} and {:?}",
                Self::MIN,
                Self::MAX
            )));
        }
        Ok(Self(value))
    }

    /// Wrapped duration value.
    pub fn get(self) -> Duration {
        self.0
    }
}

impl Default for SegmentTargetDuration {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Validated application configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    storage_root: PathBuf,
    segment_target_duration: SegmentTargetDuration,
    retention: RetentionPolicy,
}

impl AppConfig {
    /// Lowest accepted storage root: it must be an absolute path that is not
    /// the filesystem root itself, so recordings never land somewhere
    /// ambiguous and cleanup can never consider `/` a recording directory.
    pub fn builder(storage_root: impl Into<PathBuf>) -> AppConfigBuilder {
        AppConfigBuilder {
            storage_root: storage_root.into(),
            segment_target_duration: SegmentTargetDuration::DEFAULT,
            retention: RetentionPolicy::default(),
        }
    }

    /// Directory under which all camera recording trees live.
    pub fn storage_root(&self) -> &std::path::Path {
        &self.storage_root
    }

    /// Segment rotation target.
    pub fn segment_target_duration(&self) -> SegmentTargetDuration {
        self.segment_target_duration
    }

    /// Retention policy applied during cleanup passes.
    pub fn retention(&self) -> &RetentionPolicy {
        &self.retention
    }
}

/// Builder producing a validated [`AppConfig`].
#[derive(Debug, Clone)]
pub struct AppConfigBuilder {
    storage_root: PathBuf,
    segment_target_duration: SegmentTargetDuration,
    retention: RetentionPolicy,
}

impl AppConfigBuilder {
    /// Overrides the segment rotation target.
    pub fn segment_target_duration(
        mut self,
        value: Duration,
    ) -> Result<Self, crate::error::ApplicationError> {
        self.segment_target_duration = SegmentTargetDuration::new(value)?;
        Ok(self)
    }

    /// Overrides the retention policy.
    pub fn retention(
        mut self,
        value: RetentionPolicy,
    ) -> Result<Self, crate::error::ApplicationError> {
        value
            .validate()
            .map_err(|error| crate::error::ApplicationError::ConfigValidation(error.to_string()))?;
        self.retention = value;
        Ok(self)
    }

    /// Validates everything and produces the config.
    pub fn build(self) -> Result<AppConfig, crate::error::ApplicationError> {
        if !self.storage_root.is_absolute() {
            return Err(crate::error::ApplicationError::ConfigValidation(
                "storage_root must be an absolute path".to_owned(),
            ));
        }
        if self.storage_root.parent().is_none() {
            return Err(crate::error::ApplicationError::ConfigValidation(
                "storage_root must not be the filesystem root".to_owned(),
            ));
        }

        Ok(AppConfig {
            storage_root: self.storage_root,
            segment_target_duration: self.segment_target_duration,
            retention: self.retention,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let config = AppConfig::builder("/var/lib/nian-vision").build().unwrap();
        assert_eq!(
            config.segment_target_duration(),
            SegmentTargetDuration::DEFAULT
        );
        assert_eq!(config.retention(), &RetentionPolicy::default());
    }

    #[test]
    fn rejects_relative_and_root_storage_paths() {
        assert!(AppConfig::builder("relative/path").build().is_err());
        assert!(AppConfig::builder("/").build().is_err());
    }

    #[test]
    fn segment_duration_bounds_are_enforced() {
        assert!(SegmentTargetDuration::new(Duration::from_secs(4)).is_err());
        assert!(SegmentTargetDuration::new(Duration::from_secs(5)).is_ok());
        assert!(SegmentTargetDuration::new(Duration::from_secs(3601)).is_err());
        assert!(SegmentTargetDuration::new(Duration::from_secs(3600)).is_ok());
    }

    #[test]
    fn invalid_retention_is_rejected_at_build_time() {
        let result = AppConfig::builder("/tmp/nian-test").retention(RetentionPolicy {
            max_age_days: Some(0),
            max_storage_bytes: None,
        });
        assert!(result.is_err());
    }
}
