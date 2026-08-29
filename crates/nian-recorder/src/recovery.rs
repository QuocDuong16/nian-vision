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
//! # Idempotency: deterministic FINAL, unique SCRATCH, TRUSTED tombstone
//! (final correctness remediation §1/§5, final safety remediation §1)
//!
//! The recovered FINAL and the tombstone are DETERMINISTIC, derived from
//! the original's canonical name ([`recovery_identity`]): the same
//! surviving original always resolves to the same `<base>.recovered.mkv`
//! and the same `<base>.recovered.mkv.done` marker no matter how many
//! passes run. The final's MERE EXISTENCE IS NEVER PROOF (final safety
//! remediation §1): a directory, zero-byte file, corrupt file, foreign
//! file, or unrelated valid media file may sit at the deterministic
//! pathname. Instead, an explicit transaction/conflict contract decides:
//!
//! * A. final absent → a normal recovery attempt;
//! * B. final present AND a TRUSTED tombstone (v2: magic version line, the
//!   exact original/final names AND the published size, strictly parsed)
//!   proves THIS original→final transaction AND the object CURRENTLY at
//!   the final pathname is still that published regular file (same byte
//!   size) → `AlreadyRecovered` with a cleanup retry — no remux, no
//!   duplicate (identity safety remediation §1: a valid old tombstone
//!   NEVER authorizes deleting an original when the pathname has since
//!   become a directory, a zero-byte/truncated file, or a replacement);
//! * C. final present WITHOUT trusted evidence → `RecoveryConflict`:
//!   original and destination both preserved, no remux, no retroactive
//!   success tombstone, no deletion of either file. No recording data is
//!   ever deleted on name-based inference; crash-after-publication-before-
//!   tombstone states are recoverable conflicts, and future repair tooling
//!   may inspect them — M3 stays lossless first.
//!
//! Tombstone FORMAT VERSIONS: only `NIAN-RECOVERY-TOMBSTONE v2` is
//! trusted. This project is pre-v1, and v1 markers bound only NAMES —
//! insufficient to prove the object at the final pathname — so a v1
//! marker is rejected as untrusted (case C conflict, both files
//! preserved); it is never silently treated as equivalent to v2.
//!
//! Tombstone DURABILITY (identity safety remediation §6): the marker's
//! DATA is flushed with `sync_all`. On POSIX a newly created directory
//! ENTRY additionally requires syncing the parent directory, which
//! [`record_tombstone`] performs best-effort where the platform supports
//! it; on Windows/NTFS namespace operations are journaled and no public
//! directory-fsync exists, so the guarantee there is deliberately weaker.
//! The conflict contract remains safe under EVERY durability level: if a
//! tombstone disappears after sudden power loss, the surviving final has
//! no trusted evidence → case C conflict → the original partial is
//! preserved, never deleted on inference.
//!
//! The recovery SCRATCH is per-ATTEMPT UNIQUE
//! (`<base>.recovery-<pid>-<serial>-<nanos>.tmp`, created with atomic
//! no-replace `create_new` and never deleted unless THIS attempt owns it).
//! Two concurrent recovery processes therefore write two different
//! scratch pathnames; the deterministic final arbitrates: the winner's
//! no-replace publish commits, the winner THEN writes the trusted
//! tombstone and removes the original, and the loser observes
//! `DestinationExists` and resolves it through the SAME contract —
//! potentially still inside the window where the winner has not yet
//! written its tombstone. There the loser reports the pending conflict and
//! leaves the original untouched; the winner's own tombstone+cleanup (or a
//! later startup pass) converges the tree. A scan-then-delete race over a
//! shared scratch name — where one process could unlink another's active
//! scratch and keep writing an orphaned inode — cannot happen by
//! construction.
//!
//! After publication, the tombstone sidecar (created with atomic
//! no-replace `create_new` and a durable sync) records the transaction as
//! TRUSTED evidence. A crash anywhere leaves a state the next pass
//! resolves correctly, and the already-recovered path REPAIRS the
//! transaction (§5): retry removing the original, report the cleanup
//! result — convergence to "final exists, original gone, tombstone
//! trusted" instead of re-scanning the same leftover forever.
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
//! # Cooperative graceful stop (final correctness remediation §2, final
//! safety remediation §2)
//!
//! Recovery observes BOTH stop domains: the [`InterruptHandle`] (force
//! cancellation of blocked FFmpeg I/O — the escalation) and the run-level
//! [`StopFlag`] (the FIRST graceful stop). At every safe boundary — before
//! each partial, before the source reopen, before scratch acquisition,
//! before EVERY packet of the keyframe-alignment probe (an explicit loop,
//! never a collapsed `while let`), between packet operations, before
//! finalize and before publication — a graceful stop request abandons the
//! attempt: the attempt's private scratch is removed, the original stays
//! safely recoverable, nothing publishes unless the transaction ALREADY
//! crossed its durable publication commit point, and no further partial is
//! started. The first `recording.stop` therefore never needs a second
//! press merely because the job happens to be in `recovering`.
//!
//! # Failure containment (final remediation §7 / final correctness §7 /
//! final safety §5)
//!
//! Failures are TYPED, never string-classified. [`RecoveryError::
//! Infrastructure`] means the storage TARGET is unsafe (the camera tree
//! cannot be scanned, new files cannot be claimed where recovery must
//! write) — the worker fails the recording job permanently when this
//! occurs, regardless of other files' successes. [`RecoveryError::
//! Artifact`] is a per-attempt problem on THIS attempt's own artifact —
//! including every OUTPUT-side failure (open, write, finalize/trailer/
//! flush of the scratch), which is never evidence about the original's
//! content — so it coexists with continued recording. [`RecoveryError::
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
    CameraLease, PartialDisposition, PartialFile, RecordingsLayout, StorageError,
    scan_camera_partials,
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
    /// When armed, the FIRST attempt to reach its post-publication
    /// bookkeeping parks there (after `publish`, before the tombstone) until
    /// released — the deterministic seam for the concurrent-loser window of
    /// final safety remediation §1: a second attempt observes
    /// `DestinationExists` while NO trusted tombstone exists yet.
    pub static PUBLISH_HOLD: Mutex<Option<Arc<PublishHold>>> = Mutex::new(None);
    /// When armed, an attempt parks INSIDE the keyframe-alignment probe
    /// (before its first packet read) until released — the deterministic
    /// seam proving graceful stop is observed during ALIGNMENT (final
    /// safety remediation §2), not merely at the publication checkpoint.
    pub static ALIGNMENT_GATE: Mutex<Option<Arc<AlignmentGate>>> = Mutex::new(None);
    /// When armed, the attempt's muxer target is pointed at an
    /// un-creatable location AFTER the scratch claim succeeded — the
    /// deterministic output-OPEN failure seam (final safety remediation §5).
    pub static BREAK_OUTPUT_OPEN: AtomicBool = AtomicBool::new(false);
    /// When armed, the muxer finalize step fails deterministically (final
    /// safety remediation §5).
    pub static FAIL_FINALIZE: AtomicBool = AtomicBool::new(false);
    /// When armed, the keyframe-alignment probe's next packet read FAILS
    /// after sleeping this many milliseconds — the deterministic read-error
    /// seam (identity safety remediation §4): tests can race a stop domain
    /// against the error surface inside the hold window. `u64::MAX` =
    /// disarmed (0 = fail immediately).
    pub static ALIGNMENT_READ_FAIL_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    /// Every scratch path this process claimed, in claim order (uniqueness
    /// assertions for concurrent attempts).
    pub static CLAIMED_SCRATCHES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Serializes every fault-armed test against the process-global hooks.
    pub static FAULT_LOCK: Mutex<()> = Mutex::new(());

    /// One-shot arrival/release gate parked at INSIDE the alignment probe.
    pub struct AlignmentGate {
        arrived: (Mutex<bool>, std::sync::Condvar),
        released: (Mutex<bool>, std::sync::Condvar),
    }

    impl AlignmentGate {
        pub(super) fn new() -> Self {
            Self {
                arrived: (Mutex::new(false), std::sync::Condvar::new()),
                released: (Mutex::new(false), std::sync::Condvar::new()),
            }
        }

        /// Blocks until the recovery attempt has parked at the alignment
        /// probe (test-side wait).
        pub fn wait_arrived(&self) {
            let mut arrived = self
                .arrived
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*arrived {
                arrived = self
                    .arrived
                    .1
                    .wait(arrived)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        /// Parks the recovery attempt (pipeline-side): signals arrival, then
        /// blocks until the test releases.
        pub(super) fn park(&self) {
            {
                let mut arrived = self
                    .arrived
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *arrived = true;
            }
            self.arrived.1.notify_all();
            let mut released = self
                .released
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .released
                    .1
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        /// Releases the parked attempt (test-side).
        pub fn release(&self) {
            {
                let mut released = self
                    .released
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *released = true;
            }
            self.released.1.notify_all();
        }
    }

    /// One-shot arrival/release gate parked AFTER publication but BEFORE the
    /// tombstone write — exactly the concurrent-loser window.
    pub struct PublishHold {
        arrived: (Mutex<bool>, std::sync::Condvar),
        released: (Mutex<bool>, std::sync::Condvar),
    }

    impl PublishHold {
        pub(super) fn new() -> Self {
            Self {
                arrived: (Mutex::new(false), std::sync::Condvar::new()),
                released: (Mutex::new(false), std::sync::Condvar::new()),
            }
        }

        /// Blocks until the winner has PUBLISHED and parked (test-side).
        pub fn wait_arrived(&self) {
            let mut arrived = self
                .arrived
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*arrived {
                arrived = self
                    .arrived
                    .1
                    .wait(arrived)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        /// Parks the winner (pipeline-side): signals arrival, then blocks
        /// until the test releases.
        pub(super) fn park(&self) {
            {
                let mut arrived = self
                    .arrived
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *arrived = true;
            }
            self.arrived.1.notify_all();
            let mut released = self
                .released
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .released
                    .1
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        /// Releases the winner so its tombstone/cleanup complete (test-side).
        pub fn release(&self) {
            {
                let mut released = self
                    .released
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *released = true;
            }
            self.released.1.notify_all();
        }
    }

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            // Panic-safe: resets every hook whenever this guard dies.
            WRITE_FAIL_AFTER_CALLS.store(usize::MAX, Ordering::SeqCst);
            WRITE_CALLS.store(0, Ordering::SeqCst);
            FAIL_METADATA.store(false, Ordering::SeqCst);
            HIJACK_CLEANUP_TO_DIR.store(false, Ordering::SeqCst);
            PRE_PUBLISH_DELAY_MS.store(0, Ordering::SeqCst);
            FAIL_FINALIZE.store(false, Ordering::SeqCst);
            BREAK_OUTPUT_OPEN.store(false, Ordering::SeqCst);
            ALIGNMENT_READ_FAIL_MS.store(u64::MAX, Ordering::SeqCst);
            *PUBLISH_BARRIER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *PUBLISH_HOLD
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *ALIGNMENT_GATE
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
    ALIGNMENT_GATE as HOOK_ALIGNMENT_GATE, ALIGNMENT_READ_FAIL_MS as HOOK_ALIGN_READ_FAIL_MS,
    BREAK_OUTPUT_OPEN as HOOK_BREAK_OUTPUT_OPEN, CLAIMED_SCRATCHES as HOOK_SCRATCHES,
    FAIL_FINALIZE as HOOK_FAIL_FINALIZE, FAIL_METADATA as HOOK_FAIL_METADATA,
    HIJACK_CLEANUP_TO_DIR as HOOK_HIJACK_CLEANUP, PRE_PUBLISH_DELAY_MS as HOOK_DELAY_MS,
    PUBLISH_BARRIER as HOOK_BARRIER, PUBLISH_HOLD as HOOK_PUBLISH_HOLD,
    WRITE_CALLS as HOOK_WRITE_CALLS, WRITE_FAIL_AFTER_CALLS as HOOK_WRITE_FAIL_AFTER,
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

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the post-publication hold: the FIRST attempt to publish parks after
/// `publish` but BEFORE the tombstone write, deterministically opening the
/// concurrent-loser window of final safety remediation §1. Returns the guard
/// plus the hold handle (`wait_arrived` / `release`).
pub fn arm_publish_hold() -> (test_hooks::Guard, std::sync::Arc<test_hooks::PublishHold>) {
    let hold = std::sync::Arc::new(test_hooks::PublishHold::new());
    *test_hooks::PUBLISH_HOLD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::sync::Arc::clone(&hold));
    (test_hooks::Guard, hold)
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the alignment gate: the attempt parks INSIDE the keyframe-alignment
/// probe before its first packet read. Returns the guard plus the gate
/// handle (`wait_arrived` / `release`).
pub fn arm_alignment_gate() -> (test_hooks::Guard, std::sync::Arc<test_hooks::AlignmentGate>) {
    let gate = std::sync::Arc::new(test_hooks::AlignmentGate::new());
    *test_hooks::ALIGNMENT_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::sync::Arc::clone(&gate));
    (test_hooks::Guard, gate)
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the deterministic finalize fault (final safety remediation §5).
pub fn arm_finalize_failure() -> test_hooks::Guard {
    test_hooks::FAIL_FINALIZE.store(true, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the deterministic output-OPEN fault: the muxer target is pointed at
/// an un-creatable location AFTER a successful scratch claim.
pub fn arm_output_open_fault() -> test_hooks::Guard {
    test_hooks::BREAK_OUTPUT_OPEN.store(true, Ordering::SeqCst);
    test_hooks::Guard
}

#[cfg(any(test, feature = "test-hooks"))]
/// Arms the deterministic alignment read-error seam: the probe's next
/// packet read fails after holding `hold_ms` milliseconds (identity safety
/// remediation §4). A stop request landing inside the hold window must
/// classify as `Cancelled`; with no stop domain active the error is an
/// honest content verdict.
pub fn arm_alignment_read_failure(hold_ms: u64) -> test_hooks::Guard {
    test_hooks::ALIGNMENT_READ_FAIL_MS.store(hold_ms, Ordering::SeqCst);
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

    /// This original's deterministic recovered final ALREADY exists AND a
    /// TRUSTED tombstone proves this exact original→final transaction AND
    /// the object currently at the final pathname is still the published
    /// regular file (v2 size binding, identity safety remediation §1,
    /// case B). Recognized WITHOUT opening the demuxer, so the same
    /// surviving original can never be remuxed (or duplicated) again. The
    /// path then REPAIRS the transaction (final correctness remediation
    /// §5): retry removing the original, report the cleanup result.
    AlreadyRecovered {
        /// The surviving original partial.
        partial_path: PathBuf,
        /// The recording published by the earlier pass.
        final_path: PathBuf,
        /// Whether the cleanup retry removed the original THIS pass
        /// (`true` also when it was already gone — the transaction has
        /// converged). `false` is observable: the original stays safely
        /// recoverable, idempotency is unaffected, and a later startup
        /// retries.
        original_removed: bool,
    },

    /// The deterministic recovered destination ALREADY exists but carries
    /// NO trusted transaction evidence for THIS original→final pair (final
    /// safety remediation §1, case C): a directory, a zero-byte/foreign
    /// file, a valid-but-unrelated media file, a crash between publication
    /// and tombstone, or a concurrent winner that has not yet written its
    /// tombstone. Both the original partial AND the destination are
    /// preserved UNTOUCHED: no remux, no retroactive success tombstone, no
    /// deletion of either file — pathname existence alone is never proof,
    /// and no recording data may be deleted on name-based inference.
    /// Future repair tooling may inspect such conflicts; M3 stays
    /// lossless-first. A concurrent winner's own tombstone+cleanup remains
    /// authoritative and converges the tree afterwards.
    RecoveryConflict {
        /// The preserved original partial.
        partial_path: PathBuf,
        /// The preserved existing destination.
        final_path: PathBuf,
        /// Human-safe conflict description (no secrets; operator-known
        /// paths only).
        reason: String,
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
    /// Camera ownership conflict or lease-contract failure. This says nothing
    /// about storage health: another process may legitimately own the camera,
    /// or a caller supplied a lease for the wrong camera/layout. In either
    /// case recovery must stop before scanning and must never pretend the disk
    /// is broken.
    #[error("camera ownership failure while trying to {operation}: {source}")]
    Ownership {
        /// Which ownership operation failed (stable label, not an OS string).
        operation: &'static str,
        /// Typed lease error; callers never parse its display text.
        #[source]
        source: StorageError,
    },

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
    /// finalized scratch could not be stat-ed, the no-replace publish
    /// failed with something other than the idempotent collision, or an
    /// OUTPUT-side failure opening/writing/finalizing this attempt's own
    /// scratch (final safety remediation §5 — a mux/output write, trailer
    /// or flush failure is NOT evidence that the original partial's
    /// content is unreadable, so it is never classified as a content
    /// verdict). The scratch was claimed successfully, so the storage root
    /// demonstrably still accepts new files — this coexists with continued
    /// recording.
    #[error("recovery artifact failure while trying to {operation}: {source}")]
    Artifact {
        /// Which operation failed (stable label, not an OS string).
        operation: &'static str,
        /// The underlying storage error.
        #[source]
        source: StorageError,
    },

    /// One partial's CONTENT could not be proven recoverable (failed open,
    /// no video stream, no usable time base, or a poisoned/unreadable
    /// source). Per-file only: quarantine the file and keep recording — it
    /// must never block new camera work.
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
    /// Ownership, artifact, content and cancellation classify `false`.
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
    /// existence alone is NOT proof of anything (final safety remediation
    /// §1) — it must be resolved through
    /// [`tombstone_proves_transaction`] / [`resolve_existing_final`].
    final_path: PathBuf,
    /// Tombstone sidecar (`<base>.recovered.mkv.done`), created atomically
    /// (no-replace, durable) only after a publication by THIS pass. It is
    /// the ONLY trusted already-recovered evidence.
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

/// Shared storage-layer tombstone contract; recorder and retention use one parser.
type TombstoneTransaction = nian_storage::RecoveryTombstone;

fn tombstone_payload(original_name: &str, final_name: &str, size_bytes: u64) -> String {
    nian_storage::recovery_tombstone_payload(original_name, final_name, size_bytes)
}

fn parse_tombstone(bytes: &[u8]) -> Option<TombstoneTransaction> {
    nian_storage::parse_recovery_tombstone(bytes)
}

fn published_final_matches(final_path: &Path, expected_size: u64) -> bool {
    nian_storage::published_final_matches(final_path, expected_size)
}

/// Whether the tombstone next to the deterministic final TRUSTEDLY proves
/// THIS original→final transaction over the object NOW at that path
/// (final safety remediation §1 case B + identity safety remediation §1):
/// readable, structurally valid v2, naming exactly this pair, AND the
/// current object still matching the recorded published size. A tombstone
/// for a different original/final — or one whose destination was since
/// replaced — is untrusted here.
fn tombstone_proves_transaction(identity: &RecoveryIdentity, original_name: &str) -> bool {
    let Some(final_name) = identity.final_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(transaction) = std::fs::read(&identity.tombstone)
        .ok()
        .and_then(|bytes| parse_tombstone(&bytes))
    else {
        return false;
    };
    transaction.original == original_name
        && transaction.final_name == final_name
        && published_final_matches(&identity.final_path, transaction.size_bytes)
}

/// Whether a tombstone NAMES this transaction (structurally valid + name
/// bound) WITHOUT judging the object at the final path. Used only to pick
/// the precise conflict reason when the full proof above fails.
fn tombstone_names_transaction(identity: &RecoveryIdentity, original_name: &str) -> bool {
    let Some(final_name) = identity.final_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    std::fs::read(&identity.tombstone)
        .ok()
        .and_then(|bytes| parse_tombstone(&bytes))
        .is_some_and(|transaction| {
            transaction.original == original_name && transaction.final_name == final_name
        })
}

/// Writes the tombstone sidecar with atomic no-replace semantics and a
/// durable sync; returns whether it NOW persists as a TRUSTED transaction
/// record for this original→final pair. Creation only ever happens after
/// THIS pass published the recovered final (§1: never retroactively, to
/// legitimize a pre-existing destination); a `false` here is the observable
/// persistence failure.
///
/// Durability (identity safety remediation §6): the marker's bytes are
/// flushed with `sync_all`; on POSIX the freshly created directory entry
/// additionally needs the parent directory synced, which happens
/// best-effort below. Even if the marker never becomes durable, the
/// contract stays safe: a final without trusted tombstone is a preserved
/// conflict, never a deletion.
fn record_tombstone(identity: &RecoveryIdentity, original_name: &str, size_bytes: u64) -> bool {
    if tombstone_proves_transaction(identity, original_name) {
        return true;
    }
    let final_name = identity
        .final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let payload = tombstone_payload(original_name, final_name, size_bytes);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // atomic no-replace on every supported platform
        .open(&identity.tombstone)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, payload.as_bytes())?;
            file.sync_all()
        });
    if written.is_ok() {
        sync_parent_directory(&identity.tombstone);
    }
    written.is_ok() && tombstone_proves_transaction(identity, original_name)
}

/// Best-effort POSIX durability for a freshly created directory entry
/// (identity safety remediation §6): `fsync` on the parent directory.
/// No-op where the platform exposes no public directory sync (Windows/NTFS
/// journals namespace operations; the conflict contract covers the gap).
#[cfg(unix)]
fn sync_parent_directory(marker: &Path) {
    if let Some(parent) = marker.parent()
        && let Ok(dir_handle) = std::fs::File::open(parent)
    {
        let _ = dir_handle.sync_all();
    }
}

/// Windows/NTFS: no public directory-fsync; namespace changes are covered
/// by the NTFS journal. The weaker guarantee is documented, and the
/// conflict contract stays safe if a tombstone vanishes after power loss.
#[cfg(not(unix))]
fn sync_parent_directory(_marker: &Path) {}

/// Resolves an EXISTING deterministic destination against the transaction
/// contract (final safety remediation §1; object binding: identity safety
/// remediation §1). Both the early recognition path (before any media
/// work) and the concurrent `DestinationExists` publish path come through
/// here, so they can never disagree:
///
/// * trusted tombstone for this original→final pair AND the current
///   object at the final path still matching the recorded publication →
///   `AlreadyRecovered` with a cleanup retry (case B);
/// * anything else → `RecoveryConflict`, preserving BOTH files (case C):
///   no remux, no retroactive success tombstone, no deletion of either
///   file. A concurrent winner that has published but not yet written its
///   tombstone therefore surfaces as a pending conflict — the loser must
///   NOT delete the original during that window; the winner's own
///   tombstone+cleanup converges the tree afterwards. A tombstone that
///   still NAMES this transaction but whose destination has since become a
///   directory or a different-size object is likewise a conflict: names
///   alone never authorize a deletion.
fn resolve_existing_final(identity: &RecoveryIdentity, partial_path: &Path) -> RecoveryOutcome {
    let original_name = partial_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if tombstone_proves_transaction(identity, original_name) {
        let original_removed = match std::fs::remove_file(partial_path) {
            Ok(()) => true,
            // Already gone: the transaction has converged — the cleanup
            // goal is achieved whoever performed it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        return RecoveryOutcome::AlreadyRecovered {
            partial_path: partial_path.to_path_buf(),
            final_path: identity.final_path.clone(),
            original_removed,
        };
    }
    let reason = if tombstone_names_transaction(identity, original_name) {
        "trusted tombstone names this transaction, but the object at the recovered path is no longer the published recording (replaced, truncated, or not a regular file)"
    } else if identity.tombstone.is_file() {
        "recovered destination exists but its tombstone is untrusted for this transaction (malformed, legacy, or foreign)"
    } else if identity.final_path.is_dir() {
        "recovered destination exists as a directory"
    } else {
        "recovered destination exists without trusted transaction evidence (possible crash after publication, or a foreign file)"
    };
    RecoveryOutcome::RecoveryConflict {
        partial_path: partial_path.to_path_buf(),
        final_path: identity.final_path.clone(),
        reason: reason.to_owned(),
    }
}

/// Process-GLOBAL monotonic serial for per-attempt scratch names (final
/// safety remediation §6): every claim — concurrent or sequential, across
/// retries — takes the next value, so the tag is unique per attempt within
/// this process by construction, not merely per call site.
static SCRATCH_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Builds the unique-attempt tag `<pid>-<serial>-<nanos>`: the pid
/// distinguishes concurrent PROCESSES, the process-global serial
/// distinguishes every claim attempt in this process, and the clock nanos
/// break any exotic pid/counter reuse. `create_new` remains the final
/// no-replace arbiter.
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
    let mut last_error: Option<StorageError> = None;
    for _ in 0..8 {
        let serial = SCRATCH_SERIAL.fetch_add(1, Ordering::Relaxed);
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

/// Consumes the muxer — closing the output handle on success AND failure
/// alike (Windows-first) — and flattens its result for the typed artifact
/// mapping.
fn finalize_muxer(muxer: MatroskaMuxer) -> Result<String, String> {
    muxer
        .finalize()
        .map(|published| published.display().to_string())
        .map_err(|error| error.to_string())
}

#[cfg(any(test, feature = "test-hooks"))]
fn alignment_gate_wait() {
    // Clone the Arc OUT of the mutex: parking while holding the registry
    // lock would deadlock the test side.
    let gate = HOOK_ALIGNMENT_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(std::sync::Arc::clone);
    if let Some(gate) = gate {
        gate.park();
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn publish_hold_wait() {
    let hold = HOOK_PUBLISH_HOLD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(std::sync::Arc::clone);
    if let Some(hold) = hold {
        hold.park();
    }
}

/// Recovers all classifiable partials of one camera.
///
/// Standalone callers acquire the same kernel-backed [`CameraLease`] used by
/// production before scanning. If another process owns the camera, recovery
/// returns a typed ownership failure without scanning or touching recording
/// artifacts. Genuine lock-file/open filesystem failures remain infrastructure.
/// Production jobs that already hold the lease use
/// [`recover_camera_partials_with_interrupt`] so ownership remains held across
/// recovery, connecting, recording, backoff, reconnects, and shutdown.
///
/// Conservative per-file containment: one failure does not abort other
/// files' recovery. Returns outcomes in deterministic (scan) order plus the
/// failures encountered. Nothing outside the canonical layout tree is ever
/// touched.
pub fn recover_camera_partials(
    layout: &RecordingsLayout,
    camera: &CameraId,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let lease = match CameraLease::try_acquire(layout, camera) {
        Ok(lease) => lease,
        Err(source) => {
            let error = match source {
                source @ StorageError::CameraAlreadyActive { .. } => RecoveryError::Ownership {
                    operation: "acquire camera lease",
                    source,
                },
                source => RecoveryError::Infrastructure {
                    operation: "acquire camera lease",
                    source,
                },
            };
            return (
                Vec::new(),
                vec![RecoveryFailure {
                    partial_path: layout.camera_dir(camera),
                    error,
                }],
            );
        }
    };
    let interrupt = InterruptHandle::new();
    recover_camera_partials_with_interrupt(layout, camera, &lease, &interrupt, None)
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
    lease: &CameraLease,
    interrupt: &InterruptHandle,
    graceful_stop: Option<&StopFlag>,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    if let Err(source) = lease.verify(layout, camera) {
        return (
            Vec::new(),
            vec![RecoveryFailure {
                partial_path: layout.camera_dir(camera),
                error: RecoveryError::Ownership {
                    operation: "validate camera lease",
                    source,
                },
            }],
        );
    }

    let partials = scan_camera_partials(layout, camera, lease);
    recover_camera_partials_from_scan(layout, camera, partials, interrupt, graceful_stop)
}

/// Recovery engine below the ownership boundary. Production code must enter
/// through a lease-checking wrapper; unit tests use this only for deliberate
/// transaction-race coverage that predates the camera-wide lease.
#[cfg(test)]
fn recover_camera_partials_unleased_with_interrupt(
    layout: &RecordingsLayout,
    camera: &CameraId,
    interrupt: &InterruptHandle,
    graceful_stop: Option<&StopFlag>,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let partials =
        nian_storage::recovery::scan_camera_partials_without_lease_for_test(layout, camera);
    recover_camera_partials_from_scan(layout, camera, partials, interrupt, graceful_stop)
}

fn recover_camera_partials_from_scan(
    layout: &RecordingsLayout,
    camera: &CameraId,
    partials: Result<Vec<PartialFile>, StorageError>,
    interrupt: &InterruptHandle,
    graceful_stop: Option<&StopFlag>,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();

    let partials = match partials {
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
    //
    // Final safety remediation §1: pathname existence alone is NEVER proof.
    // The destination is resolved through the transaction contract — a
    // trusted tombstone yields `AlreadyRecovered` with a cleanup retry;
    // anything else yields `RecoveryConflict` preserving both files. This
    // never opens the demuxer in either branch.
    let identity =
        recovery_identity(&partial.partial_path).ok_or_else(|| RecoveryError::Unreadable {
            message: "partial name carries no canonical recovery identity".to_owned(),
        })?;
    if identity.final_path.exists() {
        return Ok(resolve_existing_final(&identity, &partial.partial_path));
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
    //
    // Final safety remediation §2: an EXPLICIT loop — graceful stop and
    // forced cancellation are checked BEFORE every packet read and
    // classified deliberately after every error. The previous
    // `while let Some(...) = next_packet().ok().flatten()` shape collapsed
    // EOF, media errors, timeouts and cancellation into one silent
    // None-path that could never observe a stop during alignment.
    //
    // Identity safety remediation §4: after a FAILED read, BOTH stop
    // domains are checked again BEFORE any content verdict — a graceful
    // stop or cancellation that raced the failing read keeps the recovery
    // status honest (`Cancelled`), never a false "unreadable".
    let mut found_keyframe = false;
    loop {
        #[cfg(any(test, feature = "test-hooks"))]
        // Deterministic seam: park INSIDE the alignment phase before the
        // first packet read (final safety remediation §2 tests).
        alignment_gate_wait();
        if stop_requested() || interrupt.is_cancelled() {
            return Err(RecoveryError::Cancelled);
        }
        let read = {
            #[cfg(any(test, feature = "test-hooks"))]
            {
                let hold_ms = HOOK_ALIGN_READ_FAIL_MS.load(Ordering::SeqCst);
                if hold_ms != u64::MAX {
                    // Deterministic read-error seam (identity safety §4):
                    // the read "fails" after the configured hold, so tests
                    // can land a stop request INSIDE the failing read.
                    std::thread::sleep(Duration::from_millis(hold_ms));
                    Err::<std::option::Option<_>, String>(
                        "injected alignment read failure".to_owned(),
                    )
                } else {
                    input.next_packet().map_err(|error| error.to_string())
                }
            }
            #[cfg(not(any(test, feature = "test-hooks")))]
            {
                input.next_packet().map_err(|error| error.to_string())
            }
        };
        match read {
            Ok(Some(packet)) => {
                let metadata = packet.metadata();
                if metadata.stream_index == video_index && metadata.keyframe {
                    found_keyframe = true;
                    break;
                }
            }
            Ok(None) => break, // readable span exhausted: no keyframe inside
            Err(error) => {
                // Stop domains WIN over content verdicts (identity safety
                // remediation §4): both the graceful StopFlag and forced
                // cancellation are re-checked after the failed read before
                // declaring the media unreadable. Only when neither stop
                // domain is active is this a deliberate per-file content
                // classification.
                if interrupt.is_cancelled() || stop_requested() {
                    return Err(RecoveryError::Cancelled);
                }
                return Ok(RecoveryOutcome::KeptUnrecoverable {
                    partial_path: partial.partial_path.clone(),
                    reason: format!("media read failed during keyframe alignment: {error}"),
                });
            }
        }
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
    // Final safety remediation §5 test seam: an output-OPEN failure AFTER a
    // successful scratch claim — the muxer target is pointed at an
    // un-creatable location so the failure flows through the real backend
    // path while `scratch` stays the cleanup target.
    #[cfg(any(test, feature = "test-hooks"))]
    let muxer_target = if HOOK_BREAK_OUTPUT_OPEN.load(Ordering::SeqCst) {
        day_dir
            .join("missing-parent")
            .join(scratch.file_name().unwrap_or_default())
    } else {
        scratch.clone()
    };
    #[cfg(not(any(test, feature = "test-hooks")))]
    let muxer_target = scratch.clone();
    let mut muxer =
        match MatroskaMuxer::create_with_selection(&mut input, &muxer_target, interrupt, |info| {
            selection
                .iter()
                .any(|s| s.stream_index == info.stream_index)
        }) {
            Ok(muxer) => muxer,
            Err(error) => {
                // Output-side failure on THIS attempt's own scratch (final
                // safety remediation §5): never evidence about the
                // original's content — a typed ARTIFACT failure. Only this
                // attempt's scratch is removed; nothing publishes; the
                // original stays untouched.
                let _ = std::fs::remove_file(&scratch);
                return Err(RecoveryError::Artifact {
                    operation: "open the recovery output",
                    source: StorageError::Io {
                        path: scratch,
                        source: std::io::Error::other(format!(
                            "salvage output could not be opened: {error}"
                        )),
                    },
                });
            }
        };

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
        // Final safety remediation §5: a mux/output write failure is an
        // OUTPUT-side problem on this attempt's own scratch — a typed
        // ARTIFACT failure, never a content verdict about the original.
        // Nothing was finalized or published; the original stays intact.
        return Err(RecoveryError::Artifact {
            operation: "write the recovery output",
            source: StorageError::Io {
                path: scratch.clone(),
                source: std::io::Error::other(
                    "salvage output write failed; output poisoned, never published",
                ),
            },
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
    // Final safety remediation §5: a finalize/trailer/flush failure is an
    // OUTPUT-side problem on this attempt's own scratch — a typed ARTIFACT
    // failure, never a verdict about the original's content. The muxer is
    // CONSUMED by `finalize` (its handle is closed on success and failure
    // alike, Windows-first), then this attempt's scratch is removed and
    // NOTHING publishes.
    let finalize_result = {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            if HOOK_FAIL_FINALIZE.load(Ordering::SeqCst) {
                drop(muxer); // close the output handle before scratch removal
                Err("injected finalize failure".to_owned())
            } else {
                finalize_muxer(muxer)
            }
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        {
            finalize_muxer(muxer)
        }
    };
    if let Err(message) = finalize_result {
        let _ = std::fs::remove_file(&scratch);
        return Err(RecoveryError::Artifact {
            operation: "finalize the recovery output",
            source: StorageError::Io {
                path: scratch,
                source: std::io::Error::other(format!(
                    "salvaged output failed to finalize: {message}"
                )),
            },
        });
    }

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
            // recording slot. Final safety remediation §1: the SAME
            // transaction contract applies here as at the early recognition
            // path — a TRUSTED tombstone yields `AlreadyRecovered` with a
            // cleanup retry; a missing/untrusted tombstone (the concurrent-
            // loser window: the winner has not yet written its tombstone,
            // or a foreign file) yields a preserved `RecoveryConflict`.
            // This attempt removes ONLY its own scratch, NEVER deletes the
            // original on inference, and NEVER writes a retroactive
            // tombstone.
            StorageError::DestinationExists { .. } => {
                let _ = std::fs::remove_file(&scratch);
                Ok(resolve_existing_final(&identity, &partial.partial_path))
            }
            other => Err(RecoveryError::Artifact {
                operation: "publish the recovered recording",
                source: other,
            }),
        };
    }

    // Final safety remediation §1 test seam: hold JUST past the durable
    // publication commit, BEFORE the tombstone — the exact concurrent-loser
    // window (the destination exists, no trusted tombstone yet).
    #[cfg(any(test, feature = "test-hooks"))]
    publish_hold_wait();

    // ---- 10. Source closed FIRST, tombstone, then observable cleanup ------
    drop(input);

    // Tombstone strictly AFTER publication: its persistence failure is
    // observable, but the deterministic identity keeps idempotency intact.
    // No stop gate here: the transaction already crossed its durable commit
    // point, so the tiny post-publication bookkeeping always completes.
    // The recorded size is the PUBLISHED file's exact size — the v2
    // evidence binding (identity safety remediation §1).
    let original_name = partial
        .partial_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let tombstone_recorded = record_tombstone(&identity, &original_name, size_bytes);

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
mod ownership_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn standalone_recovery_classifies_active_camera_as_ownership_not_infrastructure() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-owned").unwrap();
        let _holder = CameraLease::try_acquire(&layout, &camera).unwrap();

        let (outcomes, failures) = recover_camera_partials(&layout, &camera);

        assert!(outcomes.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(!failures[0].error.is_infrastructure());
        assert!(matches!(
            failures[0].error,
            RecoveryError::Ownership {
                source: StorageError::CameraAlreadyActive { .. },
                ..
            }
        ));
    }

    #[test]
    fn mismatched_lease_is_ownership_contract_failure_before_scan() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera_a = CameraId::parse("cam-a").unwrap();
        let camera_b = CameraId::parse("cam-b").unwrap();
        let lease = CameraLease::try_acquire(&layout, &camera_a).unwrap();

        // Make camera B's root deliberately unscannable as a directory. If
        // recovery ignored the lease contract and scanned anyway, this would
        // become an infrastructure error instead of the ownership error below.
        let camera_b_root = layout.camera_dir(&camera_b);
        std::fs::create_dir_all(camera_b_root.parent().unwrap()).unwrap();
        std::fs::write(&camera_b_root, b"not a directory").unwrap();

        let interrupt = InterruptHandle::new();
        let (outcomes, failures) =
            recover_camera_partials_with_interrupt(&layout, &camera_b, &lease, &interrupt, None);

        assert!(outcomes.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(!failures[0].error.is_infrastructure());
        assert!(matches!(
            failures[0].error,
            RecoveryError::Ownership {
                source: StorageError::CameraLeaseMismatch { .. },
                ..
            }
        ));
    }
}

#[cfg(test)]
mod tombstone_contract_tests {
    //! Pure unit coverage for the tombstone transaction contract (final
    //! safety remediation §1 + identity safety remediation §1): the trusted
    //! v2 format, strict parsing (legacy v1 rejected), the name binding
    //! that makes evidence transaction-specific, and the SIZE binding to
    //! the object currently at the final path.

    use super::*;

    #[test]
    fn trusted_tombstone_parses_and_binds_names_and_size() {
        let payload = tombstone_payload("08-30-00.partial.mkv", "08-30-00.recovered.mkv", 123_456);
        let parsed = parse_tombstone(payload.as_bytes()).expect("valid payload must parse");
        assert_eq!(parsed.original, "08-30-00.partial.mkv");
        assert_eq!(parsed.final_name, "08-30-00.recovered.mkv");
        assert_eq!(parsed.size_bytes, 123_456);
    }

    #[test]
    fn foreign_or_malformed_marker_content_is_never_trusted() {
        // Arbitrary/foreign `.done` contents fail the strict parser.
        for bad in [
            "",                                                                 // empty
            "nian-vision recovery tombstone\noriginal: a\nfinal: b\nsize: 1\n", // foreign format
            // Legacy v1 evidence: rejected by version (identity safety §1).
            "NIAN-RECOVERY-TOMBSTONE v1\noriginal: a\nfinal: b\n",
            "NIAN-RECOVERY-TOMBSTONE v1\noriginal: a\nfinal: b\nsize: 1\n",
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\n", // missing size
            "NIAN-RECOVERY-TOMBSTONE v2\nfinal: b\noriginal: a\nsize: 1\n", // wrong order
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: \nfinal: b\nsize: 1\n", // empty original
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\nsize: \n", // empty size
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\nsize: twelve\n", // non-numeric
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\nsize: -1\n", // negative
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\nsize: 1\nextra: x\n", // extra content
            "NIAN-RECOVERY-TOMBSTONE v2\noriginal: a\nfinal: b\nsize: 1\ntrailing\n",
        ] {
            assert!(
                parse_tombstone(bad.as_bytes()).is_none(),
                "malformed tombstone must be rejected: {bad:?}"
            );
        }
        // Non-UTF8 bytes are foreign content too.
        assert!(parse_tombstone(&[0xff, 0xfe, 0xfd]).is_none());
    }

    #[test]
    fn tombstone_evidence_is_bound_to_the_exact_transaction_and_object() {
        // A trusted tombstone for original A proves NOTHING for original B
        // sharing the same day directory, and a v2 tombstone proves NOTHING
        // once the object at the final path no longer matches the recorded
        // published size (identity safety remediation §1: name matching
        // AND the size binding are both required).
        let dir = tempfile::tempdir().unwrap();
        let camera_day = dir.path().join("day");
        std::fs::create_dir_all(&camera_day).unwrap();
        let original_a = camera_day.join("08-30-00.partial.mkv");
        std::fs::write(&original_a, b"x").unwrap();
        let identity = recovery_identity(&original_a).unwrap();
        // The published final must EXIST and match the recorded size.
        std::fs::write(&identity.final_path, vec![0u8; 4096]).unwrap();
        assert!(record_tombstone(&identity, "08-30-00.partial.mkv", 4096));
        assert!(tombstone_proves_transaction(
            &identity,
            "08-30-00.partial.mkv"
        ));
        assert!(!tombstone_proves_transaction(
            &identity,
            "08-30-01.partial.mkv"
        ));
        // The destination name is part of the binding too.
        assert!(!tombstone_proves_transaction(
            &RecoveryIdentity {
                final_path: camera_day.join("OTHER.recovered.mkv"),
                tombstone: identity.tombstone.clone(),
            },
            "08-30-00.partial.mkv"
        ));
        // The SIZE binding: a truncated replacement at the same pathname
        // invalidates the evidence even though every name still matches.
        std::fs::write(&identity.final_path, vec![0u8; 4095]).unwrap();
        assert!(
            !tombstone_proves_transaction(&identity, "08-30-00.partial.mkv"),
            "a size-mismatched replacement must never pass as the published object"
        );
        // A zero-byte replacement likewise.
        std::fs::write(&identity.final_path, b"").unwrap();
        assert!(!tombstone_proves_transaction(
            &identity,
            "08-30-00.partial.mkv"
        ));
        // A DIRECTORY at the final pathname likewise.
        std::fs::remove_file(&identity.final_path).unwrap();
        std::fs::create_dir_all(&identity.final_path).unwrap();
        assert!(!tombstone_proves_transaction(
            &identity,
            "08-30-00.partial.mkv"
        ));
    }
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
        pub(super) fn new() -> Self {
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
        // Final safety remediation §5: a mux/output write failure is a
        // typed ARTIFACT failure on the output side — never a content
        // verdict about the original.
        assert!(failures.len() == 1, "{failures:?}");
        assert!(
            matches!(
                failures[0].error,
                RecoveryError::Artifact {
                    operation: "write the recovery output",
                    ..
                }
            ),
            "write failure must be a typed artifact failure: {failures:?}"
        );
        assert!(!failures[0].error.is_infrastructure());
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

    /// Writes a TRUSTED tombstone for `original_path` through the
    /// production recorder — the exact bytes a real winning pass persists.
    /// The tombstone records the CURRENT size of the published final, the
    /// v2 object binding (identity safety remediation §1).
    fn seed_trusted_tombstone(original_path: &Path) {
        let identity = recovery_identity(original_path).unwrap();
        let original_name = original_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let size = std::fs::metadata(&identity.final_path)
            .expect("seeding requires the published final to exist")
            .len();
        assert!(
            record_tombstone(&identity, &original_name, size),
            "seeding the trusted tombstone must succeed"
        );
    }

    #[test]
    fn valid_transaction_tombstone_yields_already_recovered_and_cleanup_retry() {
        // Final safety remediation §1, case B (review row 5): the final
        // exists AND a trusted tombstone proves THIS original→final
        // transaction → AlreadyRecovered, the cleanup retry removes the
        // original, and no second recording is ever produced. The old
        // "repair" semantics (retroactively creating a tombstone for a
        // pre-existing destination) are FORBIDDEN by §1 and replaced by
        // the conflict contract.
        let storage = Storage::new();
        let (original_name, original_path) = seed_original(&storage);
        let payload = std::fs::read(&original_path).unwrap();

        // Simulate an earlier pass: published final + trusted tombstone,
        // then crashed BEFORE the original's cleanup.
        let base = original_name.strip_suffix(".partial.mkv").unwrap();
        let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::write(&final_path, &payload).unwrap();
        seed_trusted_tombstone(&original_path);

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
            "the trusted tombstone persists"
        );
        assert_eq!(count_recordings(&storage.day_dir), 1);
        assert_eq!(std::fs::read(&final_path).unwrap(), payload);
    }

    #[test]
    fn recovery_conflict_when_destination_path_is_a_directory() {
        // Final safety remediation §1 (review row 1): a DIRECTORY at the
        // deterministic recovered pathname is never proof that the original
        // was recovered. The original must survive untouched, the
        // destination must survive untouched, and no retroactive success
        // tombstone may appear.
        let storage = Storage::new();
        let (name, original_path) = seed_original(&storage);
        let base = name.strip_suffix(".partial.mkv").unwrap();
        let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::create_dir_all(&final_path).unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert!(
            failures.is_empty(),
            "a conflict is an outcome, never a failure: {failures:?}"
        );
        let conflicts: Vec<_> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                RecoveryOutcome::RecoveryConflict {
                    partial_path,
                    final_path,
                    reason,
                } => Some((partial_path.clone(), final_path.clone(), reason.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(conflicts.len(), 1, "{outcomes:?}");
        assert_eq!(conflicts[0].0, original_path);
        assert_eq!(conflicts[0].1, final_path);
        assert!(original_path.is_file(), "the original must survive");
        assert!(final_path.is_dir(), "the destination must survive");
        assert!(
            !storage
                .day_dir
                .join(format!("{base}.recovered.mkv.done"))
                .exists(),
            "no success tombstone may be created retroactively"
        );
    }

    #[test]
    fn recovery_conflict_when_destination_is_a_zero_byte_or_foreign_file() {
        // Final safety remediation §1 (review rows 2+3): a zero-byte file
        // or a foreign VALID media file at the deterministic pathname is
        // never transaction evidence — the original must survive and the
        // destination's bytes must stay untouched.
        for (label, content) in [
            ("zero-byte", Vec::new()),
            (
                "foreign-valid-mkv",
                std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
            ),
        ] {
            let storage = Storage::new();
            let (name, original_path) = seed_original(&storage);
            let base = name.strip_suffix(".partial.mkv").unwrap();
            let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
            std::fs::write(&final_path, &content).unwrap();

            let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

            assert!(
                failures.is_empty(),
                "{label}: a conflict is an outcome, never a failure: {failures:?}"
            );
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
                "{label}: the pass must report a conflict: {outcomes:?}"
            );
            assert!(
                original_path.is_file(),
                "{label}: the original must survive"
            );
            assert_eq!(
                std::fs::read(&final_path).unwrap(),
                content,
                "{label}: the destination bytes must stay untouched"
            );
            assert!(
                !storage
                    .day_dir
                    .join(format!("{base}.recovered.mkv.done"))
                    .exists(),
                "{label}: no success tombstone may be created retroactively"
            );
            assert!(
                !std::fs::read_dir(&storage.day_dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().contains(".recovery-")),
                "{label}: no scratch may be claimed for a conflict"
            );
        }
    }

    #[test]
    fn recovery_conflict_when_tombstone_is_malformed_or_foreign() {
        // Final safety remediation §1 (review row 4): a `.done` file is
        // trusted ONLY with the exact tombstone structure naming THIS
        // transaction. Malformed content, foreign markers and
        // wrong-name tombstones all yield a preserved conflict — never a
        // deletion of the original.
        let setups: [(&str, String); 3] = [
            ("malformed", "just some operator marker\n".to_owned()),
            ("empty", String::new()),
            (
                "wrong-original",
                tombstone_payload("08-30-01.partial.mkv", "08-30-00.recovered.mkv", 12),
            ),
        ];
        for (label, content) in setups {
            let storage = Storage::new();
            let (name, original_path) = seed_original(&storage);
            let base = name.strip_suffix(".partial.mkv").unwrap();
            let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
            std::fs::write(&final_path, b"destination").unwrap();
            std::fs::write(
                storage.day_dir.join(format!("{base}.recovered.mkv.done")),
                content.as_bytes(),
            )
            .unwrap();

            let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

            assert!(failures.is_empty(), "{label}: {failures:?}");
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
                "{label}: an untrusted tombstone must yield a conflict: {outcomes:?}"
            );
            assert!(
                original_path.is_file(),
                "{label}: the original must survive"
            );
            assert_eq!(
                std::fs::read(&final_path).unwrap(),
                b"destination",
                "{label}: the destination must survive"
            );
        }
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
        seed_trusted_tombstone(&original_path);

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
    fn loser_never_deletes_original_before_tombstone_evidence_exists() {
        // Final safety remediation §1 (review row 6, the race window): the
        // winner PUBLISHES and parks before its tombstone; a second
        // attempt observing the destination at that moment must NOT delete
        // the original — it reports the pending conflict and leaves the
        // cleanup to the winner. After release, the winner's trusted
        // tombstone + cleanup converge the tree.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_hook_guard, hold) = arm_publish_hold();
        let storage = Storage::new();
        let (name, original_path) = seed_original(&storage);
        let base = name.strip_suffix(".partial.mkv").unwrap();

        let layout = storage.layout.clone();
        let camera = storage.camera.clone();
        let winner = std::thread::spawn(move || {
            let interrupt = InterruptHandle::new();
            recover_camera_partials_unleased_with_interrupt(&layout, &camera, &interrupt, None)
        });

        // The winner has PUBLISHED and is parked BEFORE its tombstone.
        hold.wait_arrived();

        // The loser observes the destination INSIDE the no-evidence window.
        let loser_interrupt = InterruptHandle::new();
        let (loser_outcomes, loser_failures) = recover_camera_partials_unleased_with_interrupt(
            &storage.layout,
            &storage.camera,
            &loser_interrupt,
            None,
        );
        assert!(loser_failures.is_empty(), "{loser_failures:?}");
        assert!(
            loser_outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
            "the loser must report a pending conflict: {loser_outcomes:?}"
        );
        assert!(
            original_path.is_file(),
            "the loser must NEVER delete the original before trusted evidence exists"
        );
        assert!(
            !storage
                .day_dir
                .join(format!("{base}.recovered.mkv.done"))
                .exists(),
            "the loser must never write a retroactive tombstone"
        );

        // The winner completes its own transaction.
        hold.release();
        let (winner_outcomes, winner_failures) = winner.join().unwrap();
        assert!(winner_failures.is_empty(), "{winner_failures:?}");
        assert!(winner_outcomes.iter().any(|outcome| matches!(
            outcome,
            RecoveryOutcome::Recovered {
                tombstone_recorded: true,
                original_removed: true,
                ..
            }
        )));

        // Convergence: one final, trusted tombstone, original gone.
        assert_eq!(count_recordings(&storage.day_dir), 1);
        assert!(
            tombstone_proves_transaction(
                &recovery_identity(&original_path).unwrap(),
                &original_path.file_name().unwrap().to_string_lossy()
            ),
            "the winner's tombstone must be trusted evidence"
        );
        assert!(!original_path.exists());
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
        // Final correctness remediation §1 + final safety remediation §1:
        // two process-shaped attempts against the SAME original. The
        // publication barrier releases both at their commit step
        // deterministically; the deterministic final arbitrates. Exactly
        // one final, unique scratch pathnames, and the loser resolves the
        // collision through the SAME transaction contract as every other
        // path: a trusted tombstone in time → AlreadyRecovered (deletion
        // WITH evidence); the winner's not-yet-tombstoned window →
        // RecoveryConflict (original left to the winner's cleanup).
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
            let attempt = move || {
                let interrupt = InterruptHandle::new();
                recover_camera_partials_unleased_with_interrupt(&layout, &camera, &interrupt, None)
            };
            std::thread::spawn(attempt)
        };
        let interrupt_a = InterruptHandle::new();
        let res_a = recover_camera_partials_unleased_with_interrupt(
            &storage.layout,
            &storage.camera,
            &interrupt_a,
            None,
        );
        let res_b = handle.join().unwrap();

        let mut recovered = 0usize;
        let mut loser_kinds: Vec<&'static str> = Vec::new();
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
                        loser_kinds.push("already");
                        finals.push(final_path);
                        // The loser saw the winner's trusted tombstone in
                        // time and retried the cleanup: deletion WITH
                        // evidence; either unlink outcome is honest.
                        let _ = original_removed;
                    }
                    RecoveryOutcome::RecoveryConflict {
                        final_path, reason, ..
                    } => {
                        loser_kinds.push("conflict");
                        finals.push(final_path);
                        assert!(!reason.is_empty(), "conflicts report an observable reason");
                    }
                    other => panic!("unexpected outcome in concurrent run: {other:?}"),
                }
            }
        }
        assert_eq!(recovered, 1, "exactly one attempt publishes");
        assert_eq!(loser_kinds.len(), 1, "exactly one loser");
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

        // Convergence: exactly ONE recovered final, the winner's trusted
        // tombstone persists, the original is gone (winner cleanup), and
        // no scratch remains. The final independently demuxes.
        assert_eq!(count_recordings(&storage.day_dir), 1);
        assert!(
            tombstone_proves_transaction(
                &recovery_identity(&original_path).unwrap(),
                &original_path.file_name().unwrap().to_string_lossy()
            ),
            "the winner's tombstone must be trusted evidence"
        );
        assert!(!original_path.exists());
        let leftovers: Vec<String> = std::fs::read_dir(&storage.day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.iter().all(|name| !name.contains(".recovery-")),
            "no scratch may survive a converged transaction: {leftovers:?}"
        );
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
        let _ = original_payload;
    }

    #[test]
    fn graceful_stop_during_keyframe_alignment_abandons_at_the_packet_boundary() {
        // Final safety remediation §2: the alignment probe is an EXPLICIT
        // loop that checks the graceful-stop domain before EVERY packet
        // read. The attempt parks INSIDE the alignment phase (before its
        // first packet, deterministically); ONE stop request makes it exit
        // at the next packet boundary with a typed cancellation — no
        // scratch is ever claimed, nothing publishes, the original stays,
        // and no force-cancel is needed.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_hook_guard, gate) = arm_alignment_gate();
        let storage = Storage::new();
        let (_, original_path) = seed_original(&storage);

        let stop = StopFlag::new();
        let stop_for_worker = stop.clone();
        let layout = storage.layout.clone();
        let camera = storage.camera.clone();
        let worker = std::thread::spawn(move || {
            let interrupt = InterruptHandle::new();
            let lease = CameraLease::try_acquire(&layout, &camera).unwrap();
            recover_camera_partials_with_interrupt(
                &layout,
                &camera,
                &lease,
                &interrupt,
                Some(&stop_for_worker),
            )
        });

        // Parked INSIDE the alignment probe → press stop ONCE → release.
        gate.wait_arrived();
        stop.request();
        gate.release();

        let (outcomes, failures) = worker.join().unwrap();
        assert!(
            failures
                .iter()
                .all(|failure| matches!(failure.error, RecoveryError::Cancelled)),
            "alignment-phase stop is a typed cancellation: {failures:?}"
        );
        assert!(
            outcomes.is_empty(),
            "nothing may be reported as published: {outcomes:?}"
        );
        // The probe never completed: NO scratch was ever claimed.
        assert!(
            HOOK_SCRATCHES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the alignment phase must exit before scratch acquisition"
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
            "no scratch artifact may appear: {names:?}"
        );
    }

    #[test]
    fn finalize_failure_is_a_typed_artifact_failure_never_published() {
        // Final safety remediation §5: a muxer finalize/trailer failure is
        // an OUTPUT-side problem — a typed ARTIFACT failure, never a
        // verdict about the original's content. Nothing publishes; this
        // attempt's scratch is removed; the original stays intact.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_finalize_failure();
        let storage = Storage::new();
        seed_original(&storage);

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            matches!(
                failures[0].error,
                RecoveryError::Artifact {
                    operation: "finalize the recovery output",
                    ..
                }
            ),
            "finalize failure must be a typed artifact failure: {failures:?}"
        );
        assert!(!failures[0].error.is_infrastructure());
        assert!(
            outcomes.is_empty(),
            "nothing may publish after a finalize failure: {outcomes:?}"
        );
        assert_eq!(count_files(&storage.day_dir, false), 0);
        assert_eq!(count_files(&storage.day_dir, true), 1, "original intact");
    }

    #[test]
    fn output_open_failure_is_a_typed_artifact_failure_never_published() {
        // Final safety remediation §5: an output-OPEN failure AFTER a
        // successful scratch claim is a per-attempt ARTIFACT failure (the
        // claim proved the storage root still accepts new files). Nothing
        // publishes; the claimed scratch is cleaned up; the original
        // stays intact.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_output_open_fault();
        let storage = Storage::new();
        seed_original(&storage);

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            matches!(
                failures[0].error,
                RecoveryError::Artifact {
                    operation: "open the recovery output",
                    ..
                }
            ),
            "output-open failure must be a typed artifact failure: {failures:?}"
        );
        assert!(!failures[0].error.is_infrastructure());
        assert!(outcomes.is_empty(), "{outcomes:?}");
        // The claim happened (one scratch), but its cleanup removed it.
        assert_eq!(
            HOOK_SCRATCHES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        assert_eq!(count_files(&storage.day_dir, false), 0);
        assert_eq!(count_files(&storage.day_dir, true), 1, "original intact");
    }

    #[test]
    fn trusted_tombstone_with_a_replaced_destination_is_a_conflict_never_a_deletion() {
        // Identity safety remediation §1: a valid v2 tombstone proves only
        // the PUBLISHED object. When the deterministic final pathname has
        // SINCE become a directory, a zero-byte file, or a different-size
        // replacement, the evidence must NOT authorize deleting the
        // original — the pass reports a conflict, preserves both objects,
        // and never repairs the tombstone retroactively. No test may
        // authorize an original deletion purely from names.
        fn replace_with_directory(final_path: &Path) {
            std::fs::remove_file(final_path).unwrap();
            std::fs::create_dir_all(final_path).unwrap();
        }
        fn replace_with_zero_byte(final_path: &Path) {
            std::fs::write(final_path, b"").unwrap();
        }
        fn replace_with_other_size(final_path: &Path) {
            std::fs::write(final_path, b"a replacement of another length").unwrap();
        }
        for (label, replace) in [
            ("directory", replace_with_directory as fn(&Path)),
            ("zero-byte", replace_with_zero_byte as fn(&Path)),
            ("different-size", replace_with_other_size as fn(&Path)),
        ] {
            let storage = Storage::new();
            let (name, original_path) = seed_original(&storage);
            let base = name.strip_suffix(".partial.mkv").unwrap();
            let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
            // The EARLIER pass published this final and tombstoned it:
            std::fs::write(
                &final_path,
                std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
            )
            .unwrap();
            seed_trusted_tombstone(&original_path);
            // …then the pathname was REPLACED by a foreign object:
            replace(&final_path);
            // The object that must survive is the REPLACEMENT, whatever it
            // is (the point is that NEITHER side is touched by recovery).
            let surviving_object: Vec<u8> = match label {
                "directory" => Vec::new(), // checked via is_dir below
                "zero-byte" => Vec::new(),
                _ => b"a replacement of another length".to_vec(),
            };

            let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

            assert!(
                failures.is_empty(),
                "{label}: a conflict is an outcome, never a failure: {failures:?}"
            );
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
                "{label}: a replaced destination must be a conflict: {outcomes:?}"
            );
            assert!(
                !outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
                "{label}: the stale tombstone must NOT authorize AlreadyRecovered: {outcomes:?}"
            );
            assert!(
                original_path.is_file(),
                "{label}: the original must survive — names alone never authorize deletion"
            );
            if label == "directory" {
                assert!(final_path.is_dir(), "{label}: destination preserved");
            } else {
                assert_eq!(
                    std::fs::read(&final_path).unwrap(),
                    surviving_object,
                    "{label}: the destination object must stay untouched"
                );
            }
            assert!(
                !std::fs::read_dir(&storage.day_dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().contains(".recovery-")),
                "{label}: no scratch may be claimed for a conflict"
            );
        }
    }

    #[test]
    fn legacy_v1_tombstones_are_rejected_as_untrusted_conflicts() {
        // Identity safety remediation §1, backward compatibility: v1
        // markers bound only NAMES, insufficient to prove the object at
        // the final pathname. This pre-v1 project rejects them as
        // untrusted (case C conflict, both files preserved) and never
        // silently treats v1 as equivalent to v2.
        let storage = Storage::new();
        let (name, original_path) = seed_original(&storage);
        let base = name.strip_suffix(".partial.mkv").unwrap();
        let final_path = storage.day_dir.join(format!("{base}.recovered.mkv"));
        std::fs::write(
            &final_path,
            std::fs::read(fixtures_dir().join("sample_av.mkv")).unwrap(),
        )
        .unwrap();
        // An EXACT v1-format tombstone with fully matching names:
        let v1_payload =
            format!("NIAN-RECOVERY-TOMBSTONE v1\noriginal: {name}\nfinal: {base}.recovered.mkv\n");
        std::fs::write(
            storage.day_dir.join(format!("{base}.recovered.mkv.done")),
            v1_payload.as_bytes(),
        )
        .unwrap();

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert!(failures.is_empty(), "{failures:?}");
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
            "legacy v1 evidence must yield a conflict: {outcomes:?}"
        );
        assert!(
            !outcomes
                .iter()
                .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
            "v1 evidence must never authorize the original's deletion: {outcomes:?}"
        );
        assert!(original_path.is_file(), "the original must survive");
        assert!(final_path.is_file(), "the destination must survive");
    }

    #[test]
    fn alignment_read_error_is_a_content_verdict_when_no_stop_domain_is_active() {
        // Identity safety remediation §4, verdict branch: a read failure
        // during keyframe alignment with NEITHER stop domain active is an
        // honest per-file content classification — no publication, no
        // scratch, the original preserved.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_alignment_read_failure(0);
        let storage = Storage::new();
        let (_, original_path) = seed_original(&storage);

        let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

        assert!(failures.is_empty(), "{failures:?}");
        let kept = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                RecoveryOutcome::KeptUnrecoverable {
                    partial_path,
                    reason,
                } if reason.contains("media read failed during keyframe alignment") => {
                    Some(partial_path.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(kept, vec![original_path.clone()], "{outcomes:?}");
        assert!(original_path.is_file(), "the original must survive");
        assert!(HOOK_SCRATCHES.lock().unwrap().is_empty());
        assert_eq!(count_recordings(&storage.day_dir), 0);
    }

    #[test]
    fn alignment_read_error_yields_to_the_graceful_stop_domain() {
        // Identity safety remediation §4, stop branch: a graceful stop
        // that lands WHILE the alignment read is failing must classify as
        // `Cancelled` — never as a false "unreadable" content verdict.
        // Nothing publishes, no scratch is claimed, the original stays
        // safely recoverable, and no second press is needed.
        let _fault_serialization_guard = test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _hook_guard = arm_alignment_read_failure(1_000);
        let storage = Storage::new();
        let (_, original_path) = seed_original(&storage);

        let stop = StopFlag::new();
        let stop_for_worker = stop.clone();
        let layout = storage.layout.clone();
        let camera = storage.camera.clone();
        let worker = std::thread::spawn(move || {
            let interrupt = InterruptHandle::new();
            let lease = CameraLease::try_acquire(&layout, &camera).unwrap();
            recover_camera_partials_with_interrupt(
                &layout,
                &camera,
                &lease,
                &interrupt,
                Some(&stop_for_worker),
            )
        });

        // The stop lands INSIDE the failing read's hold window (1 s).
        std::thread::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        stop.request();

        let (outcomes, failures) = worker.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the graceful stop must end the failing read promptly"
        );
        assert!(
            failures
                .iter()
                .all(|failure| matches!(failure.error, RecoveryError::Cancelled)),
            "a stop racing the read error is a cancellation, never a verdict: {failures:?}"
        );
        assert!(
            outcomes.iter().all(|outcome| matches!(
                outcome,
                RecoveryOutcome::NothingToDo | RecoveryOutcome::KeptUnrecoverable { .. }
            )) || outcomes.is_empty(),
            "nothing may publish after the stop: {outcomes:?}"
        );
        assert!(original_path.is_file(), "the original must survive");
        assert!(HOOK_SCRATCHES.lock().unwrap().is_empty());
        assert_eq!(count_recordings(&storage.day_dir), 0);
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
            let lease = CameraLease::try_acquire(&layout, &camera).unwrap();
            recover_camera_partials_with_interrupt(
                &layout,
                &camera,
                &lease,
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
