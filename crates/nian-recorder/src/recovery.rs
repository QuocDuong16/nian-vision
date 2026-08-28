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
//! # True idempotency (final remediation §5)
//!
//! Recovery uses a DETERMINISTIC identity derived from the original's
//! canonical name (see [`recovery_identity`]): the same surviving original
//! always resolves to the same scratch file, the same recovered final and
//! the same tombstone marker, no matter how many startup passes run. The
//! final's mere existence is the authoritative "already recovered" signal
//! — a repeat pass recognizes it BEFORE opening the demuxer and never
//! remuxes again. After publication, a tombstone sidecar
//! (`<base>.recovered.mkv.done`, created with atomic no-replace
//! `create_new`) records the transaction for observability and debugging;
//! a crash anywhere in the sequence leaves a state the next pass resolves
//! correctly:
//!
//! * crash before publication → scratch only; the stale scratch is safe to
//!   delete (the original always outlives it) and the pass simply redoes
//!   the recovery;
//! * crash after publication, before tombstone/unlink → the deterministic
//!   final exists → the next pass reports `AlreadyRecovered` without
//!   touching the demuxer;
//! * crash after tombstone, before unlink → same as above; unlink is then
//!   retried.
//!
//! The identity names (`<base>.recovered-tmp`, `<base>.recovered.mkv`,
//! `<base>.recovered.mkv.done`) never parse as canonical segment names, so
//! the storage scanner and the M4 retention janitor cannot mistake recovery
//! artifacts for crash partials or recordings. No SQLite, no index: the
//! filesystem alone carries the transaction state (Windows-first semantics:
//! every step is `create_new`/no-replace-rename, both atomic on NTFS).
//!
//! A truncated-but-readable partial never becomes "the final it was named
//! after": its recovered content is a DISTINCT recording slot
//! (`<original-stem>.recovered.mkv`), so an existing final can never be
//! overwritten by recovery, and normal-recording names never collide with
//! recovery identities.
//!
//! # Failure containment (final remediation §7)
//!
//! Failures are TYPED, not string-classified: [`RecoveryError::Storage`]
//! means storage infrastructure trouble (scan/claim/publish/stat failed —
//! safe recording is impossible), [`RecoveryError::Unreadable`] means one
//! partial's content could not be proven (quarantine it, keep recording),
//! and [`RecoveryError::Cancelled`] means stop/shutdown interrupted the
//! pass. One partial's failure never aborts other files: caller-visible
//! failures are collected per-file, and the source file always survives any
//! failed attempt.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

    /// Final remediation §5: this original's deterministic recovered final
    /// ALREADY exists — an earlier pass published it (possibly crashing
    /// before cleanup). Recognized WITHOUT opening the demuxer, so the same
    /// surviving original can never be remuxed (or duplicated) again.
    AlreadyRecovered {
        /// The surviving original partial.
        partial_path: PathBuf,
        /// The recording published by the earlier pass.
        final_path: PathBuf,
    },

    /// Readable content was salvaged into the original's deterministic
    /// recovery slot, finalized durably and published no-replace. The
    /// tombstone is written only after that publication; the ORIGINAL
    /// partial is removed last.
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
        /// itself stays valid, and idempotency is guaranteed by the
        /// deterministic identity — a repeat pass recognizes the published
        /// final and never remuxes again.
        original_removed: bool,
        /// Final remediation §5: whether the tombstone sidecar now persists
        /// next to the recording. `false` is an observable failure (the
        /// recording itself remains fully valid; the deterministic identity
        /// still prevents duplicates).
        tombstone_recorded: bool,
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

/// Recovery-specific failures. The variant IS the classification (final
/// remediation §7): callers never parse error strings to decide whether a
/// failure is a per-file content problem or broken storage infrastructure.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// Storage infrastructure failure: scanning the camera tree, claiming
    /// the recovery output, stat-ing the finalized scratch or publishing
    /// failed at the filesystem level. Safe storage operation is
    /// impossible — the CALLER may fail the recording job permanently.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// One partial's CONTENT could not be proven recoverable (failed open,
    /// no video stream, no usable time base, trailer/flush failure on the
    /// salvage output, or a poisoned mux write). Per-file only: quarantine
    /// the file and keep recording — it must never block new camera work.
    #[error("partial is not provably recoverable media: {message}")]
    Unreadable {
        /// Human-safe backend description (secret-free by media-layer
        /// contract).
        message: String,
    },

    /// Stop/shutdown interrupted recovery (final remediation §6/§7). The
    /// untouched partial stays for the next startup pass; this is neither
    /// infrastructure breakage nor a content verdict.
    #[error("recovery was cancelled")]
    Cancelled,
}

impl RecoveryError {
    /// True when this failure means storage infrastructure is unusable —
    /// the worker-level signal for a permanent job failure. Per-file
    /// content failures and cancellation classify `false`.
    pub fn is_infrastructure(&self) -> bool {
        matches!(self, RecoveryError::Storage(_))
    }
}

/// The deterministic recovery transaction for one original partial (final
/// remediation §5, option C): every artifact name is derived from the
/// original's canonical name, so the same original always maps to the same
/// scratch, the same final and the same tombstone across any number of
/// passes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryIdentity {
    /// Scratch file the salvage muxer writes into (`<base>.recovered-tmp`).
    /// Never a canonical segment name, so the scanner cannot mistake a
    /// crashed pass's scratch for a camera partial.
    scratch: PathBuf,
    /// Deterministic recovered recording (`<base>.recovered.mkv`). Its
    /// EXISTENCE is the authoritative already-recovered signal.
    final_path: PathBuf,
    /// Tombstone sidecar (`<base>.recovered.mkv.done`), created atomically
    /// (no-replace) only after publication.
    tombstone: PathBuf,
}

/// Canonical partial names end with exactly this suffix.
const PARTIAL_NAME_SUFFIX: &str = ".partial.mkv";

/// Derives the identity from a partial's canonical name; `None` for names
/// that somehow reached recovery without the scanner's canonical guarantee.
fn recovery_identity(partial_path: &Path) -> Option<RecoveryIdentity> {
    let name = partial_path.file_name()?.to_str()?;
    let base = name.strip_suffix(PARTIAL_NAME_SUFFIX)?;
    let dir = partial_path.parent()?;
    Some(RecoveryIdentity {
        scratch: dir.join(format!("{base}.recovered-tmp")),
        final_path: dir.join(format!("{base}.recovered.mkv")),
        tombstone: dir.join(format!("{base}.recovered.mkv.done")),
    })
}

/// Writes the tombstone sidecar with atomic no-replace semantics; returns
/// whether it NOW persists as a regular file (freshly created or left by an
/// earlier pass). Creation only ever happens after the recovered final is
/// published (§5); a `false` here is the observable persistence failure.
fn ensure_tombstone(identity: &RecoveryIdentity, original: &Path) -> bool {
    if identity.tombstone.is_file() {
        return true;
    }
    let payload = format!(
        "nian-vision recovery tombstone\noriginal: {}\nfinal: {}\n",
        original
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
        identity
            .final_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default(),
    );
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // atomic no-replace on every supported platform
        .open(&identity.tombstone)
        .and_then(|mut file| std::io::Write::write_all(&mut file, payload.as_bytes()))
        .is_ok()
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
    let interrupt = InterruptHandle::new();
    recover_camera_partials_with_interrupt(layout, camera, &interrupt)
}

/// Like [`recover_camera_partials`], but all media I/O observes the given
/// interrupt handle (final remediation §6): cancelling it aborts blocked
/// demux/mux operations and stops before starting any further file, so a
/// stop/shutdown during asynchronous startup recovery is BOUNDED. Files
/// never attempted simply stay partials for the next startup pass.
pub fn recover_camera_partials_with_interrupt(
    layout: &RecordingsLayout,
    camera: &CameraId,
    interrupt: &InterruptHandle,
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
        if interrupt.is_cancelled() {
            // Bounded stop (§6): the run is being stopped; remaining
            // partials keep waiting for the next startup reconciliation.
            break;
        }
        match recover_one(&partial, interrupt) {
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
    partial: &PartialFile,
    interrupt: &InterruptHandle,
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
        // publish → tombstone → remove original LAST.
        PartialDisposition::RecoverableMedia { .. }
        | PartialDisposition::FinalizedButUnpublished { .. } => salvage_media(partial, interrupt),
    }
}

/// The demux → alignment → claim → copy → finalize → publish → tombstone →
/// cleanup pipeline shared by both media classes.
///
/// M3 remediation ordering contract (§9–§12), extended by the final
/// remediation (§5 deterministic identity, §6 cancellation):
///
/// 1. RECOGNIZE an already-recovered original FIRST (§5): when the
///    deterministic final exists, return `AlreadyRecovered` without ever
///    opening the demuxer — repeat passes never remux again;
/// 2. open the ORIGINAL and validate streams/time base (§10: before any
///    new output exists);
/// 3. scan/discard until the first primary-video keyframe is PROVEN to
///    exist (§10), then restart the source for the actual copy;
/// 4. ONLY THEN claim the DETERMINISTIC recovery scratch (§5) and open its
///    muxer — a stale scratch from a crashed pass is deleted first (the
///    original always outlives the scratch, so it can never hold unique
///    content);
/// 5. write packets keyframe-aligned until EOF/truncation;
/// 6. ANY muxer write failure poisons the output (§9): it is never
///    finalized/published — the poisoned scratch is removed, the original
///    stays untouched, a `RecoveryFailure` is reported;
/// 7. finalize durably; the size lookup on the still-partial file must
///    succeed or nothing publishes (§11: no `unwrap_or(0)`);
/// 8. no-replace publication commits the recovered recording at its
///    DETERMINISTIC final — a `DestinationExists` here means an earlier
///    pass already published it: report `AlreadyRecovered`, never a
///    duplicate;
/// 9. drop the SOURCE INPUT first (§12), write the tombstone (§5), then
///    remove the original — with a failed unlink surfaced observably via
///    `original_removed=false`.
fn salvage_media(
    partial: &PartialFile,
    interrupt: &InterruptHandle,
) -> Result<RecoveryOutcome, RecoveryError> {
    let from_finalized = matches!(
        partial.disposition,
        PartialDisposition::FinalizedButUnpublished { .. }
    );

    // ---- 1. Already-recovered recognition BEFORE any media work (§5) -------
    let identity =
        recovery_identity(&partial.partial_path).ok_or_else(|| RecoveryError::Unreadable {
            message: "partial name carries no canonical recovery identity".to_owned(),
        })?;
    if identity.final_path.exists() {
        // Best-effort tombstone: covers the crash window between publication
        // and tombstone persistence. The recording itself is authoritative.
        let _ = ensure_tombstone(&identity, &partial.partial_path);
        return Ok(RecoveryOutcome::AlreadyRecovered {
            partial_path: partial.partial_path.clone(),
            final_path: identity.final_path,
        });
    }

    // Cancellation-aware open mapping: a cancel arriving during a blocked
    // open classifies as Cancelled, never as a content verdict (§6/§7).
    let open_result = |error: nian_media::MediaError| {
        if interrupt.is_cancelled() {
            RecoveryError::Cancelled
        } else {
            RecoveryError::Unreadable {
                message: error.to_string(),
            }
        }
    };

    // ---- 2. Open + validate the ORIGINAL -----------------------------------
    let open_budget = interrupt.scoped_deadline(Duration::from_secs(15));
    let source = partial_source(&partial.partial_path);
    let mut input = MediaInput::open(&source, interrupt).map_err(open_result)?;
    drop(open_budget);

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

    // ---- 3. Alignment probe BEFORE claiming anything (§10) -----------------
    //
    // Prove a selected VIDEO keyframe is reachable WITHOUT creating an
    // output first: a truncated-before-keyframe candidate manufactures ZERO
    // junk recovery artifacts.
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
    // loop performs its own alignment identically to live recording. The
    // restart open gets its own operation-scoped budget (M3 §5 discipline).
    drop(input);
    let reopen_budget = interrupt.scoped_deadline(Duration::from_secs(15));
    let mut input = MediaInput::open(&source, interrupt).map_err(open_result)?;
    drop(reopen_budget);

    // ---- 4. Claim the DETERMINISTIC scratch (§5) ----------------------------
    //
    // A stale scratch can only come from a crashed earlier pass of THIS
    // same original: publication renames it away, and the original is
    // unlinked strictly after that. Deleting it therefore never discards
    // unique content.
    let scratch = claim_recovery_scratch(&identity.scratch)?;

    let selection = streams.clone();
    let mut muxer = MatroskaMuxer::create_with_selection(&mut input, &scratch, interrupt, |info| {
        selection
            .iter()
            .any(|s| s.stream_index == info.stream_index)
    })
    .map_err(|error| RecoveryError::Unreadable {
        message: format!("salvage output could not be opened: {error}"),
    })?;

    // ---- 5. Copy loop with POISON-on-write-failure (§9) ---------------------
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
                        // Poisoned scratch is OUR artifact, not a camera
                        // partial: remove it, keep the original, report.
                        let _ = std::fs::remove_file(&scratch);
                        return Err(RecoveryError::Unreadable {
                            message:
                                "salvage output write failed; output poisoned, never published"
                                    .to_owned(),
                        });
                    }
                }
                if muxer.write_packet(&packet).is_err() {
                    // M2 invariant applies verbatim: ANY mux/output write
                    // failure poisons this output — NEVER finalize/publish
                    // it. The scratch is removed (it is recovery-owned,
                    // never a camera partial); the original stays intact.
                    let _ = std::fs::remove_file(&scratch);
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
            Err(_) => {
                if interrupt.is_cancelled() {
                    // Stop/shutdown interrupted the salvage (§6): nothing
                    // publishes, the original stays for the next pass.
                    let _ = std::fs::remove_file(&scratch);
                    return Err(RecoveryError::Cancelled);
                }
                break; // truncation mid-partial: expected shape here
            }
        }
    }

    if mux_write_failed {
        drop(muxer); // NOT finalized: nothing published
        return Err(RecoveryError::Unreadable {
            message: "salvage output write failed; output poisoned, never published".to_owned(),
        });
    }

    if interrupt.is_cancelled() {
        drop(muxer);
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Cancelled);
    }

    // Zero committed video packets is not a recording (tiny-output guard).
    // Remove the recovery-owned scratch, keep the original, publish nothing.
    if video_packets == 0 {
        drop(muxer);
        let _ = std::fs::remove_file(&scratch);
        return Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "demuxed but no video packets survived alignment".to_owned(),
        });
    }

    // ---- 7. Durable finalize + REQUIRED size lookup (§11) ------------------
    muxer
        .finalize()
        .map_err(|error| RecoveryError::Unreadable {
            message: format!("salvaged output failed to finalize: {error}"),
        })?;

    #[cfg(test)]
    if HOOK_FAIL_METADATA.load(std::sync::atomic::Ordering::SeqCst) {
        // Simulate a post-finalize stat failure: nothing may publish even
        // though the trailer succeeded (finding 11's transaction boundary).
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Storage(StorageError::Io {
            path: scratch.clone(),
            source: std::io::Error::other("injected"),
        }));
    }

    let size_bytes = std::fs::metadata(&scratch)
        .map_err(|source| {
            RecoveryError::Storage(StorageError::Io {
                path: scratch.clone(),
                source,
            })
        })?
        .len();

    let media_duration = match (start_media, last_media) {
        (Some(start), Some(last)) => time_base.duration_of(last.saturating_sub(start).max(0)),
        _ => None,
    };

    // ---- 8. Atomic no-replace publication at the DETERMINISTIC final =======
    if let Err(error) = publish_no_replace(&scratch, &identity.final_path) {
        return match error {
            // An earlier pass (or the crash window between publication and
            // tombstone) already committed this recording. The identity
            // guarantees it is the SAME original's recovered slot: report
            // idempotent success, never a duplicate publication.
            StorageError::DestinationExists { .. } => {
                let _ = std::fs::remove_file(&scratch);
                let _ = ensure_tombstone(&identity, &partial.partial_path);
                Ok(RecoveryOutcome::AlreadyRecovered {
                    partial_path: partial.partial_path.clone(),
                    final_path: identity.final_path,
                })
            }
            other => Err(RecoveryError::Storage(other)),
        };
    }

    // ---- 9. Source closed FIRST, tombstone, then observable cleanup (§12) --
    drop(input);

    // Tombstone strictly AFTER publication (§5): its persistence failure is
    // observable, but the deterministic identity keeps idempotency intact.
    let tombstone_recorded = ensure_tombstone(&identity, &partial.partial_path);

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
        final_path: identity.final_path,
        media_duration,
        size_bytes,
        from_finalized_leftover: from_finalized,
        original_removed,
        tombstone_recorded,
    })
}

/// Exclusively claims the deterministic recovery scratch. A stale scratch
/// (crashed earlier pass of the same original) is deleted first — see
/// `salvage_media` step 4 for why that is always safe.
fn claim_recovery_scratch(scratch: &Path) -> Result<PathBuf, RecoveryError> {
    let claim = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(scratch)
    };
    match claim() {
        Ok(_) => Ok(scratch.to_path_buf()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(scratch).map_err(|remove_source| {
                RecoveryError::Storage(StorageError::Io {
                    path: scratch.to_path_buf(),
                    source: remove_source,
                })
            })?;
            claim().map_err(|source| {
                RecoveryError::Storage(StorageError::Io {
                    path: scratch.to_path_buf(),
                    source,
                })
            })?;
            Ok(scratch.to_path_buf())
        }
        Err(source) => Err(RecoveryError::Storage(StorageError::Io {
            path: scratch.to_path_buf(),
            source,
        })),
    }
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

        // NOTHING published: no non-partial, non-recovery file anywhere.
        assert_eq!(
            count_files(&storage.day_dir, false),
            0,
            "poisoned recovery output must never be published"
        );
        // The poisoned recovery scratch is REMOVED (it is recovery-owned,
        // never a camera partial); the ORIGINAL stays untouched as the
        // canonical leftover for the next pass.
        let leftovers = count_files(&storage.day_dir, true);
        assert_eq!(
            leftovers, 1,
            "exactly the original partial must remain: {outcomes:?} / {failures:?}"
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
        // published; failure surfaced as typed Storage (infrastructure);
        // original intact.
        assert!(
            failures
                .iter()
                .any(|failure| failure.error.is_infrastructure()),
            "metadata failure must surface as typed storage failure: {failures:?}"
        );
        assert_eq!(count_files(&storage.day_dir, false), 0);
    }

    #[test]
    fn cleanup_failure_is_observable_and_repeat_pass_never_duplicates() {
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
                    tombstone_recorded,
                    ..
                } => Some((*original_removed, *tombstone_recorded, final_path.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recovered.len(), 1);
        let (original_removed, tombstone_recorded, final_path) = &recovered[0];
        assert!(!original_removed, "hijacked unlink must be observable");
        assert!(
            *tombstone_recorded,
            "the tombstone persists after successful publication"
        );
        assert!(
            final_path.is_file(),
            "the recovered recording itself stays fully valid"
        );

        // Final remediation §5: the repeat pass on the same surviving
        // original must produce ZERO additional final files — the
        // deterministic identity makes duplicates impossible.
        let finals_before = count_recordings(&storage.day_dir);
        let (o2, _f2) = recover_camera_partials(&storage.layout, &storage.camera);
        let finals_after = count_recordings(&storage.day_dir);
        assert_eq!(
            finals_after, finals_before,
            "a repeat recovery pass must never publish a second copy: {o2:?}"
        );
        // …and OUR final from before is untouched:
        assert!(final_path.is_file());
    }

    /// Counts recordings the way the §5 invariant demands: recovered
    /// finals are `<stem>.recovered.mkv` files; scratch/tombstone artifacts
    /// and partials never count.
    fn count_recordings(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        name.ends_with(".recovered.mkv")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn surviving_original_recognized_as_already_recovered_without_remux() {
        // Final remediation §5: simulate the crash window AFTER publication
        // but BEFORE cleanup — the deterministic final and the original
        // both exist. The next pass must recognize the recording WITHOUT
        // opening the demuxer: no remux, no new files, original untouched.
        let storage = Storage::new();
        let original_name = __recovery_canonical_name(chrono::Local::now().naive_local());
        let original_path = storage.day_dir.join(&original_name);
        let payload = std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap();
        std::fs::write(&original_path, &payload).unwrap();

        // Derive the deterministic final the same way recovery does.
        let base = original_name.strip_suffix(".partial.mkv").unwrap();
        let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::write(&final_path, &payload).unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert!(
            failures.is_empty(),
            "already-recovered recognition is not a failure: {failures:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
            "the pass must report AlreadyRecovered: {outcomes:?}"
        );
        // The original was NOT remuxed: its bytes are byte-identical, the
        // deterministic final is still the ONLY recovered recording, and no
        // scratch artifact appeared (the best-effort tombstone may exist).
        assert_eq!(std::fs::read(&original_path).unwrap(), payload);
        let names: Vec<String> = std::fs::read_dir(&storage.day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.ends_with(".recovered-tmp")),
            "no scratch artifact may appear: {names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| name.ends_with(".recovered.mkv"))
                .count(),
            1,
            "exactly one recovered final must exist: {names:?}"
        );
    }

    #[test]
    fn tombstone_persistence_failure_is_observable_but_not_blocking() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let storage = Storage::new();
        let name = __recovery_canonical_name(chrono::Local::now().naive_local());
        let original_path = storage.day_dir.join(&name);
        std::fs::write(
            &original_path,
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();

        // Pre-create the tombstone as a DIRECTORY: create_new fails
        // deterministically regardless of uid, exactly like a real
        // persistence failure would.
        let base = name.strip_suffix(".partial.mkv").unwrap();
        std::fs::create_dir_all(storage.day_dir.join(format!("{base}.recovered.mkv.done")))
            .unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
        assert!(
            failures.is_empty(),
            "tombstone failure must not fail the recovery: {failures:?}"
        );
        let recovered = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                RecoveryOutcome::Recovered {
                    tombstone_recorded, ..
                } => Some(*tombstone_recorded),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recovered.len(), 1);
        assert!(
            !recovered[0],
            "the failed tombstone persistence must be observable"
        );
        // The recording is still fully published and valid.
        assert!(
            storage
                .day_dir
                .join(format!("{base}.recovered.mkv"))
                .is_file()
        );
    }
}
