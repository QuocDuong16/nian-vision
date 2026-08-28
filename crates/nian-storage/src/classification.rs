//! Filesystem classification contract for recording-tree files (final
//! correctness remediation §6).
//!
//! Before M4 indexing/retention begins, the storage layer owns ONE parser
//! that answers what a file in the canonical recording tree IS. Recovered
//! recordings (`<stem>.recovered.mkv`) contain real footage and are
//! FIRST-CLASS recordings — M4 reconciliation must enumerate them, M4
//! retention must account and delete their bytes, and a future timeline
//! index must ingest them. Recovery scratch and tombstones are artifacts,
//! never recordings; crash partials are neither until recovery proves
//! otherwise.

use std::path::Path;

use crate::paths::{SEGMENT_EXTENSION, parse_segment_file_name};

/// What a file inside the canonical recording tree is.
///
/// The classification is purely NAME-based (the same filesystem-facts-only
/// discipline as partial scanning): content inspection belongs to the
/// media layer, enumeration/deletion decisions to M4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFileKind {
    /// A completed normal recording: `<HH-MM-SS[-N]>.mkv`.
    NormalRecording,
    /// A completed recording produced by startup recovery:
    /// `<HH-MM-SS[-N]>.recovered.mkv`. FIRST-CLASS recording — real
    /// footage published through the ordinary no-replace machinery.
    RecoveredRecording,
    /// A canonical crash partial: `<HH-MM-SS[-N]>.partial.mkv`. Neither a
    /// recording nor deletable without a recovery/retenion decision.
    ActiveOrCrashPartial,
    /// One recovery attempt's private scratch, matching the EXACT
    /// production grammar `<canonical-stem>.recovery-<pid>-<serial>
    /// -<nonce>.tmp` (final safety remediation §3). Never a recording;
    /// safe to delete only when ownership/staleness is provable (M4
    /// janitor's decision).
    RecoveryScratch,
    /// The post-publication transaction marker
    /// (`<stem>.recovered.mkv.done`). Never a recording.
    RecoveryTombstone,
    /// Anything else (operator files, foreign names, dotfiles, probes).
    /// Never deleted by the storage layer.
    Unknown,
}

/// Canonical partial names end with exactly this suffix.
const PARTIAL_NAME_SUFFIX: &str = ".partial.mkv";
/// Recovered recordings carry this infix before the extension.
const RECOVERED_NAME_SUFFIX: &str = ".recovered.mkv";
/// Tombstones append `.done` to the recovered final's name.
const TOMBSTONE_NAME_SUFFIX: &str = ".done";
/// Per-attempt recovery scratch infix (final correctness remediation §1).
const SCRATCH_INFIX: &str = ".recovery-";
/// Per-attempt recovery scratch suffix (production generator shape).
const SCRATCH_SUFFIX: &str = ".tmp";

/// Matches ONLY the exact recovery-scratch grammar the production
/// generator emits (final safety remediation §3):
/// `<canonical-stem>.recovery-<pid>-<serial>-<nonce>.tmp` — the infix
/// followed by exactly THREE non-empty all-numeric dash-separated
/// components, then the `.tmp` suffix, on a canonical `<HH-MM-SS[-N]>`
/// stem. Anything else (`.txt`/`.mkv` endings, missing or non-numeric
/// components, foreign stems) classifies Unknown: the future M4 janitor
/// must never be handed a broad matcher that could delete files Nian
/// Vision does not own.
fn is_recovery_scratch_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(SCRATCH_SUFFIX) else {
        return false;
    };
    let Some((stem, tag)) = rest.split_once(SCRATCH_INFIX) else {
        return false;
    };
    if !is_canonical_stem(stem) {
        return false;
    }
    let components: Vec<&str> = tag.split('-').collect();
    components.len() == 3
        && components
            .iter()
            .all(|component| !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit()))
}

/// Validates that `stem` has the canonical `<HH-MM-SS[-N]>` shape by
/// parsing it as the stem of a canonical segment name.
fn is_canonical_stem(stem: &str) -> bool {
    parse_segment_file_name(&format!("{stem}{PARTIAL_NAME_SUFFIX}")).is_ok()
}

/// Classifies a file NAME inside the recording tree.
pub fn classify_recording_file_name(name: &str) -> RecordingFileKind {
    // Tombstone first: `<stem>.recovered.mkv.done` also ends with `.done`
    // and must never be mistaken for a recording.
    if let Some(stem) = name
        .strip_suffix(TOMBSTONE_NAME_SUFFIX)
        .and_then(|inner| inner.strip_suffix(RECOVERED_NAME_SUFFIX))
    {
        if is_canonical_stem(stem) {
            return RecordingFileKind::RecoveryTombstone;
        }
        return RecordingFileKind::Unknown;
    }

    if let Some(base) = name.strip_suffix(PARTIAL_NAME_SUFFIX) {
        return if is_canonical_stem(base) {
            RecordingFileKind::ActiveOrCrashPartial
        } else {
            RecordingFileKind::Unknown
        };
    }

    if let Some(stem) = name.strip_suffix(RECOVERED_NAME_SUFFIX) {
        return if is_canonical_stem(stem) {
            RecordingFileKind::RecoveredRecording
        } else {
            RecordingFileKind::Unknown
        };
    }

    if is_recovery_scratch_name(name) {
        return RecordingFileKind::RecoveryScratch;
    }

    if name.ends_with(SEGMENT_EXTENSION)
        && parse_segment_file_name(name).is_ok_and(|parsed| !parsed.is_partial)
    {
        return RecordingFileKind::NormalRecording;
    }

    RecordingFileKind::Unknown
}

/// Classifies a file PATH by its file name.
pub fn classify_recording_file(path: &Path) -> RecordingFileKind {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => classify_recording_file_name(name),
        None => RecordingFileKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kind_of(name: &str) -> RecordingFileKind {
        classify_recording_file_name(name)
    }

    #[test]
    fn recovered_recordings_are_first_class_recordings() {
        assert_eq!(
            kind_of("08-30-00.recovered.mkv"),
            RecordingFileKind::RecoveredRecording
        );
        assert_eq!(
            kind_of("08-30-00-3.recovered.mkv"),
            RecordingFileKind::RecoveredRecording
        );
    }

    #[test]
    fn normal_recordings_and_partials_keep_their_kinds() {
        assert_eq!(kind_of("08-30-00.mkv"), RecordingFileKind::NormalRecording);
        assert_eq!(
            kind_of("08-30-00-2.mkv"),
            RecordingFileKind::NormalRecording
        );
        assert_eq!(
            kind_of("08-30-00.partial.mkv"),
            RecordingFileKind::ActiveOrCrashPartial
        );
        assert_eq!(
            kind_of("08-30-00-7.partial.mkv"),
            RecordingFileKind::ActiveOrCrashPartial
        );
    }

    #[test]
    fn recovery_artifacts_are_never_recordings() {
        assert_eq!(
            kind_of("08-30-00.recovered.mkv.done"),
            RecordingFileKind::RecoveryTombstone
        );
        // The EXACT production scratch grammar: three numeric components.
        assert_eq!(
            kind_of("08-30-00.recovery-4194305-0-123456789.tmp"),
            RecordingFileKind::RecoveryScratch
        );
        assert_eq!(
            kind_of("08-30-00-3.recovery-1-42-999999999.tmp"),
            RecordingFileKind::RecoveryScratch
        );
    }

    #[test]
    fn scratch_classification_matches_only_the_exact_production_grammar() {
        // Final safety remediation §3: RecoveryScratch must never hand the
        // future M4 janitor a broad matcher. Every deviation from the
        // generator's shape is Unknown.
        assert_eq!(
            kind_of("08-30-00.recovery-not-ours.txt"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of("08-30-00.recovery-123.mkv"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of("08-30-00.recovery-.tmp"),
            RecordingFileKind::Unknown
        );
        // Wrong component counts / non-numeric components.
        assert_eq!(
            kind_of("08-30-00.recovery-123-456.tmp"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of("08-30-00.recovery-1-2-3-4.tmp"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of("08-30-00.recovery-12a-3-4.tmp"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of("08-30-00.recovery--1-2.tmp"),
            RecordingFileKind::Unknown
        );
        // A non-canonical stem is foreign even with the right tail.
        assert_eq!(
            kind_of("holiday-video.recovery-1-2-3.tmp"),
            RecordingFileKind::Unknown
        );
        // A second infix makes the tag non-numeric.
        assert_eq!(
            kind_of("08-30-00.recovery-1.recovery-2-3-4.tmp"),
            RecordingFileKind::Unknown
        );
    }

    #[test]
    fn foreign_and_probe_names_are_unknown() {
        assert_eq!(kind_of("notes.txt"), RecordingFileKind::Unknown);
        assert_eq!(kind_of("not-a-segment.mkv"), RecordingFileKind::Unknown);
        assert_eq!(
            kind_of("12-00-00.recovered.mkv.doneX"),
            RecordingFileKind::Unknown
        );
        assert_eq!(
            kind_of(".nian-write-probe-123-456.tmp"),
            RecordingFileKind::Unknown
        );
        // Malformed time stems never become recordings or partials.
        assert_eq!(
            kind_of("99-99-99.recovered.mkv"),
            RecordingFileKind::Unknown
        );
        assert_eq!(kind_of("ab-cd-ef.mkv"), RecordingFileKind::Unknown);
    }

    #[test]
    fn classifier_works_on_paths() {
        let path = PathBuf::from("/records/cam-1/2026/08/27/08-30-00.recovered.mkv");
        assert_eq!(
            classify_recording_file(&path),
            RecordingFileKind::RecoveredRecording
        );
        assert_eq!(
            classify_recording_file(&PathBuf::from("/")),
            RecordingFileKind::Unknown
        );
    }
}
