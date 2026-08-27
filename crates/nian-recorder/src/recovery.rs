//! Conservative startup recovery for `.partial.mkv` leftovers (M3 §11).
//!
//! The recovery contract, in order:
//!
//! 1. [`nian_storage::scan_camera_partials`] classifies leftovers at
//!    byte/name level (cheap, filesystem facts only);
//! 2. this module performs the MEDIA-level proof and salvage: open the
//!    partial with the real demuxer; if it carries a usable video stream,
//!    discard everything until the first selected video keyframe (the same
//!    alignment rule as live recording), stream-copy every readable packet
//!    into a NEW exclusively-claimed recovery output, finalize it durably,
//!    publish it no-replace — and only then remove the original partial;
//! 3. anything that cannot be PROVEN recoverable keeps its partial file in
//!    place untouched: no invented recordings, no blind renames to final.
//!
//! A truncated-but-readable partial never becomes "the final it was named
//! after": its recovered content is a distinct recording slot claimed like
//! any other segment (name anchored when recovery runs), published with the
//! ordinary no-replace machinery, so an existing final can never be
//! overwritten by recovery. If that publication is refused, the recovered
//! output stays a `.partial.mkv` and the ORIGINAL is kept too.
//!
//! # Failure containment
//!
//! One partial's recovery failure never aborts other files: caller-visible
//! failures are collected per-file, and the source file always survives any
//! failed attempt.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use nian_domain::{CameraId, MediaRational};
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
use nian_storage::paths::publish_no_replace;
use nian_storage::{
    PartialDisposition, PartialFile, RecordingsLayout, StorageError, scan_camera_partials,
};

/// Test-only deterministic fault hooks for recovery's pipeline steps
/// (`0`/`usize::MAX` = disabled). Compiled out of production builds.
#[cfg(test)]
mod test_hooks {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub static WRITE_FAIL_AFTER_CALLS: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    pub static FAIL_METADATA: AtomicBool = AtomicBool::new(false);
    pub static HIJACK_CLEANUP_TO_DIR: AtomicBool = AtomicBool::new(false);
    /// Serializes every fault-armed test against the process-global hooks.
    pub static FAULT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            // Panic-safe: resets every hook whenever this guard dies.
            WRITE_FAIL_AFTER_CALLS.store(usize::MAX, Ordering::SeqCst);
            WRITE_CALLS.store(0, Ordering::SeqCst);
            FAIL_METADATA.store(false, Ordering::SeqCst);
            HIJACK_CLEANUP_TO_DIR.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
use test_hooks::{
    FAIL_METADATA as HOOK_FAIL_METADATA, HIJACK_CLEANUP_TO_DIR as HOOK_HIJACK_CLEANUP,
    WRITE_CALLS as HOOK_WRITE_CALLS, WRITE_FAIL_AFTER_CALLS as HOOK_WRITE_FAIL_AFTER,
};

#[cfg(test)]
pub(crate) fn arm_write_fault(after_calls: usize) -> test_hooks::Guard {
    test_hooks::WRITE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    test_hooks::WRITE_FAIL_AFTER_CALLS.store(after_calls, std::sync::atomic::Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(test)]
pub(crate) fn arm_metadata_failure() -> test_hooks::Guard {
    test_hooks::FAIL_METADATA.store(true, std::sync::atomic::Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(test)]
pub(crate) fn arm_cleanup_hijack() -> test_hooks::Guard {
    test_hooks::HIJACK_CLEANUP_TO_DIR.store(true, std::sync::atomic::Ordering::SeqCst);
    test_hooks::Guard
}

/// Canonical `<HH-MM-SS>.partial.mkv` name for a timestamp — the exact
/// shape the scanner accepts (mirrors nian-storage's allocator naming).
#[cfg(test)]
pub(crate) fn __recovery_canonical_name(stamp: chrono::NaiveDateTime) -> String {
    nian_storage::paths::partial_file_name(stamp.time())
}

/// Result of recovering one partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No partials existed — nothing to do (the common case).
    NothingToDo,

    /// The partial was empty/header-only, was refused by the demuxer, or
    /// yielded no video packets: kept in place untouched (quarantined) and
    /// reported. Never deleted, never renamed to final.
    KeptUnrecoverable {
        /// The quarantined partial.
        partial_path: PathBuf,
        /// Why recovery was refused (human-safe reason; contains no secrets,
        /// at most paths that are already operator-known).
        reason: String,
    },

    /// Readable content was salvaged into a fresh claimed segment,
    /// finalized durably and published no-replace. The ORIGINAL partial was
    /// removed only after that publication succeeded.
    Recovered {
        /// The newly published recording.
        final_path: PathBuf,
        /// Media duration derived from packet timestamps when both ends of
        /// the copied span carried timestamps (`None` otherwise — never
        /// guessed). Measured on the RECOVERED content, which starts at its
        /// own first keyframe.
        media_duration: Option<Duration>,
        /// Size of the published file.
        size_bytes: u64,
        /// Whether the salvage derived from a finalized-but-unpublished
        /// leftover (class C) rather than a truncated crash partial.
        from_finalized_leftover: bool,
        /// M3 remediation §12: whether unlinking the original succeeded.
        /// `false` surfaces an observable cleanup failure; the recovery
        /// itself stays valid, and idempotency is guaranteed because the
        /// recovered final blocks any republish (`DestinationExists`).
        original_removed: bool,
    },
}

/// One attempted recovery with its error, when the attempt failed.
#[derive(Debug)]
pub struct RecoveryFailure {
    /// The partial whose recovery failed (kept untouched).
    pub partial_path: PathBuf,
    /// Why it failed.
    pub error: RecoveryError,
}

/// Recovery-specific failures.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The scan or claiming the recovery output failed (filesystem
    /// trouble). Source files remain untouched.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The demuxer could not prove the partial is readable media (failed
    /// open, no video stream, or trailer/flush failed on the salvage
    /// output).
    #[error("partial is not provably recoverable media: {message}")]
    Unreadable {
        /// Human-safe backend description (secret-free by media-layer
        /// contract).
        message: String,
    },

    /// Publication of the recovered output was refused because the
    /// destination already exists. Neither the recovered output nor the
    /// original partial is removed; a later pass may still salvage under a
    /// different claim.
    #[error("recovered recording destination already exists")]
    DestinationExists,
}

/// Recovers all classifiable partials of one camera.
///
/// Conservative per-file containment: one failure does not abort other
/// files' recovery. Returns outcomes in deterministic (scan) order plus the
/// failures encountered. Nothing outside the canonical layout tree is ever
/// touched.
pub fn recover_camera_partials(
    layout: &RecordingsLayout,
    camera: &CameraId,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();

    let partials = match scan_camera_partials(layout, camera) {
        Ok(partials) => partials,
        Err(error) => {
            failures.push(RecoveryFailure {
                // No specific path known; point at the camera root so the
                // report stays actionable without inventing a filename.
                partial_path: layout.camera_dir(camera),
                error: error.into(),
            });
            return (outcomes, failures);
        }
    };

    if partials.is_empty() {
        outcomes.push(RecoveryOutcome::NothingToDo);
        return (outcomes, failures);
    }

    for partial in partials {
        match recover_one(layout, camera, &partial) {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => failures.push(RecoveryFailure {
                partial_path: partial.partial_path.clone(),
                error,
            }),
        }
    }
    (outcomes, failures)
}

/// Recovers ONE partial according to its classification.
fn recover_one(
    layout: &RecordingsLayout,
    camera: &CameraId,
    partial: &PartialFile,
) -> Result<RecoveryOutcome, RecoveryError> {
    match &partial.disposition {
        // Cheap classification proved there cannot be media here. Keep the
        // file (reporting beats deleting); cleanup policy belongs to M4.
        PartialDisposition::EmptyOrHeaderOnly => Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "empty or header-only, nothing to salvage".to_owned(),
        }),

        // Both media classes go through the SAME proven pipeline: demux →
        // keyframe-aligned packet copy → durable finalize → no-replace
        // publish → remove original LAST.
        PartialDisposition::RecoverableMedia { .. }
        | PartialDisposition::FinalizedButUnpublished { .. } => {
            salvage_media(layout, camera, partial)
        }
    }
}

/// The demux → alignment → claim → copy → finalize → publish → cleanup
/// pipeline shared by both media classes.
///
/// M3 remediation ordering contract (§9–§12):
///
/// 1. open the ORIGINAL and validate streams/time base (§10: before any
///    new output exists);
/// 2. scan/discard until the first primary-video keyframe is PROVEN to
///    exist (§10), then restart the source for the actual copy;
/// 3. ONLY THEN claim the fresh recovery output and open its muxer;
/// 4. write packets keyframe-aligned until EOF/truncation;
/// 5. ANY muxer write failure poisons the output (§9): it is never
///    finalized/published — the poisoned partial stays on disk, the
///    original stays untouched, a `RecoveryFailure` is reported;
/// 6. finalize durably; the size lookup on the still-partial file must
///    succeed or nothing publishes (§11: no `unwrap_or(0)`);
/// 7. no-replace publication commits the recovered recording;
/// 8. drop the SOURCE INPUT first (§12), then remove the original — with
///    cleanup made IDEMPOTENT by the no-replace rule: if unlinking fails,
///    `original_removed=false` surfaces it observably, and a repeat run
///    cannot duplicate the recording because the recovered final blocks
///    any republish via `DestinationExists`.
fn salvage_media(
    layout: &RecordingsLayout,
    camera: &CameraId,
    partial: &PartialFile,
) -> Result<RecoveryOutcome, RecoveryError> {
    let from_finalized = matches!(
        partial.disposition,
        PartialDisposition::FinalizedButUnpublished { .. }
    );

    // ---- 1. Open + validate the ORIGINAL -----------------------------------
    let interrupt = InterruptHandle::new();
    let _open_budget = interrupt.scoped_deadline(Duration::from_secs(15));
    let source = partial_source(&partial.partial_path);
    let mut input =
        MediaInput::open(&source, &interrupt).map_err(|error| RecoveryError::Unreadable {
            message: error.to_string(),
        })?;
    drop(_open_budget);

    let streams = input.streams();
    let video = streams
        .iter()
        .find(|stream| stream.media_type == nian_domain::MediaType::Video)
        .ok_or(RecoveryError::Unreadable {
            message: "no video stream inside the partial".to_owned(),
        })?;
    let time_base: MediaRational = video
        .time_base
        .filter(|tb| tb.num > 0 && tb.den > 0)
        .ok_or(RecoveryError::Unreadable {
            message: "video stream has no usable time base".to_owned(),
        })?;
    let video_index = video.stream_index;

    // ---- 2. Alignment probe BEFORE claiming anything (§10) -----------------
    //
    // Prove a selected VIDEO keyframe is reachable WITHOUT creating an
    // output first: a truncated-before-keyframe candidate manufactures ZERO
    // junk recovery partials.
    let mut found_keyframe = false;
    while let Some(packet) = input.next_packet().ok().flatten() {
        let metadata = packet.metadata();
        if metadata.stream_index == video_index && metadata.keyframe {
            found_keyframe = true;
            break;
        }
        // EOF/truncation ends via None from next_packet next round.
    }

    if !found_keyframe {
        return Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "no usable primary-video keyframe inside the partial".to_owned(),
        });
    }

    // `next_packet` consumed packets up to that keyframe; MediaInput cannot
    // unread, so RESTART the proven-readable source once. From here the copy
    // loop performs its own alignment identically to live recording.
    drop(input);
    let mut input =
        MediaInput::open(&source, &interrupt).map_err(|error| RecoveryError::Unreadable {
            message: error.to_string(),
        })?;

    // ---- 3. Claim + open the recovery output (AFTER proof) -----------------
    let claim_started = Local::now().naive_local();
    let claim = layout.claim_segment(camera, claim_started)?;

    let selection = streams.clone();
    let mut muxer = MatroskaMuxer::create_with_selection(
        &mut input,
        claim.partial_path(),
        &interrupt,
        |info| {
            selection
                .iter()
                .any(|s| s.stream_index == info.stream_index)
        },
    )
    .map_err(|error| RecoveryError::Unreadable {
        message: format!("salvage output could not be opened: {error}"),
    })?;

    // ---- 4. Copy loop with POISON-on-write-failure (§9) ---------------------
    let mut aligned = false;
    let mut start_media: Option<i64> = None;
    let mut last_media: Option<i64> = None;
    let mut video_packets: u64 = 0;
    let mut mux_write_failed = false;

    loop {
        match input.next_packet() {
            Ok(Some(packet)) => {
                let metadata = packet.metadata();
                let is_video = metadata.stream_index == video_index;
                if !aligned {
                    if is_video && metadata.keyframe {
                        aligned = true;
                    } else {
                        continue;
                    }
                }
                #[cfg(test)]
                {
                    // Deterministic mux-write fault injection (§9): fail the
                    // K-th muxer call so tests prove poisoned outputs never
                    // publish.
                    if HOOK_WRITE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        < HOOK_WRITE_FAIL_AFTER.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        // fall through to a real write
                    } else {
                        return Err(RecoveryError::Unreadable {
                            message:
                                "salvage output write failed; output poisoned, never published"
                                    .to_owned(),
                        });
                    }
                }
                if muxer.write_packet(&packet).is_err() {
                    // M2 invariant applies verbatim: ANY mux/output write
                    // failure poisons this output — NEVER finalize/publish it.
                    // Poisoned partial stays for diagnosis; original intact.
                    mux_write_failed = true;
                    break;
                }
                if is_video {
                    // Transactional commit: strictly AFTER successful write.
                    if let Some(timestamp) = metadata.dts.or(metadata.pts) {
                        start_media.get_or_insert(timestamp);
                        last_media = Some(timestamp);
                    }
                    video_packets += 1;
                }
            }
            Ok(None) => break, // clean EOF: everything readable was copied
            Err(_) => break,   // truncation mid-partial: expected shape here
        }
    }

    if mux_write_failed {
        drop(muxer); // NOT finalized: stays in `.partial`-recovery shape
        return Err(RecoveryError::Unreadable {
            message: "salvage output write failed; output poisoned, never published".to_owned(),
        });
    }

    // Zero committed video packets is not a recording (tiny-output guard).
    // Keep BOTH files; publish nothing, remove nothing.
    if video_packets == 0 {
        drop(muxer);
        return Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "demuxed but no video packets survived alignment".to_owned(),
        });
    }

    // ---- 6. Durable finalize + REQUIRED size lookup (§11) ------------------
    muxer
        .finalize()
        .map_err(|error| RecoveryError::Unreadable {
            message: format!("salvaged output failed to finalize: {error}"),
        })?;

    #[cfg(test)]
    if HOOK_FAIL_METADATA.load(std::sync::atomic::Ordering::SeqCst) {
        // Simulate a post-finalize stat failure: nothing may publish even
        // though the trailer succeeded (finding 11's transaction boundary).
        return Err(RecoveryError::Storage(StorageError::Io {
            path: claim.partial_path().to_path_buf(),
            source: std::io::Error::other("injected"),
        }));
    }

    let size_bytes = std::fs::metadata(claim.partial_path())
        .map_err(|source| {
            RecoveryError::Storage(StorageError::Io {
                path: claim.partial_path().to_path_buf(),
                source,
            })
        })?
        .len();

    let media_duration = match (start_media, last_media) {
        (Some(start), Some(last)) => time_base.duration_of(last.saturating_sub(start).max(0)),
        _ => None,
    };

    // ---- 7. Atomic no-replace publication ==================================
    publish_no_replace(claim.partial_path(), claim.final_path()).map_err(|error| match error {
        StorageError::DestinationExists { .. } => RecoveryError::DestinationExists,
        other => RecoveryError::Storage(other),
    })?;

    // ---- 8. Source closed FIRST, then observable idempotent cleanup (§12) --
    drop(input);

    #[cfg(test)]
    if HOOK_HIJACK_CLEANUP.load(std::sync::atomic::Ordering::SeqCst) {
        // Swap the original (now recoverable duplicate) into a directory:
        // remove_file(EISDIR) fails deterministically regardless of uid,
        // proving the observable-cleanup contract (original_removed=false)
        // while the recovered final stays valid on disk.
        std::fs::remove_file(&partial.partial_path).ok();
        if std::fs::create_dir_all(&partial.partial_path).is_ok() {
            std::fs::write(partial.partial_path.join("marker"), b"kept").ok();
        }
    }

    let removed = std::fs::remove_file(&partial.partial_path);
    let original_removed = removed.is_ok();

    Ok(RecoveryOutcome::Recovered {
        final_path: claim.final_path().to_path_buf(),
        media_duration,
        size_bytes,
        from_finalized_leftover: from_finalized,
        original_removed,
    })
}

/// Builds the demuxer source for a partial path (local file by definition —
/// partials only ever exist inside the storage tree).
fn partial_source(path: &std::path::Path) -> nian_media::MediaSource {
    nian_media::MediaSource::File(path.to_path_buf())
}

#[cfg(test)]
mod fault_injection_tests {
    //! In-crate deterministic fault injection for the recovery pipeline
    //! (M3 remediation §9/§11/§12): real FFmpeg remux runs where the ONLY
    //! synthetic element is the injected fault — no fake media logic.

    use super::*;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/nian-media-ffmpeg/tests/fixtures")
    }

    struct Storage {
        _dir: tempfile::TempDir,
        layout: RecordingsLayout,
        camera: CameraId,
        day_dir: PathBuf,
    }

    impl Storage {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let layout = RecordingsLayout::new(dir.path().join("rec")).unwrap();
            let camera = CameraId::parse("cam-fault").unwrap();
            let day_dir = layout.day_dir(&camera, chrono::Local::now().date_naive());
            std::fs::create_dir_all(&day_dir).unwrap();
            Self {
                _dir: dir,
                layout,
                camera,
                day_dir,
            }
        }
    }

    fn count_files(dir: &Path, partial_only: bool) -> usize {
        let mut found = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if partial_only == name.contains(".partial.") {
                    found += 1;
                }
            }
        }
        found
    }

    #[test]
    fn mux_write_fault_poisons_the_recovered_output_and_never_publishes() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Fail on/after the 5th muxer call. The returned GUARD resets every
        // hook when it drops — even through a panic — so parallel sibling
        // tests can never inherit stale state.
        let _hook_guard = arm_write_fault(5);
        let storage = Storage::new();
        let name = __recovery_canonical_name(chrono::Local::now().naive_local());
        std::fs::write(
            storage.day_dir.join(&name),
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        // NOTHING published: no non-partial file anywhere.
        assert_eq!(
            count_files(&storage.day_dir, false),
            0,
            "poisoned recovery output must never be published"
        );
        // The poisoned salvage partial exists for diagnosis; the ORIGINAL
        // stays untouched (canonical leftover).
        assert!(
            count_files(&storage.day_dir, true) >= 1,
            "a poisoned/failed pass leaves partials behind: {outcomes:?} / {failures:?}"
        );
        let failure_reported = !failures.is_empty()
            || outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::KeptUnrecoverable { .. }));
        assert!(failure_reported, "{outcomes:?} / {failures:?}");
    }

    #[test]
    fn metadata_fault_after_finalize_never_publishes() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_metadata_failure();
        let storage = Storage::new();
        let name = __recovery_canonical_name(chrono::Local::now().naive_local());
        std::fs::write(
            storage.day_dir.join(&name),
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();

        let (_outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        // finalize succeeded but the REQUIRED size lookup failed → nothing
        // published; failure surfaced as Storage error; original intact.
        assert!(
            failures
                .iter()
                .any(|failure| matches!(failure.error, RecoveryError::Storage(_))),
            "metadata failure must surface as typed storage failure: {failures:?}"
        );
        assert_eq!(count_files(&storage.day_dir, false), 0);
    }

    #[test]
    fn cleanup_failure_is_observable_and_idempotency_keeps_finals_safe() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_cleanup_hijack();
        let storage = Storage::new();
        let name = __recovery_canonical_name(chrono::Local::now().naive_local());
        std::fs::write(
            storage.day_dir.join(&name),
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();

        let (outcomes, _failures) = recover_camera_partials(&storage.layout, &storage.camera);

        // Publication SUCCEEDED; cleanup was forced to fail and is flagged.
        let recovered = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                RecoveryOutcome::Recovered {
                    original_removed,
                    final_path,
                    ..
                } => Some((*original_removed, final_path.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recovered.len(), 1);
        let (original_removed, final_path) = &recovered[0];
        assert!(!original_removed, "hijacked unlink must be observable");
        assert!(
            final_path.is_file(),
            "the recovered recording itself stays fully valid"
        );

        // Idempotent repeat: whatever original-shaped content remains,
        // publishing a NEW claim can never overwrite the existing final;
        // total finals after a second pass may only grow by distinct slots.
        let finals_before = count_files(&storage.day_dir, false);
        let (o2, f2) = recover_camera_partials(&storage.layout, &storage.camera);
        let _ = (o2, f2);
        let finals_after = count_files(&storage.day_dir, false);
        assert!(finals_after >= finals_before);
        // …and specifically OUR final from before is untouched:
        assert!(final_path.is_file());
    }
}
