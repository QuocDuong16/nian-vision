//! Deterministic recordings directory layout with traversal-safe names.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nian_domain::CameraId;

use crate::error::StorageError;

/// File extension used for finalized recording segments.
pub const SEGMENT_EXTENSION: &str = "mkv";

/// Suffix inserted before the extension while a segment is still open
/// (`08-30-00.partial.mkv`); finalized files never carry it.
pub const SEGMENT_PARTIAL_SUFFIX: &str = ".partial";

/// Lowest disambiguation sequence rendered explicitly. Sequence 1 is the
/// bare name (`08-30-00.mkv`); from 2 on, the sequence appears as a suffix
/// (`08-30-00-2.mkv`). The suffix keeps recording collision-free when the
/// worker reconnects or restarts more than once within the same second.
pub const MIN_EXPLICIT_SEQUENCE: u32 = 2;

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

    /// Computes the paths for a new segment starting at `started_at` as if
    /// nothing else existed yet (sequence 1 when the day directory is
    /// missing).
    ///
    /// Collision-aware but **scan-only**: this is a dry-run naming helper
    /// for diagnostics and tests, not an acquisition primitive — between
    /// scanning and file creation another writer could take the name. The
    /// recorder must use [`RecordingsLayout::claim_segment`], which closes
    /// that TOCTOU window with exclusive creation.
    ///
    /// The names stay deterministic and parseable
    /// ([`parse_segment_file_name`]) so reconciliation can classify disk
    /// contents without trusting the database index.
    pub fn allocate_segment(
        &self,
        camera: &CameraId,
        started_at: NaiveDateTime,
    ) -> Result<AllocatedSegmentPaths, StorageError> {
        let day_dir = self.day_dir(camera, started_at.date());
        let sequence = allocate_segment_sequence(&day_dir, started_at.time())?;
        Ok(AllocatedSegmentPaths {
            partial_path: day_dir
                .join(partial_file_name_with_sequence(started_at.time(), sequence)),
            final_path: day_dir.join(segment_file_name_with_sequence(started_at.time(), sequence)),
        })
    }

    fn join_checked(&self, components: &[&str]) -> Result<PathBuf, StorageError> {
        let mut path = self.root.clone();
        for component in components {
            path.push(checked_component(component)?);
        }
        Ok(path)
    }

    /// Exclusively claims the next free segment slot for `started_at`.
    ///
    /// This is the race-safe primitive M2 must use instead of
    /// [`RecordingsLayout::allocate_segment`] (whose scan-then-open shape is
    /// only a dry-run naming helper): the day directory is created if needed,
    /// then the partial file is created with exclusive semantics
    /// (`create_new`, O_EXCL). If another worker won the same name between
    /// scan and create, the claim retries with the next sequence, so two
    /// workers racing in the same second always end up with distinct files
    /// and an existing recording is never truncated or replaced.
    ///
    /// The returned [`ClaimedSegment`] documents the finalize contract
    /// (write into `partial_path`, publish to `final_path` with
    /// [`publish_no_replace`]).
    pub fn claim_segment(
        &self,
        camera: &CameraId,
        started_at: NaiveDateTime,
    ) -> Result<ClaimedSegment, StorageError> {
        let day_dir = self.day_dir(camera, started_at.date());
        std::fs::create_dir_all(&day_dir).map_err(|source| StorageError::Io {
            path: day_dir.clone(),
            source,
        })?;

        loop {
            let sequence = allocate_segment_sequence(&day_dir, started_at.time())?;
            let partial_path =
                day_dir.join(partial_file_name_with_sequence(started_at.time(), sequence));
            let final_path =
                day_dir.join(segment_file_name_with_sequence(started_at.time(), sequence));

            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)
            {
                Ok(partial_file) => {
                    return Ok(ClaimedSegment {
                        partial_path,
                        final_path,
                        _partial_file: partial_file,
                    });
                }
                // Lost the race for this name (another writer created it
                // first): rescan and try the next sequence.
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(StorageError::Io {
                        path: partial_path,
                        source,
                    });
                }
            }
        }
    }
}

/// An **exclusively claimed** segment slot — the race-safe acquisition M2
/// must build on.
///
/// The partial file was created with `O_EXCL` semantics (`create_new`), so
/// at acquisition time no other writer on the system held this name. The
/// open file handle is retained as the claim token: while it is alive,
/// another process's `create_new` on the same name fails and its
/// [`RecordingsLayout::claim_segment`] rescan moves to the next sequence.
/// Duplicate workers therefore never share (or truncate) a segment.
///
/// Dropping the claim closes the handle but deliberately keeps the (empty or
/// partial) file: an abandoned claim is indistinguishable from a crash and
/// stays eligible for startup reconciliation.
///
/// Finalize flow for the owner:
/// 1. write the segment through `MatroskaMuxer::create(partial_path)` —
///    safe because the claimed file exists and is owned by this process;
/// 2. publish with [`publish_no_replace`] from `partial_path` to
///    `final_path`: atomic on every supported platform and never replaces an
///    existing recording ([`StorageError::DestinationExists`] on collision).
#[derive(Debug)]
pub struct ClaimedSegment {
    partial_path: PathBuf,
    final_path: PathBuf,
    /// Claim token; kept private so callers cannot accidentally close it
    /// while still treating the slot as theirs. Rust opens files with
    /// FILE_SHARE_READ|WRITE|DELETE on Windows, so holding this handle does
    /// not block the hard-link/unlink publication step.
    _partial_file: std::fs::File,
}

impl ClaimedSegment {
    /// File the recorder writes into (already created, empty).
    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }

    /// No-replace publication target after successful finalization.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

/// Publishes finalized content at its final name **atomically and without
/// ever replacing an existing file**.
///
/// Implemented as hard-link + unlink: `hard_link` fails with "already
/// exists" when the destination exists on every platform we target (this is
/// the no-replace guarantee; a plain `rename` would silently replace on
/// Unix), so the instant the final name appears it already references the
/// complete content. Removing the source link afterwards completes the
/// logical move.
///
/// Both paths must live on the same volume, which the recordings layout
/// guarantees. If only the unlink fails, the segment is already durably
/// published under its final name; the returned error tells reconciliation
/// to sweep the stale `.partial` link.
pub fn publish_no_replace(from: &Path, to: &Path) -> Result<(), StorageError> {
    if let Err(source) = std::fs::hard_link(from, to) {
        return Err(if source.kind() == std::io::ErrorKind::AlreadyExists {
            StorageError::DestinationExists {
                destination: to.to_path_buf(),
            }
        } else {
            StorageError::Io {
                path: to.to_path_buf(),
                source,
            }
        });
    }
    std::fs::remove_file(from).map_err(|source| StorageError::Io {
        path: from.to_path_buf(),
        source,
    })
}

/// Partial and final paths for one freshly allocated segment, sharing the
/// same sequence so `.partial` → final rename recovery stays well-defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatedSegmentPaths {
    /// File the recorder writes into (`…/08-30-00.partial.mkv`).
    pub partial_path: PathBuf,
    /// Rename target after successful finalization (`…/08-30-00.mkv`).
    pub final_path: PathBuf,
}

/// Picks the smallest segment sequence for `started_at` that collides with
/// no existing file in `day_dir`. See [`RecordingsLayout::allocate_segment`].
pub fn allocate_segment_sequence(
    day_dir: &Path,
    started_at: NaiveTime,
) -> Result<u32, StorageError> {
    let entries = match std::fs::read_dir(day_dir) {
        Ok(entries) => entries,
        // No directory yet: nothing can collide, use the bare name.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(source) => {
            return Err(StorageError::Io {
                path: day_dir.to_path_buf(),
                source,
            });
        }
    };

    let mut occupied: HashSet<u32> = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            path: day_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue; // non-UTF-8 names cannot be our segments
        };
        if let Ok(parsed) = parse_segment_file_name(name)
            && parsed.started_at == started_at
        {
            occupied.insert(parsed.sequence);
        }
    }

    let mut sequence = 1_u32;
    while occupied.contains(&sequence) {
        sequence = sequence.checked_add(1).ok_or_else(|| StorageError::Io {
            path: day_dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "segment sequence exhausted for this start time",
            ),
        })?;
    }
    Ok(sequence)
}

/// Formats the final segment file name for a start time (`08-30-00.mkv`).
pub fn segment_file_name(started_at: NaiveTime) -> String {
    segment_file_name_with_sequence(started_at, 1)
}

/// Like [`segment_file_name`] but with an explicit disambiguation sequence;
/// sequence 1 renders as the bare name, larger ones as `08-30-00-N.mkv`.
pub fn segment_file_name_with_sequence(started_at: NaiveTime, sequence: u32) -> String {
    format!("{}.{}", time_stem(started_at, sequence), SEGMENT_EXTENSION)
}

/// Formats the in-progress segment file name (`08-30-00.partial.mkv`).
pub fn partial_file_name(started_at: NaiveTime) -> String {
    partial_file_name_with_sequence(started_at, 1)
}

/// Like [`partial_file_name`] but with an explicit disambiguation sequence.
pub fn partial_file_name_with_sequence(started_at: NaiveTime, sequence: u32) -> String {
    format!(
        "{}{}.{}",
        time_stem(started_at, sequence),
        SEGMENT_PARTIAL_SUFFIX,
        SEGMENT_EXTENSION
    )
}

/// `HH-MM-SS` for sequence 1, `HH-MM-SS-N` from [`MIN_EXPLICIT_SEQUENCE`] on.
fn time_stem(started_at: NaiveTime, sequence: u32) -> String {
    let time = format!(
        "{:02}-{:02}-{:02}",
        started_at.hour(),
        started_at.minute(),
        started_at.second()
    );
    if sequence == 1 {
        time
    } else {
        format!("{time}-{sequence}")
    }
}

/// A parsed segment file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSegmentName {
    /// Segment start time-of-day recovered from the name.
    pub started_at: NaiveTime,
    /// Disambiguation sequence; 1 for the bare form (`08-30-00.mkv`),
    /// `>= [`MIN_EXPLICIT_SEQUENCE`]` for `08-30-00-N.mkv`.
    pub sequence: u32,
    /// Whether the name refers to a not-yet-finalized partial file.
    pub is_partial: bool,
}

/// Parses `HH-MM-SS[-N][.partial].mkv` back into its parts.
///
/// Used by startup reconciliation to classify files found on disk without
/// trusting the database index. The optional `-N` suffix must be a decimal
/// number `>= [`MIN_EXPLICIT_SEQUENCE`]` without leading zeros, mirroring
/// exactly what the allocator emits.
pub fn parse_segment_file_name(name: &str) -> Result<ParsedSegmentName, StorageError> {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return Err(unrecognized(name));
    };
    if extension != SEGMENT_EXTENSION {
        return Err(unrecognized(name));
    }

    let (stem, is_partial) = match stem.strip_suffix(SEGMENT_PARTIAL_SUFFIX) {
        Some(stripped) => (stripped, true),
        None => (stem, false),
    };

    let mut parts = stem.split('-');
    let (h, m, s, sequence_part) = match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(h), Some(m), Some(s), None, None) => (h, m, s, None),
        (Some(h), Some(m), Some(s), Some(sequence), None) => (h, m, s, Some(sequence)),
        _ => return Err(unrecognized(name)),
    };

    let invalid = || unrecognized(name);

    let hour: u32 = h.parse().map_err(|_| invalid())?;
    let minute: u32 = m.parse().map_err(|_| invalid())?;
    let second: u32 = s.parse().map_err(|_| invalid())?;
    if h.len() != 2 || m.len() != 2 || s.len() != 2 {
        return Err(invalid());
    }

    let sequence = match sequence_part {
        None => 1,
        Some(text) => {
            // Canonical rendering only: no leading zeros, and below the
            // minimum explicit sequence the bare form is authoritative.
            if text.starts_with('0') {
                return Err(invalid());
            }
            let value: u32 = text.parse().map_err(|_| invalid())?;
            if value < MIN_EXPLICIT_SEQUENCE {
                return Err(invalid());
            }
            value
        }
    };

    let started_at = NaiveTime::from_hms_opt(hour, minute, second).ok_or_else(invalid)?;

    Ok(ParsedSegmentName {
        started_at,
        sequence,
        is_partial,
    })
}

fn unrecognized(name: &str) -> StorageError {
    StorageError::UnrecognizedSegmentName {
        name: name.to_owned(),
    }
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

    fn layout_in(root: &Path) -> RecordingsLayout {
        RecordingsLayout::new(root).unwrap()
    }

    fn sample_start() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(8, 30, 0)
            .unwrap()
    }

    fn touch(path: &Path) {
        std::fs::File::create(path).unwrap();
    }

    #[test]
    fn layout_matches_spec_example() {
        let layout = RecordingsLayout::new("/srv/nian-vision/recordings").unwrap();
        let camera = CameraId::parse("cam-1").unwrap();
        let path = layout
            .allocate_segment(&camera, sample_start())
            .unwrap()
            .final_path;
        // Nothing exists yet, so the bare spec name is allocated…
        let rendered = path.to_string_lossy().replace('\\', "/");
        assert_eq!(
            rendered,
            "/srv/nian-vision/recordings/cam-1/2026/08/26/08-30-00.mkv"
        );
    }

    #[test]
    fn rejects_relative_and_root_storage_roots() {
        assert!(RecordingsLayout::new("relative/path").is_err());
        assert!(RecordingsLayout::new("/").is_err());
    }

    #[test]
    fn naming_forms_render_and_roundtrip() {
        let time = sample_start().time();

        assert_eq!(segment_file_name(time), "08-30-00.mkv");
        assert_eq!(partial_file_name(time), "08-30-00.partial.mkv");
        assert_eq!(segment_file_name_with_sequence(time, 2), "08-30-00-2.mkv");
        assert_eq!(
            partial_file_name_with_sequence(time, 12),
            "08-30-00-12.partial.mkv"
        );

        let bare = parse_segment_file_name("08-30-00.mkv").unwrap();
        assert_eq!(bare.sequence, 1);
        assert!(!bare.is_partial);
        assert_eq!(bare.started_at, time);

        let sequenced_partial = parse_segment_file_name("08-30-00-7.partial.mkv").unwrap();
        assert_eq!(sequenced_partial.sequence, 7);
        assert!(sequenced_partial.is_partial);
        assert_eq!(sequenced_partial.started_at, time);

        // Recovery rename target of a sequenced partial is the matching
        // sequenced final name.
        let partial_name = partial_file_name_with_sequence(time, 3);
        let parsed = parse_segment_file_name(&partial_name).unwrap();
        assert_eq!(
            segment_file_name_with_sequence(parsed.started_at, parsed.sequence),
            segment_file_name_with_sequence(time, 3)
        );
    }

    #[test]
    fn parse_rejects_foreign_and_noncanonical_names() {
        for bad in [
            "notes.txt",
            "video.mp4",
            "8-30-00.mkv",
            "123456.mkv",
            "08-30-00",
            "",
            "08-30-00.final.mkv",
            // sequence suffixes are canonical-only
            "08-30-00-1.mkv",   // 1 is spelled bare
            "08-30-00-01.mkv",  // leading zero
            "08-30-00-0.mkv",   // zero is never valid
            "08-30-00-x.mkv",   // not numeric
            "08-30-00-2-3.mkv", // one suffix only
        ] {
            assert!(parse_segment_file_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn allocations_in_same_second_never_collide() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        // The allocator never creates directories; emulate the recorder.
        std::fs::create_dir_all(layout.day_dir(&camera, sample_start().date())).unwrap();

        let first = layout.allocate_segment(&camera, sample_start()).unwrap();
        touch(&first.partial_path); // recorder creates the partial…

        let second = layout.allocate_segment(&camera, sample_start()).unwrap();
        touch(&second.partial_path);

        let third = layout.allocate_segment(&camera, sample_start()).unwrap();

        let names: Vec<String> = [first, second, third]
            .iter()
            .map(|p| {
                p.final_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["08-30-00.mkv", "08-30-00-2.mkv", "08-30-00-3.mkv"]
        );

        // Every allocated name still parses back to the same wall-clock start.
        for name in &names {
            let parsed = parse_segment_file_name(name).unwrap();
            assert_eq!(parsed.started_at, sample_start().time());
        }
    }

    #[test]
    fn existing_finalized_segment_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();
        // A completed recording exists — no partial anywhere.
        touch(&day_dir.join("08-30-00.mkv"));

        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert!(
            allocated
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-2.mkv")
        );
    }

    #[test]
    fn allocator_reuses_gaps_and_skips_both_name_forms() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.mkv")); // seq 1 taken (final)
        touch(&day_dir.join("08-30-00-2.partial.mkv")); // seq 2 taken (partial)

        // Gap reuse: -2 is occupied too, but an unrelated leftover such as a
        // stray -5 final would be skipped just the same.
        touch(&day_dir.join("08-30-00-5.mkv"));
        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert!(
            allocated
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-3.mkv")
        );

        // Other times of day are irrelevant to the collision check.
        let other_time = NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(8, 30, 1)
            .unwrap();
        let other = layout.allocate_segment(&camera, other_time).unwrap();
        assert!(
            other
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("08-30-01.mkv")
        );
    }

    #[test]
    fn missing_day_dir_allocates_bare_name() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            allocated.final_path.file_name().unwrap().to_string_lossy(),
            "08-30-00.mkv"
        );
        assert_eq!(
            allocated
                .partial_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "08-30-00.partial.mkv"
        );
    }

    #[test]
    fn claims_in_same_second_are_distinct_and_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let first = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            first.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00.partial.mkv"
        );

        // While the first claim is alive, a second claim for the same second
        // must land on the next sequence — never the same file.
        let second = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            second.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
        assert_ne!(first.partial_path(), second.partial_path());
        assert!(first.partial_path().is_file());
        assert!(second.partial_path().is_file());

        // Dropping a claim keeps the file (crash-recovery semantics).
        drop(second);
        let day_dir = first.partial_path().parent().unwrap().to_path_buf();
        assert!(day_dir.join("08-30-00-2.partial.mkv").is_file());
    }

    #[test]
    fn claim_skips_existing_files_without_overwriting_them() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        // Pre-existing content of both forms must survive untouched.
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("08-30-00.mkv"), b"finalized").unwrap();
        std::fs::write(day_dir.join("08-30-00-2.partial.mkv"), b"partial").unwrap();

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-3.mkv"
        );
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.mkv")).unwrap(),
            b"finalized"
        );
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00-2.partial.mkv")).unwrap(),
            b"partial"
        );
    }

    #[test]
    fn publish_no_replace_moves_content_and_refuses_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        std::fs::write(claim.partial_path(), b"segment-bytes").unwrap();

        publish_no_replace(claim.partial_path(), claim.final_path()).unwrap();
        assert!(!claim.partial_path().exists(), "partial link must be gone");
        assert_eq!(std::fs::read(claim.final_path()).unwrap(), b"segment-bytes");

        // Publishing another segment's content onto the now-existing final
        // name is refused and the existing recording stays byte-identical.
        let other = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_ne!(other.final_path(), claim.final_path());
        std::fs::write(other.partial_path(), b"other").unwrap();
        match publish_no_replace(other.partial_path(), claim.final_path()) {
            Err(StorageError::DestinationExists { destination }) => {
                assert_eq!(destination, claim.final_path().to_path_buf());
            }
            other => panic!("expected DestinationExists, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(claim.final_path()).unwrap(),
            b"segment-bytes",
            "existing finalized segment must never be replaced"
        );
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
}
