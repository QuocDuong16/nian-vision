//! Conservative startup reconciliation for `.partial.mkv` files (M3 §11).
//!
//! After a crash or forced shutdown, `.partial.mkv` files remain in the
//! canonical recording layout. Before the recorder (or a supervisor) starts
//! new work, those leftovers must be classified so that:
//!
//! * nothing is ever renamed straight to its final name merely because the
//!   *filename* looks valid — only proven, readable media content may be
//!   salvaged, and even then through a full remux + no-replace publication
//!   driven by the recorder crate;
//! * genuinely unreadable partials are not silently deleted; they are
//!   reported and left in place (or quarantined by the caller);
//! * finalized content that failed to publish earlier is recognized as such
//!   where determinable and re-offered for publication.
//!
//! This module deliberately contains NO FFmpeg code: classification happens
//! on filesystem facts (size, canonical name, byte-level Matroska structure
//! sniffing). The actual salvage/remux lives in `nian-recorder`, which owns
//! the media stack.
//!
//! The filesystem stays authoritative — no SQLite yet.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use nian_domain::CameraId;

use crate::error::StorageError;
use crate::paths::{ParsedSegmentName, parse_segment_file_name};

/// Minimum number of bytes for a partial file to be worth media-level
/// inspection. The EBML/Matroska magic plus one cluster header needs more
/// than this; anything smaller cannot possibly be readable media.
const MIN_MEDIA_CANDIDATE_BYTES: u64 = 64;

/// Canonical Matroska/EBML signature bytes: the EBML header element ID
/// (`0x1A45DFA3`) followed by the doc-type position where "matroska" must
/// appear. This is a cheap structural sniff, NOT a validation — real
/// readability is proven later by opening the file with the demuxer.
const EBML_HEADER_ID: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];

/// What this process should do with a discovered partial file.
///
/// The disposition classifies FIRST at byte/name level (cheap, safe); the
/// expensive proof (demux it) belongs to the recovery engine in
/// `nian-recorder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialDisposition {
    /// Empty or too small to carry any Matroska structure. Safe to delete,
    /// but deletion remains the CALLER's decision (this module never removes
    /// files) — conservative reporting first.
    EmptyOrHeaderOnly,

    /// Carries plausible EBML structure and must go through media-level
    /// recovery: remux readable packets into a fresh claimed segment,
    /// finalize, publish no-replace, and only then remove the original.
    /// Recovery must begin on a valid primary-video keyframe inside the
    /// partial.
    RecoverableMedia { size_bytes: u64 },

    /// A fully finalized recording stuck under a `.partial` name because
    /// publication previously failed after a successful trailer. Where the
    /// Matroska "duration" cues exist and the muxer's final flush completed,
    /// direct no-replace publication of the existing bytes MAY succeed; if
    /// it collides with an existing final, the media-level path still
    /// applies. Recognizing this state is best-effort: `parse` proves EBML
    /// and Segment structure, but callers must treat publication failure as
    /// falling back to media recovery, never as evidence of corruption.
    FinalizedButUnpublished { size_bytes: u64 },
}

/// One discovered leftover partial file with everything recovery needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialFile {
    /// Path of the `<HH-MM-SS[-N]>.partial.mkv` file.
    pub partial_path: PathBuf,
    /// Where a recovered/finalized recording would be published
    /// (`<HH-MM-SS[-N]>.mkv` next to the partial).
    pub final_path: PathBuf,
    /// Parsed name components (start time-of-day, sequence).
    pub parsed: ParsedSegmentName,
    /// What this process should attempt with the file.
    pub disposition: PartialDisposition,
}

impl PartialFile {
    /// The wall-clock start timestamp implied by the partial's day directory
    /// and its name. Used by recovery to claim a properly-named output slot;
    /// reconstructing from disk layout keeps names truthful without trusting
    /// an index (there is none yet).
    pub fn started_at(&self) -> Result<NaiveDateTime, StorageError> {
        let Some(name) = self.partial_path.file_name().and_then(|n| n.to_str()) else {
            return Err(StorageError::UnrecognizedSegmentName {
                name: String::new(),
            });
        };
        let parsed = parse_segment_file_name(name)?;
        // The parent directories encode <camera>/YYYY/MM/DD.
        let dir =
            self.partial_path
                .parent()
                .ok_or_else(|| StorageError::UnrecognizedSegmentName {
                    name: name.to_owned(),
                })?;
        let date = dir_date(dir).ok_or_else(|| StorageError::UnrecognizedSegmentName {
            name: name.to_owned(),
        })?;
        Ok(date.and_time(parsed.started_at))
    }
}

/// Extracts YYYY/MM/DD from `<…>/<year>/<month>/<day>`-shaped parents.
fn dir_date(day_dir: &Path) -> Option<NaiveDate> {
    let day = day_dir.file_name()?.to_str()?.parse::<u32>().ok()?;
    let month_dir = day_dir.parent()?;
    let month = month_dir.file_name()?.to_str()?.parse::<u32>().ok()?;
    let year_dir = month_dir.parent()?;
    let year = year_dir.file_name()?.to_str()?.parse::<i32>().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Scans ONE camera's recording tree for leftover partial files.
///
/// Only canonical layout paths under `<root>/<camera-id>/**/` are visited —
/// arbitrary user-supplied filenames are never trusted, every candidate must
/// pass [`parse_segment_file_name`] with `is_partial == true`. Files whose
/// names do not parse are IGNORED here (not errors): the storage layer never
/// deletes what it does not understand; a stricter janitor belongs to M4
/// retention.
///
/// Returns candidates sorted deterministically (by path) so recovery runs
/// are reproducible. The scan creates and deletes nothing.
pub fn scan_camera_partials(
    layout: &crate::paths::RecordingsLayout,
    camera: &CameraId,
) -> Result<Vec<PartialFile>, StorageError> {
    let camera_root = layout.camera_dir(camera);
    let mut found: Vec<PartialFile> = Vec::new();

    // Walk depth-first; missing camera root simply means "no leftovers".
    let mut stack = vec![camera_root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StorageError::Io { path: dir, source });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| StorageError::Io {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) => classify_candidate(path, &mut found)?,
                Err(source) => {
                    return Err(StorageError::Io {
                        path: dir.clone(),
                        source,
                    });
                }
            }
        }
    }

    found.sort_by(|a, b| a.partial_path.cmp(&b.partial_path));
    Ok(found)
}

/// Classifies one path (if it is a canonical partial name) and appends the
/// result. Non-canonical names are skipped silently per module contract.
fn classify_candidate(path: PathBuf, out: &mut Vec<PartialFile>) -> Result<(), StorageError> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let Ok(parsed) = parse_segment_file_name(name) else {
        return Ok(()); // foreign name: ignore, never delete what we don't parse
    };
    if !parsed.is_partial || !path.is_file() {
        return Ok(());
    }

    let metadata = std::fs::metadata(&path).map_err(|source| StorageError::Io {
        path: path.clone(),
        source,
    })?;
    let size_bytes = metadata.len();
    let final_path = match path.parent() {
        Some(parent) => parent.join(crate::paths::segment_file_name_with_sequence(
            parsed.started_at,
            parsed.sequence,
        )),
        None => return Ok(()), // unreachable for names with separators
    };

    let disposition = if size_bytes < MIN_MEDIA_CANDIDATE_BYTES {
        PartialDisposition::EmptyOrHeaderOnly
    } else {
        match sniff_matroska(&path, size_bytes) {
            MatroskaShape::FinalizedLooksComplete => {
                PartialDisposition::FinalizedButUnpublished { size_bytes }
            }
            MatroskaShape::TruncatedMedia | MatroskaShape::UnknownPayload => {
                PartialDisposition::RecoverableMedia { size_bytes }
            }
        }
    };

    out.push(PartialFile {
        partial_path: path,
        final_path,
        parsed,
        disposition,
    });
    Ok(())
}

/// Byte-level structural verdict of a partial file.
enum MatroskaShape {
    /// EBML header present AND the Cues trailer element near the end — the
    /// strongest available hint that `finalize()` ran before the crash or
    /// publication gap.
    FinalizedLooksComplete,
    /// EBML header present but no Cues element in the tail: truncated
    /// mid-cluster media, salvageable only by remuxing readable packets.
    TruncatedMedia,
    /// Bytes exist but not even the EBML magic is at offset 0 — garbage
    /// payload. Media-level recovery will refuse it; the file stays
    /// quarantined rather than deleted.
    UnknownPayload,
}

/// The Cues (seek index) master element ID — written only by Matroska
/// finalization (`av_write_trailer`), never while clusters stream.
const CUES_ELEMENT_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];

/// Reads just enough of the file to answer which [`MatroskaShape`] it has.
///
/// The EBML magic at offset 0 proves the file came from a Matroska writer;
/// the Cues element ID in the last few KiB proves finalization completed.
/// Anything ambiguous conservatively lands in
/// [`MatroskaShape::TruncatedMedia`] — the recovery engine's demux step
/// provides the real verdict; this sniff only routes, never deletes.
fn sniff_matroska(path: &Path, size_bytes: u64) -> MatroskaShape {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return MatroskaShape::UnknownPayload;
    };

    let mut head = [0_u8; EBML_HEADER_ID.len()];
    if file.read_exact(&mut head).is_err() || head != EBML_HEADER_ID {
        return MatroskaShape::UnknownPayload;
    }

    const TAIL_LEN: u64 = 4096;
    let tail_start = size_bytes.saturating_sub(TAIL_LEN);
    if file.seek(SeekFrom::Start(tail_start)).is_err() {
        return MatroskaShape::TruncatedMedia;
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        return MatroskaShape::TruncatedMedia;
    }

    if tail
        .windows(CUES_ELEMENT_ID.len())
        .any(|window| window == CUES_ELEMENT_ID)
    {
        MatroskaShape::FinalizedLooksComplete
    } else {
        MatroskaShape::TruncatedMedia
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::RecordingsLayout;

    fn layout_in(root: &Path) -> RecordingsLayout {
        RecordingsLayout::new(root).unwrap()
    }

    const CAMERA: &str = "cam-1";

    /// Writes a minimal fake "matroska-shaped" payload: EBML magic at 0,
    /// optional Cues id near the end. NOT a valid file for demuxing — the
    /// recovery engine's media step is what truly proves readability; these
    /// bytes only exercise classification.
    fn write_shaped(path: &Path, with_cues: bool, padding: usize) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&EBML_HEADER_ID);
        bytes.extend(std::iter::repeat_n(0xAB_u8, padding));
        if with_cues {
            // Simulate a trailer: place the Cues element id within the last
            // 4 KiB (here: immediately at the end).
            bytes.extend_from_slice(&CUES_ELEMENT_ID);
            bytes.extend_from_slice(&[0x00, 0x01, 0x02, 0x03]);
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn day_dir(layout: &RecordingsLayout) -> PathBuf {
        let camera = CameraId::parse(CAMERA).unwrap();
        let dir = layout.day_dir(&camera, NaiveDate::from_ymd_opt(2026, 8, 27).unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn zero_byte_and_tiny_partials_classify_as_empty_or_header_only() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        std::fs::write(day.join("08-30-00.partial.mkv"), b"").unwrap();
        write_shaped(&day.join("08-30-01.partial.mkv"), false, 8); // < MIN bytes

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert_eq!(found.len(), 2);
        assert!(
            found
                .iter()
                .all(|p| p.disposition == PartialDisposition::EmptyOrHeaderOnly)
        );
    }

    #[test]
    fn readable_media_partial_classifies_as_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        write_shaped(&day.join("09-00-00.partial.mkv"), false, 4096);
        // The sequenced partial stays truncated (no Cues) on purpose.
        write_shaped(&day.join("09-05-00-2.partial.mkv"), false, 8000);

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert_eq!(found.len(), 2);
        for partial in &found {
            match &partial.disposition {
                PartialDisposition::RecoverableMedia { size_bytes } => {
                    assert!(size_bytes > &MIN_MEDIA_CANDIDATE_BYTES)
                }
                other => panic!("expected RecoverableMedia, got {other:?}"),
            }
        }
    }

    #[test]
    fn finalized_looking_partial_classifies_as_finalized_but_unpublished() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        write_shaped(&day.join("10-00-00.partial.mkv"), true, 9000);

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].disposition,
            PartialDisposition::FinalizedButUnpublished { size_bytes: 9012 }
        );
        // The final target name matches sequence 1 exactly.
        assert_eq!(
            found[0].final_path.file_name().unwrap().to_string_lossy(),
            "10-00-00.mkv"
        );
    }

    #[test]
    fn garbage_payload_sniffs_as_recoverable_media_for_media_level_refusal() {
        // Non-EBML bytes above the size threshold must NOT be offered as
        // finalized; they land in RecoverableMedia and it is the DEMUX step
        // that refuses them (keeping the file quarantined).
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        std::fs::write(day.join("11-00-00.partial.mkv"), vec![0x07_u8; 512]).unwrap();

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].disposition,
            PartialDisposition::RecoverableMedia { size_bytes: 512 }
        );
    }

    #[test]
    fn foreign_names_and_finals_are_ignored_never_deleted_or_reported() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        std::fs::write(day.join("notes.txt"), b"ignore me").unwrap();
        std::fs::write(day.join("12-00-00.mkv"), b"a finished recording").unwrap();
        std::fs::write(day.join("not-a-segment.mkv"), [0x1A_u8, 0x45].repeat(150)).unwrap();

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert!(
            found.is_empty(),
            "foreign/final files must be invisible to reconciliation"
        );
        // Nothing was deleted.
        assert!(day.join("notes.txt").is_file());
        assert!(day.join("12-00-00.mkv").is_file());
    }

    #[test]
    fn missing_camera_tree_scans_to_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let found = scan_camera_partials(&layout, &CameraId::parse("ghost-cam").unwrap()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn started_at_reconstructs_from_layout_and_name() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        write_shaped(&day.join("13-45-30.partial.mkv"), false, 200);
        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        assert_eq!(found.len(), 1);

        let started = found[0].started_at().unwrap();
        assert_eq!(
            started,
            NaiveDate::from_ymd_opt(2026, 8, 27)
                .unwrap()
                .and_hms_opt(13, 45, 30)
                .unwrap()
        );
    }

    #[test]
    fn results_are_deterministically_sorted() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let day = day_dir(&layout);

        write_shaped(&day.join("15-00-00.partial.mkv"), false, 200);
        write_shaped(&day.join("14-00-00.partial.mkv"), false, 200);
        let day2_root = layout.day_dir(
            &CameraId::parse(CAMERA).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
        );
        std::fs::create_dir_all(&day2_root).unwrap();
        write_shaped(&day2_root.join("23-59-59.partial.mkv"), false, 200);

        let found = scan_camera_partials(&layout, &CameraId::parse(CAMERA).unwrap()).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.partial_path.to_string_lossy().into_owned())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
