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
//!    into THIS ATTEMPT'S exclusively-created scratch file, finalize it
//!    durably, publish it no-replace at the DETERMINISTIC recovered
//!    final — and only then remove the original partial;
//! 3. anything that cannot be PROVEN recoverable keeps its partial file in
//!    place untouched: no invented recordings, no blind renames to final.
//!
//! # Idempotency: deterministic FINAL, unique SCRATCH (final correctness
//! remediation §1/§5)
//!
//! The recovered FINAL and the tombstone are DETERMINISTIC, derived from
//! the original's canonical name ([`recovery_identity`]): the same
//! surviving original always resolves to the same `<base>.recovered.mkv`
//! and the same `<base>.recovered.mkv.done` marker no matter how many
//! passes run. The final's mere existence is the authoritative
//! "already recovered" signal — a repeat pass recognizes it BEFORE opening
//! the demuxer and never remuxes again, so the same original can never
//! produce a second recording.
//!
//! The recovery SCRATCH is per-ATTEMPT UNIQUE
//! (`<base>.recovery-<pid>-<serial>-<nanos>.tmp`, created with atomic
//! no-replace `create_new` and never deleted unless THIS attempt owns it).
//! Two concurrent recovery processes therefore write two different
//! scratch pathnames; the deterministic final arbitrates: the winner's
//! no-replace publish commits, the loser observes `DestinationExists`,
//! recognizes the final as already committed, removes ONLY its own
//! scratch and reports `AlreadyRecovered`. A scan-then-delete race over a
//! shared scratch name — where one process could unlink another's active
//! scratch and keep writing an orphaned inode — cannot happen by
//! construction.
//!
//! After publication, the tombstone sidecar (created with atomic
//! no-replace `create_new`) records the transaction for observability. A
//! crash anywhere leaves a state the next pass resolves correctly, and the
//! already-recovered path REPAIRS the transaction (§5): ensure the
//! tombstone, retry removing the original, report the cleanup result —
//! convergence to "final exists, original gone, tombstone in known state"
//! instead of re-scanning the same leftover forever.
//!
//! All identity names (`<base>.recovered.mkv`, `…done`, the unique
//! scratch) never parse as canonical segment names — see
//! `nian_storage::classify_recording_file_name`, the M4-facing contract
//! that classifies recovered recordings as first-class recordings and
//! scratch/tombstones as artifacts. No SQLite, no index: the filesystem
//! alone carries the transaction state (Windows-first semantics: every
//! step is `create_new`/no-replace-rename, both atomic on NTFS).
//!
//! A truncated-but-readable partial never becomes "the final it was named
//! after": its recovered content is a DISTINCT recording slot
//! (`<original-stem>.recovered.mkv`), so an existing final can never be
//! overwritten by recovery, and normal-recording names never collide with
//! recovery identities.
//!
//! # Cooperative graceful stop (final correctness remediation §2)
//!
//! Recovery observes BOTH stop domains: the [`InterruptHandle`] (force
//! cancellation of blocked FFmpeg I/O — the escalation) and the run-level
//! [`StopFlag`] (the FIRST graceful stop). At every safe boundary — before
//! each partial, before the source reopen, before scratch acquisition,
//! between packet operations, before finalize and before publication — a
//! graceful stop request abandons the attempt: the attempt's private
//! scratch is removed, the original stays safely recoverable, nothing
//! publishes unless the transaction ALREADY crossed its durable
//! publication commit point, and no further partial is started. The first
//! `recording.stop` therefore never needs a second press merely because
//! the job happens to be in `recovering`.
//!
//! # Failure containment (final remediation §7 / final correctness §7)
//!
//! Failures are TYPED, never string-classified. [`RecoveryError::
//! Infrastructure`] means the storage TARGET is unsafe (the camera tree
//! cannot be scanned, new files cannot be claimed where recovery must
//! write) — the worker fails the recording job permanently when this
//! occurs, regardless of other files' successes. [`RecoveryError::
//! Artifact`] is a per-attempt storage problem on THIS attempt's own
//! artifact (stat of the finalized scratch, a non-collision publish
//! failure) — it coexists with continued recording. [`RecoveryError::
//! Unreadable`] quarantines one partial's content. [`RecoveryError::
//! Cancelled`] is stop/shutdown, neither breakage nor a verdict. One
//! partial's failure never aborts other files, and the source file always
//! survives any failed attempt.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nian_domain::{CameraId, MediaRational};
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
use nian_storage::paths::publish_no_replace;
use nian_storage::{
    PartialDisposition, PartialFile, RecordingsLayout, StorageError, scan_camera_partials,
};

use crate::StopFlag;

/// Deterministic test/seam hooks for recovery's pipeline. Inert unless
/// armed; compiled out of production builds unless the `test-hooks` cargo
/// feature is enabled (which only test targets do, via dev-dependencies).
#[cfg(any(test, feature = "test-hooks"))]
pub mod test_hooks {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    pub static WRITE_FAIL_AFTER_CALLS: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    pub static FAIL_METADATA: AtomicBool = AtomicBool::new(false);
    pub static HIJACK_CLEANUP_TO_DIR: AtomicBool = AtomicBool::new(false);
    /// Milliseconds to hold the attempt at its PRE-PUBLICATION checkpoint
    /// (before the graceful-stop gate that decides publish vs abandon).
    /// Used to prove cooperative stop during asynchronous recovery.
    pub static PRE_PUBLISH_DELAY_MS: AtomicU64 = AtomicU64::new(0);
    /// When armed, every attempt blocks at its publication step until all
    /// armed attempts arrive — the deterministic concurrency seam for
    /// simultaneous recovery of one original.
    pub static PUBLISH_BARRIER: Mutex<Option<Arc<std::sync::Barrier>>> = Mutex::new(None);
    /// Every scratch path this process claimed, in claim order (uniqueness
    /// assertions for concurrent attempts).
    pub static CLAIMED_SCRATCHES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Serializes every fault-armed test against the process-global hooks.
    pub static FAULT_LOCK: Mutex<()> = Mutex::new(());

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            // Panic-safe: resets every hook whenever this guard dies.
            WRITE_FAIL_AFTER_CALLS.store(usize::MAX, Ordering::SeqCst);
            WRITE_CALLS.store(0, Ordering::SeqCst);
            FAIL_METADATA.store(false, Ordering::SeqCst);
            HIJACK_CLEANUP_TO_DIR.store(false, Ordering::SeqCst);
            PRE_PUBLISH_DELAY_MS.store(0, Ordering::SeqCst);
            *PUBLISH_BARRIER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            CLAIMED_SCRATCHES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
use test_hooks::{
    CLAIMED_SCRATCHES as HOOK_SCRATCHES, FAIL_METADATA as HOOK_FAIL_METADATA,
    HIJACK_CLEANUP_TO_DIR as HOOK_HIJACK_CLEANUP, PRE_PUBLISH_DELAY_MS as HOOK_DELAY_MS,
    PUBLISH_BARRIER as HOOK_BARRIER, WRITE_CALLS as HOOK_WRITE_CALLS,
    WRITE_FAIL_AFTER_CALLS as HOOK_WRITE_FAIL_AFTER,
};

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the deterministic mux-write fault (fail on/after the K-th muxer
/// call) and returns a panic-safe reset guard.
pub fn arm_write_fault(after_calls: usize) -> test_hooks::Guard {
    test_hooks::WRITE_CALLS.store(0, Ordering::SeqCst);
    test_hooks::WRITE_FAIL_AFTER_CALLS.store(after_calls, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the post-finalize stat failure hook.
pub fn arm_metadata_failure() -> test_hooks::Guard {
    test_hooks::FAIL_METADATA.store(true, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the cleanup hijack (the original is swapped into a directory just
/// before its unlink, forcing a deterministic EISDIR regardless of uid).
pub fn arm_cleanup_hijack() -> test_hooks::Guard {
    test_hooks::HIJACK_CLEANUP_TO_DIR.store(true, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the pre-publication delay (milliseconds) used by stop-cooperativity
/// tests to hold recovery at a known checkpoint.
pub fn arm_recovery_delay(delay_ms: u64) -> test_hooks::Guard {
    test_hooks::PRE_PUBLISH_DELAY_MS.store(delay_ms, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms a publication barrier shared by `lanes` concurrent attempts.
pub fn arm_publish_barrier(lanes: usize) -> test_hooks::Guard {
    *test_hooks::PUBLISH_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(std::sync::Arc::new(std::sync::Barrier::new(lanes)));
    test_hooks::Guard
}

/// Canonical `<HH-MM-SS>.partial.mkv` name for a timestamp — the exact
/// shape the scanner accepts (mirrors nian-storage's allocator naming).
#[cfg(any(test, feature = "test-hooks"))]
pub fn __recovery_canonical_name(stamp: chrono::NaiveDateTime) -> String {
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

    /// This original's deterministic recovered final ALREADY exists — an
    /// earlier pass published it (possibly crashing before cleanup).
    /// Recognized WITHOUT opening the demuxer, so the same surviving
    /// original can never be remuxed (or duplicated) again. The path then
    /// REPAIRS the transaction (final correctness remediation §5): ensure
    /// the tombstone, retry removing the original, report the result.
    AlreadyRecovered {
        /// The surviving original partial.
        partial_path: PathBuf,
        /// The recording published by the earlier pass.
        final_path: PathBuf,
        /// Whether the cleanup retry removed the original THIS pass.
        /// `false` is observable: the original stays safely recoverable,
        /// idempotency is unaffected, and a later startup retries.
        original_removed: bool,
    },

    /// Readable content was salvaged through THIS attempt's unique scratch,
    /// finalized durably and published no-replace at the deterministic
    /// final. The tombstone is written only after that publication; the
    /// ORIGINAL partial is removed last.
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
        /// Whether unlinking the original succeeded. `false` surfaces an
        /// observable cleanup failure; the recovery itself stays valid, and
        /// idempotency is guaranteed by the deterministic identity — a
        /// repeat pass recognizes the published final and never remuxes.
        original_removed: bool,
        /// Whether the tombstone sidecar now persists next to the
        /// recording. `false` is an observable failure (the recording
        /// itself remains fully valid; the deterministic identity still
        /// prevents duplicates).
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
/// remediation §7, final correctness remediation §7): callers never parse
/// error strings to decide what a failure means for the recording job.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// Storage INFRASTRUCTURE failure: the storage target itself is unsafe
    /// for continued operation — the camera tree cannot be scanned, or new
    /// files cannot be created where recovery must claim its scratch. New
    /// recording claims through the same machinery, so the worker fails
    /// the recording job permanently when this occurs, regardless of any
    /// other file's success.
    #[error("storage infrastructure failure while trying to {operation}: {source}")]
    Infrastructure {
        /// Which operation failed (stable label, not an OS string).
        operation: &'static str,
        /// The underlying storage error.
        #[source]
        source: StorageError,
    },

    /// A per-ATTEMPT storage problem on this attempt's OWN artifact: the
    /// finalized scratch could not be stat-ed, or the no-replace publish
    /// failed with something other than the idempotent collision. The
    /// scratch was claimed successfully, so the storage root demonstrably
    /// still accepts new files — this coexists with continued recording.
    #[error("recovery artifact failure while trying to {operation}: {source}")]
    Artifact {
        /// Which operation failed (stable label, not an OS string).
        operation: &'static str,
        /// The underlying storage error.
        #[source]
        source: StorageError,
    },

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

    /// Graceful stop or force cancellation interrupted recovery (final
    /// correctness remediation §2). The untouched original stays for the
    /// next startup pass; this is neither infrastructure breakage nor a
    /// content verdict, and nothing of this attempt publishes.
    #[error("recovery was cancelled")]
    Cancelled,
}

impl RecoveryError {
    /// True when this failure means the storage target is unsafe for new
    /// recording — the worker-level signal for a permanent job failure.
    /// Artifact, content and cancellation classify `false`.
    pub fn is_infrastructure(&self) -> bool {
        matches!(self, RecoveryError::Infrastructure { .. })
    }
}

/// The deterministic recovery transaction identity for one original partial
/// (final remediation §5, option C): the FINAL and the tombstone are
/// derived from the original's canonical name, so the same original always
/// maps to the same recovered recording and marker across any number of
/// passes. The scratch is deliberately NOT part of the identity — it is
/// per-attempt unique (final correctness remediation §1).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryIdentity {
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

/// Process-unique serial for per-attempt scratch names: pid distinguishes
/// concurrent PROCESSES, the counter distinguishes concurrent ATTEMPTS in
/// one process, and the clock nanos break any exotic pid/counter reuse.
fn unique_attempt_tag(serial: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{serial}-{nanos}", std::process::id())
}

/// Creates THIS attempt's private scratch with atomic no-replace semantics.
/// The name is unique per attempt (`<base>.recovery-<pid>-<serial>-<nanos>
/// .tmp`), so two concurrent recoveries never share a writable pathname and
/// no scan-then-delete race can unlink another process's active scratch.
/// Claims through the same `create_new` primitive as live segment claims:
/// failing here means the storage root cannot host new files at all — an
/// INFRASTRUCTURE failure.
fn claim_unique_scratch(day_dir: &Path, base: &str) -> Result<PathBuf, RecoveryError> {
    let serial_counter = AtomicU64::new(0);
    let mut last_error: Option<StorageError> = None;
    for _ in 0..8 {
        let serial = serial_counter.fetch_add(1, Ordering::Relaxed);
        let scratch = day_dir.join(format!(
            "{base}.recovery-{}.tmp",
            unique_attempt_tag(serial)
        ));
        #[cfg(any(test, feature = "test-hooks"))]
        HOOK_SCRATCHES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(scratch.clone());
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
        {
            Ok(_claim) => return Ok(scratch),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // Astronomically unlikely (pid+serial+nanos); retry with a
                // fresh tag. NEVER delete the existing file — it may be
                // another process's active scratch.
                last_error = Some(StorageError::Io {
                    path: scratch,
                    source,
                });
            }
            Err(source) => {
                return Err(RecoveryError::Infrastructure {
                    operation: "claim the recovery scratch",
                    source: StorageError::Io {
                        path: scratch,
                        source,
                    },
                });
            }
        }
    }
    Err(RecoveryError::Infrastructure {
        operation: "claim the recovery scratch",
        source: last_error.unwrap_or_else(|| StorageError::Io {
            path: day_dir.to_path_buf(),
            source: std::io::Error::other("scratch claim exhausted retries"),
        }),
    })
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
    recover_camera_partials_with_interrupt(layout, camera, &interrupt, None)
}

/// Like [`recover_camera_partials`], but observes BOTH stop domains (final
/// correctness remediation §2):
///
/// * `interrupt` — force cancellation: blocked demux/mux operations abort
///   immediately;
/// * `graceful_stop` — the run-level FIRST-stop flag: at every safe
///   boundary (before each partial, before the source reopen, before
///   scratch acquisition, between packet operations, before finalize and
///   before publication) the attempt abandons without publishing, so the
///   first `recording.stop` never needs a second press during `recovering`.
///
/// Files never attempted simply stay partials for the next startup pass.
pub fn recover_camera_partials_with_interrupt(
    layout: &RecordingsLayout,
    camera: &CameraId,
    interrupt: &InterruptHandle,
    graceful_stop: Option<&StopFlag>,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();

    let partials = match scan_camera_partials(layout, camera) {
        Ok(partials) => partials,
        Err(source) => {
            failures.push(RecoveryFailure {
                // No specific path known; point at the camera root so the
                // report stays actionable without inventing a filename.
                partial_path: layout.camera_dir(camera),
                error: RecoveryError::Infrastructure {
                    operation: "scan the camera tree",
                    source,
                },
            });
            return (outcomes, failures);
        }
    };

    if partials.is_empty() {
        outcomes.push(RecoveryOutcome::NothingToDo);
        return (outcomes, failures);
    }

    for partial in partials {
        if interrupt.is_cancelled() || graceful_stop.is_some_and(StopFlag::is_requested) {
            // Bounded stop (§2): the run is being stopped; remaining
            // partials keep waiting for the next startup reconciliation.
            break;
        }
        match recover_one(&partial, interrupt, graceful_stop) {
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
    graceful_stop: Option<&StopFlag>,
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
        | PartialDisposition::FinalizedButUnpublished { .. } => {
            salvage_media(partial, interrupt, graceful_stop)
        }
    }
}

/// The demux → alignment → claim → copy → finalize → publish → tombstone →
/// cleanup pipeline shared by both media classes.
///
/// Ordering contract (M3 remediation §9–§12 + final remediation + final
/// correctness remediation):
///
/// 1. RECOGNIZE an already-recovered original FIRST: when the
///    deterministic final exists, never open the demuxer — ensure the
///    tombstone, retry the original's cleanup and report `AlreadyRecovered`
///    with the cleanup result;
/// 2. graceful-stop gate (before any work on this file);
/// 3. open the ORIGINAL and validate streams/time base (before any new
///    output exists);
/// 4. prove a primary-video keyframe exists, then gate again and restart
///    the source for the actual copy;
/// 5. graceful-stop gate, then claim THIS attempt's UNIQUE scratch;
/// 6. write packets keyframe-aligned until EOF/truncation, gating between
///    packet operations;
/// 7. ANY muxer write failure poisons the output (never finalized or
///    published; scratch removed; original untouched);
/// 8. gates before finalize and — after the pre-publication checkpoint —
///    before publication: a transaction that has not crossed its durable
///    commit point never publishes after a stop request;
/// 9. no-replace publication at the DETERMINISTIC final: a
///    `DestinationExists` means an earlier pass already committed —
///    recognize it, remove ONLY this attempt's scratch, repair the
///    transaction, report `AlreadyRecovered`;
/// 10. drop the SOURCE INPUT first, write the tombstone, then remove the
///     original — failed unlink surfaced observably via
///     `original_removed=false`.
fn salvage_media(
    partial: &PartialFile,
    interrupt: &InterruptHandle,
    graceful_stop: Option<&StopFlag>,
) -> Result<RecoveryOutcome, RecoveryError> {
    let from_finalized = matches!(
        partial.disposition,
        PartialDisposition::FinalizedButUnpublished { .. }
    );
    let stop_requested = || graceful_stop.is_some_and(StopFlag::is_requested);

    // ---- 1. Already-recovered recognition BEFORE any media work ------------
    let identity =
        recovery_identity(&partial.partial_path).ok_or_else(|| RecoveryError::Unreadable {
            message: "partial name carries no canonical recovery identity".to_owned(),
        })?;
    if identity.final_path.exists() {
        // Repair the transaction (final correctness remediation §5): ensure
        // the tombstone, retry the original's cleanup, report both — never
        // remux, never duplicate.
        let _tombstone_recorded = ensure_tombstone(&identity, &partial.partial_path);
        let original_removed = std::fs::remove_file(&partial.partial_path).is_ok();
        return Ok(RecoveryOutcome::AlreadyRecovered {
            partial_path: partial.partial_path.clone(),
            final_path: identity.final_path,
            original_removed,
        });
    }

    // ---- 2. Graceful-stop gate before working on this file (§2) ------------
    if stop_requested() {
        return Err(RecoveryError::Cancelled);
    }

    // Cancellation-aware open mapping: a cancel arriving during a blocked
    // open classifies as Cancelled, never as a content verdict (§2/§7).
    let open_result = |error: nian_media::MediaError| {
        if interrupt.is_cancelled() {
            RecoveryError::Cancelled
        } else {
            RecoveryError::Unreadable {
                message: error.to_string(),
            }
        }
    };

    // ---- 3. Open + validate the ORIGINAL -----------------------------------
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

    // ---- 4. Alignment probe BEFORE claiming anything -----------------------
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
    // unread, so RESTART the proven-readable source once. Gate first (§2),
    // then reopen with its own operation-scoped budget (M3 §5 discipline).
    if stop_requested() {
        return Err(RecoveryError::Cancelled);
    }
    drop(input);
    let reopen_budget = interrupt.scoped_deadline(Duration::from_secs(15));
    let mut input = MediaInput::open(&source, interrupt).map_err(open_result)?;
    drop(reopen_budget);

    // ---- 5. Graceful-stop gate, then claim THIS attempt's UNIQUE scratch ---
    if stop_requested() {
        return Err(RecoveryError::Cancelled);
    }
    let base = partial
        .partial_path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(PARTIAL_NAME_SUFFIX))
        .ok_or_else(|| RecoveryError::Unreadable {
            message: "partial name carries no canonical recovery identity".to_owned(),
        })?;
    let day_dir = partial
        .partial_path
        .parent()
        .ok_or_else(|| RecoveryError::Unreadable {
            message: "partial has no parent directory".to_owned(),
        })?
        .to_path_buf();
    let scratch = claim_unique_scratch(&day_dir, base)?;

    let selection = streams.clone();
    let mut muxer = MatroskaMuxer::create_with_selection(&mut input, &scratch, interrupt, |info| {
        selection
            .iter()
            .any(|s| s.stream_index == info.stream_index)
    })
    .map_err(|error| RecoveryError::Unreadable {
        message: format!("salvage output could not be opened: {error}"),
    })?;

    // ---- 6. Copy loop with POISON-on-write-failure + stop gates ------------
    let mut aligned = false;
    let mut start_media: Option<i64> = None;
    let mut last_media: Option<i64> = None;
    let mut video_packets: u64 = 0;
    let mut mux_write_failed = false;
    #[allow(unused_mut)] // mutated only under test hooks
    let mut fault_poisoned = false;

    loop {
        // Between-packet stop gate (§2): a graceful stop abandons the
        // attempt at the next packet boundary. Aborts are resolved AFTER
        // the loop (the muxer must be closed before the scratch removal).
        if stop_requested() || interrupt.is_cancelled() {
            break;
        }
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
                #[cfg(any(test, feature = "test-hooks"))]
                {
                    // Deterministic mux-write fault injection: fail the
                    // K-th muxer call so tests prove poisoned outputs never
                    // publish.
                    if HOOK_WRITE_CALLS.fetch_add(1, Ordering::SeqCst)
                        < HOOK_WRITE_FAIL_AFTER.load(Ordering::SeqCst)
                    {
                        // fall through to a real write
                    } else {
                        fault_poisoned = true;
                        break;
                    }
                }
                if muxer.write_packet(&packet).is_err() {
                    // M2 invariant applies verbatim: ANY mux/output write
                    // failure poisons this output — NEVER finalize/publish
                    // it. The scratch is removed after the loop (it is
                    // recovery-owned, never a camera partial); the original
                    // stays intact.
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

    // Post-loop aborts. Each closes the muxer FIRST (Windows-first: the
    // open handle must not block the scratch removal) and removes ONLY
    // this attempt's scratch — the original is never touched here.
    if fault_poisoned || mux_write_failed {
        drop(muxer);
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Unreadable {
            message: "salvage output write failed; output poisoned, never published".to_owned(),
        });
    }

    if stop_requested() || interrupt.is_cancelled() {
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

    // ---- 8a. Durable finalize + REQUIRED size lookup -----------------------
    //
    // Graceful-stop gate BEFORE finalize: a transaction that has not yet
    // crossed its durable publication commit point never publishes after a
    // stop request.
    if stop_requested() {
        drop(muxer);
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Cancelled);
    }
    muxer
        .finalize()
        .map_err(|error| RecoveryError::Unreadable {
            message: format!("salvaged output failed to finalize: {error}"),
        })?;

    #[cfg(any(test, feature = "test-hooks"))]
    if HOOK_FAIL_METADATA.load(Ordering::SeqCst) {
        // Simulate a post-finalize stat failure: nothing may publish even
        // though the trailer succeeded (finding 11's transaction boundary).
        // Per-attempt artifact failure — other work continues.
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Artifact {
            operation: "stat the finalized recovery scratch",
            source: StorageError::Io {
                path: scratch.clone(),
                source: std::io::Error::other("injected"),
            },
        });
    }

    let size_bytes = std::fs::metadata(&scratch)
        .map_err(|source| RecoveryError::Artifact {
            operation: "stat the finalized recovery scratch",
            source: StorageError::Io {
                path: scratch.clone(),
                source,
            },
        })?
        .len();

    let media_duration = match (start_media, last_media) {
        (Some(start), Some(last)) => time_base.duration_of(last.saturating_sub(start).max(0)),
        _ => None,
    };

    // ---- 8b. Pre-publication checkpoint: test seam, stop gate, commit ======
    #[cfg(any(test, feature = "test-hooks"))]
    {
        // Deterministic delay + concurrency seam: hold THIS attempt at a
        // known point immediately before the durable commit, then release
        // all barriered attempts together.
        let delay = HOOK_DELAY_MS.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        // Clone the Arc OUT of the mutex: waiting on the barrier while
        // holding the registry lock would deadlock the second lane.
        let barrier = HOOK_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(std::sync::Arc::clone);
        if let Some(barrier) = barrier {
            let _ = barrier.wait();
        }
    }

    // The durable-publication gate (§2): past this point the transaction is
    // committed; before it, a graceful stop abandons without publishing.
    if stop_requested() {
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Cancelled);
    }

    if let Err(error) = publish_no_replace(&scratch, &identity.final_path) {
        return match error {
            // An earlier pass (or a concurrent one) already committed this
            // recording. The deterministic identity guarantees it is the
            // SAME original's recovered slot: recognize it, remove ONLY
            // this attempt's scratch (never another attempt's), repair the
            // transaction and report idempotent success — never a
            // duplicate publication.
            StorageError::DestinationExists { .. } => {
                let _ = std::fs::remove_file(&scratch);
                let _tombstone_recorded = ensure_tombstone(&identity, &partial.partial_path);
                let original_removed = std::fs::remove_file(&partial.partial_path).is_ok();
                Ok(RecoveryOutcome::AlreadyRecovered {
                    partial_path: partial.partial_path.clone(),
                    final_path: identity.final_path,
                    original_removed,
                })
            }
            other => Err(RecoveryError::Artifact {
                operation: "publish the recovered recording",
                source: other,
            }),
        };
    }

    // ---- 10. Source closed FIRST, tombstone, then observable cleanup ------
    drop(input);

    // Tombstone strictly AFTER publication: its persistence failure is
    // observable, but the deterministic identity keeps idempotency intact.
    // No stop gate here: the transaction already crossed its durable commit
    // point, so the tiny post-publication bookkeeping always completes.
    let tombstone_recorded = ensure_tombstone(&identity, &partial.partial_path);

    #[cfg(any(test, feature = "test-hooks"))]
    if HOOK_HIJACK_CLEANUP.load(Ordering::SeqCst) {
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

/// Builds the demuxer source for a partial path (local file by definition —
/// partials only ever exist inside the storage tree).
fn partial_source(path: &std::path::Path) -> nian_media::MediaSource {
    nian_media::MediaSource::File(path.to_path_buf())
}

#[cfg(test)]
mod fault_injection_tests {
    //! In-crate deterministic fault injection for the recovery pipeline:
    //! real FFmpeg remux runs where the ONLY synthetic element is the
    //! injected fault — no fake media logic.

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

    /// Counts recovered finals (`<stem>.recovered.mkv`) — the §5 invariant
    /// unit; scratch/tombstone artifacts and partials never count.
    fn count_recordings(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        name.ends_with(".recovered.mkv") && !name.ends_with(".done")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn seed_original(storage: &Storage) -> (String, PathBuf) {
        let name = __recovery_canonical_name(chrono::Local::now().naive_local());
        let path = storage.day_dir.join(&name);
        std::fs::write(
            &path,
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();
        (name, path)
    }

    #[test]
    fn mux_write_fault_poisons_the_recovered_output_and_never_publishes() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_write_fault(5);
        let storage = Storage::new();
        seed_original(&storage);

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
    fn metadata_fault_after_finalize_is_an_artifact_failure_never_published() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_metadata_failure();
        let storage = Storage::new();
        seed_original(&storage);

        let (_outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        // Final correctness remediation §7: a stat failure on THIS
        // attempt's OWN finalized scratch is a per-attempt ARTIFACT
        // failure — the scratch claim had already succeeded, proving the
        // storage root still accepts new files. It must NOT classify as
        // infrastructure (which would fail the whole recording job) and
        // nothing may publish.
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            matches!(
                failures[0].error,
                RecoveryError::Artifact {
                    operation: "stat the finalized recovery scratch",
                    ..
                }
            ),
            "stat failure must be a typed artifact failure: {failures:?}"
        );
        assert!(!failures[0].error.is_infrastructure());
        assert_eq!(count_files(&storage.day_dir, false), 0);
    }

    #[test]
    fn cleanup_failure_is_observable_and_repeat_pass_never_duplicates() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_cleanup_hijack();
        let storage = Storage::new();
        seed_original(&storage);

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

        // The repeat pass on the same surviving original must produce ZERO
        // additional final files — the deterministic identity makes
        // duplicates impossible.
        let finals_before = count_recordings(&storage.day_dir);
        let (o2, _f2) = recover_camera_partials(&storage.layout, &storage.camera);
        let finals_after = count_recordings(&storage.day_dir);
        assert_eq!(
            finals_after, finals_before,
            "a repeat recovery pass must never publish a second copy: {o2:?}"
        );
        assert!(final_path.is_file());
    }

    #[test]
    fn already_recovered_path_repairs_the_transaction() {
        // Final correctness remediation §5: crash window AFTER publication
        // but BEFORE cleanup/tombstone — the deterministic final and the
        // original both exist. The next pass must recognize the recording
        // WITHOUT remuxing, CREATE the missing tombstone, RETRY the
        // original's cleanup, and converge: final exists, original gone,
        // tombstone in known state.
        let storage = Storage::new();
        let (original_name, original_path) = seed_original(&storage);
        let payload = std::fs::read(&original_path).unwrap();

        // Simulate the earlier pass's published final; no tombstone yet.
        let base = original_name.strip_suffix(".partial.mkv").unwrap();
        let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::write(&final_path, &payload).unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert!(
            failures.is_empty(),
            "already-recovered repair is not a failure: {failures:?}"
        );
        let repaired = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                RecoveryOutcome::AlreadyRecovered {
                    original_removed, ..
                } => Some(*original_removed),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repaired.len(), 1);
        assert!(
            repaired[0],
            "the cleanup retry must remove the surviving original"
        );
        assert!(
            !original_path.exists(),
            "convergence: the original is gone after the repair pass"
        );
        assert!(
            storage
                .day_dir
                .join(format!("{base}.recovered.mkv.done"))
                .is_file(),
            "convergence: the tombstone is created during the repair pass"
        );
        assert_eq!(count_recordings(&storage.day_dir), 1);
        assert_eq!(std::fs::read(&final_path).unwrap(), payload);
    }

    #[test]
    fn surviving_original_recognized_without_remux_and_never_duplicated() {
        let storage = Storage::new();
        let (original_name, original_path) = seed_original(&storage);
        let payload = std::fs::read(&original_path).unwrap();
        let base = original_name.strip_suffix(".partial.mkv").unwrap();
        std::fs::write(
            storage.day_dir.join(format!("{base}.recovered.mkv")),
            &payload,
        )
        .unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
            "the pass must report AlreadyRecovered: {outcomes:?}"
        );
        // Exactly ONE recovered final; no scratch artifact ever appeared.
        assert_eq!(count_recordings(&storage.day_dir), 1);
        let names: Vec<String> = std::fs::read_dir(&storage.day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "no scratch artifact may appear: {names:?}"
        );
    }

    #[test]
    fn tombstone_persistence_failure_is_observable_but_not_blocking() {
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let storage = Storage::new();
        let (name, _) = seed_original(&storage);

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
        assert!(
            storage
                .day_dir
                .join(format!("{base}.recovered.mkv"))
                .is_file()
        );
    }

    #[test]
    fn concurrent_recoveries_use_unique_scratches_and_produce_one_final() {
        // Final correctness remediation §1: two process-shaped attempts
        // against the SAME original. The publication barrier releases both
        // at their commit step deterministically; the deterministic final
        // arbitrates. Exactly one final, unique scratch pathnames, the
        // loser reports AlreadyRecovered and removes only its own scratch.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_publish_barrier(2);
        let storage = Storage::new();
        let (_name, original_path) = seed_original(&storage);
        let original_payload = std::fs::read(&original_path).unwrap();

        let layout = storage.layout.clone();
        let camera = storage.camera.clone();
        let handle = {
            let attempt = move || recover_camera_partials(&layout, &camera);
            std::thread::spawn(attempt)
        };
        let res_a = recover_camera_partials(&storage.layout, &storage.camera);
        let res_b = handle.join().unwrap();

        let mut recovered = 0usize;
        let mut already = 0usize;
        let mut finals = Vec::new();
        for (outcomes, failures) in [res_a, res_b] {
            assert!(
                failures.is_empty(),
                "concurrent attempts must not fail: {failures:?}"
            );
            for outcome in outcomes {
                match outcome {
                    RecoveryOutcome::Recovered { final_path, .. } => {
                        recovered += 1;
                        finals.push(final_path);
                    }
                    RecoveryOutcome::AlreadyRecovered {
                        final_path,
                        original_removed,
                        ..
                    } => {
                        already += 1;
                        finals.push(final_path);
                        // The loser's original-cleanup retry races the
                        // winner's — either outcome is honest; the loser
                        // NEVER reports a second recording.
                        let _ = original_removed;
                    }
                    other => panic!("unexpected outcome in concurrent run: {other:?}"),
                }
            }
        }
        assert_eq!(recovered, 1, "exactly one attempt publishes");
        assert_eq!(already, 1, "the loser recognizes the committed final");
        assert_eq!(
            finals[0], finals[1],
            "both agree on the deterministic final"
        );

        // Both attempts wrote DISTINCT scratch pathnames — never a shared
        // writable path that could be unlinked under a live writer.
        let scratches = HOOK_SCRATCHES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(scratches.len(), 2, "{scratches:?}");
        assert_ne!(scratches[0], scratches[1]);
        assert!(
            scratches
                .iter()
                .all(|path| path.to_string_lossy().contains(".recovery-")),
            "scratch names carry the unique-attempt shape: {scratches:?}"
        );

        // Exactly ONE recovered final exists and it independently demuxes;
        // the original was removed by the winner (never overwritten).
        assert_eq!(count_recordings(&storage.day_dir), 1);
        let final_path = &finals[0];
        use nian_media::Probe as _;
        let backend = nian_media_ffmpeg::FfmpegBackend::new().unwrap();
        let report = backend
            .probe(&nian_media::MediaSource::File(final_path.clone()))
            .expect("the recovered final must be independently probeable");
        assert!(
            report
                .streams
                .iter()
                .any(|stream| stream.media_type == nian_domain::MediaType::Video)
        );
        assert!(!original_path.exists());
        let _ = original_payload;
    }

    #[test]
    fn graceful_stop_during_recovery_abandons_without_publishing() {
        // Final correctness remediation §2: recovery observes the run-level
        // graceful-stop domain. The attempt is held at its pre-publication
        // checkpoint; ONE stop request makes it abandon (scratch removed,
        // nothing published, original intact) — no force-cancel needed.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_recovery_delay(700);
        let storage = Storage::new();
        let (_, original_path) = seed_original(&storage);

        let stop = StopFlag::new();
        let stop_for_worker = stop.clone();
        let layout = storage.layout.clone();
        let camera = storage.camera.clone();
        let worker = std::thread::spawn(move || {
            let interrupt = InterruptHandle::new();
            recover_camera_partials_with_interrupt(
                &layout,
                &camera,
                &interrupt,
                Some(&stop_for_worker),
            )
        });

        // One graceful press while the attempt sits at its checkpoint.
        std::thread::sleep(Duration::from_millis(150));
        let started = std::time::Instant::now();
        stop.request();

        let (outcomes, failures) = worker.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the graceful stop must end recovery promptly, took {started:?}"
        );
        assert!(
            outcomes.iter().all(|outcome| matches!(
                outcome,
                RecoveryOutcome::NothingToDo | RecoveryOutcome::KeptUnrecoverable { .. }
            )) || outcomes.is_empty(),
            "nothing may publish after the stop: {outcomes:?}"
        );
        assert!(
            failures
                .iter()
                .all(|failure| matches!(failure.error, RecoveryError::Cancelled)),
            "the stopped attempt reports cancellation, never a verdict: {failures:?}"
        );
        // Original stays safely recoverable; no recovered final; no scratch.
        assert!(original_path.is_file());
        assert_eq!(count_recordings(&storage.day_dir), 0);
        let names: Vec<String> = std::fs::read_dir(&storage.day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "the attempt's private scratch is removed on abandonment: {names:?}"
        );
    }
}
