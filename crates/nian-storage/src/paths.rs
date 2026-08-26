//! Deterministic recordings directory layout with traversal-safe names.

use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nian_domain::CameraId;

use crate::error::StorageError;

/// File extension used for finalized recording segments.
pub const SEGMENT_EXTENSION: &str = "mkv";

/// Suffix inserted before the extension while a segment is still open
/// (`08-30-00.partial.mkv`); finalized files never carry it.
pub const SEGMENT_PARTIAL_SUFFIX: &str = ".partial";

/// Builds paths inside the recordings storage root.
///
/// Every component is either derived from a validated [`CameraId`] or from a
/// formatted timestamp, and is re-checked by [`checked_component`] so no
/// user-controlled string can escape the storage tree.
#[derive(Debug, Clone)]
pub struct RecordingsLayout {
    root: PathBuf,
}

impl RecordingsLayout {
    /// Creates a layout rooted at an absolute directory.
    ///
    /// The filesystem root itself is rejected: cleanup must never be able to
    /// mistake `/` (or a drive root) for a recordings directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(StorageError::InvalidRoot {
                reason: "storage root must be absolute".to_owned(),
            });
        }
        if root.parent().is_none() {
            return Err(StorageError::InvalidRoot {
                reason: "storage root must not be the filesystem root".to_owned(),
            });
        }
        Ok(Self { root })
    }

    /// The validated storage root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<camera-id>`
    // The expects below are invariants, not guesses: `checked_component` only
    // rejects separators, control characters, and dot components, none of
    // which a validated `CameraId` or a formatted date can contain.
    #[allow(clippy::expect_used)]
    pub fn camera_dir(&self, camera: &CameraId) -> PathBuf {
        self.join_checked(&[camera.as_str()])
            .expect("validated CameraId is always path-safe")
    }

    /// `<root>/<camera-id>/<year>/<month>/<day>`
    #[allow(clippy::expect_used)]
    pub fn day_dir(&self, camera: &CameraId, date: NaiveDate) -> PathBuf {
        self.join_checked(&[
            camera.as_str(),
            &format!("{:04}", date.year()),
            &format!("{:02}", date.month()),
            &format!("{:02}", date.day()),
        ])
        .expect("formatted date components are always path-safe")
    }

    /// `<day-dir>/HH-MM-SS.mkv` — final form of a segment.
    pub fn segment_path(&self, camera: &CameraId, started_at: NaiveDateTime) -> PathBuf {
        self.day_dir(camera, started_at.date())
            .join(segment_file_name(started_at.time()))
    }

    /// `<day-dir>/HH-MM-SS.partial.mkv` — file being written right now.
    pub fn partial_segment_path(&self, camera: &CameraId, started_at: NaiveDateTime) -> PathBuf {
        self.day_dir(camera, started_at.date())
            .join(partial_file_name(started_at.time()))
    }

    fn join_checked(&self, components: &[&str]) -> Result<PathBuf, StorageError> {
        let mut path = self.root.clone();
        for component in components {
            path.push(checked_component(component)?);
        }
        Ok(path)
    }
}

/// Formats the final segment file name for a start time (`08-30-00.mkv`).
pub fn segment_file_name(started_at: NaiveTime) -> String {
    format!(
        "{:02}-{:02}-{:02}.{}",
        started_at.hour(),
        started_at.minute(),
        started_at.second(),
        SEGMENT_EXTENSION
    )
}

/// Formats the in-progress segment file name (`08-30-00.partial.mkv`).
pub fn partial_file_name(started_at: NaiveTime) -> String {
    format!(
        "{:02}-{:02}-{:02}{}.{}",
        started_at.hour(),
        started_at.minute(),
        started_at.second(),
        SEGMENT_PARTIAL_SUFFIX,
        SEGMENT_EXTENSION
    )
}

/// A parsed segment file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSegmentName {
    /// Segment start time-of-day recovered from the name.
    pub started_at: NaiveTime,
    /// Whether the name refers to a not-yet-finalized partial file.
    pub is_partial: bool,
}

/// Parses `HH-MM-SS[.partial].mkv` back into its parts.
///
/// Used by startup reconciliation to classify files found on disk without
/// trusting the database index.
pub fn parse_segment_file_name(name: &str) -> Result<ParsedSegmentName, StorageError> {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return Err(StorageError::UnrecognizedSegmentName {
            name: name.to_owned(),
        });
    };
    if extension != SEGMENT_EXTENSION {
        return Err(StorageError::UnrecognizedSegmentName {
            name: name.to_owned(),
        });
    }

    let (time_part, is_partial) = match stem.strip_suffix(SEGMENT_PARTIAL_SUFFIX) {
        Some(stripped) => (stripped, true),
        None => (stem, false),
    };

    let mut parts = time_part.split('-');
    let (h, m, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(s), None) => (h, m, s),
        _ => {
            return Err(StorageError::UnrecognizedSegmentName {
                name: name.to_owned(),
            });
        }
    };

    let invalid = || StorageError::UnrecognizedSegmentName {
        name: name.to_owned(),
    };

    let hour: u32 = h.parse().map_err(|_| invalid())?;
    let minute: u32 = m.parse().map_err(|_| invalid())?;
    let second: u32 = s.parse().map_err(|_| invalid())?;
    if h.len() != 2 || m.len() != 2 || s.len() != 2 {
        return Err(invalid());
    }

    let started_at = NaiveTime::from_hms_opt(hour, minute, second).ok_or_else(invalid)?;

    Ok(ParsedSegmentName {
        started_at,
        is_partial,
    })
}

/// Returns the input unchanged when it is safe to embed in a path.
fn checked_component(component: &str) -> Result<&str, StorageError> {
    let unsafe_chars = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    if component.is_empty()
        || component == "."
        || component == ".."
        || component
            .chars()
            .any(|c| c.is_control() || unsafe_chars.contains(&c))
    {
        return Err(StorageError::UnsafeComponent {
            component: component.to_owned(),
        });
    }
    Ok(component)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> RecordingsLayout {
        RecordingsLayout::new("/srv/nian-vision/recordings").unwrap()
    }

    fn sample_start() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(8, 30, 0)
            .unwrap()
    }

    #[test]
    fn layout_matches_spec_example() {
        let camera = CameraId::parse("cam-1").unwrap();
        let path = layout().segment_path(&camera, sample_start());
        // Backslash-free forward-slash rendering for the assertion only.
        let rendered = path.to_string_lossy().replace('\\', "/");
        assert_eq!(
            rendered,
            "/srv/nian-vision/recordings/cam-1/2026/08/26/08-30-00.mkv"
        );
    }

    #[test]
    fn partial_naming_and_finalize_roundtrip() {
        let camera = CameraId::parse("cam-1").unwrap();
        let partial = layout().partial_segment_path(&camera, sample_start());
        assert!(partial.to_string_lossy().ends_with("08-30-00.partial.mkv"));

        let parsed =
            parse_segment_file_name(partial.file_name().unwrap().to_str().unwrap()).unwrap();
        assert!(parsed.is_partial);
        assert_eq!(parsed.started_at, sample_start().time());

        let finalized_name = partial
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(".partial.mkv", ".mkv");
        assert_eq!(finalized_name, segment_file_name(sample_start().time()));
    }

    #[test]
    fn parse_rejects_foreign_names() {
        for bad in [
            "notes.txt",
            "video.mp4",
            "8-30-00.mkv",
            "123456.mkv",
            "08-30-00",
            "",
            "08-30-00.final.mkv",
        ] {
            assert!(parse_segment_file_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_unsafe_components() {
        for evil in ["..", ".", "", "a/b", r"a\b", "a:b", "a\nb"] {
            assert!(
                checked_component(evil).is_err(),
                "accepted component {evil:?}"
            );
        }
    }

    #[test]
    fn rejects_relative_and_root_storage_roots() {
        assert!(RecordingsLayout::new("relative/path").is_err());
        assert!(RecordingsLayout::new("/").is_err());
    }
}
