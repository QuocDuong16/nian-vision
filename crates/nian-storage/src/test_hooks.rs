//! Deterministic test/seam hooks for the storage claim pipeline (atomic
//! identity claim remediation §5). Inert unless armed; compiled out of
//! production builds unless the `test-hooks` cargo feature is enabled
//! (which only test targets do, via dev-dependencies).
//!
//! The gate is KEYED TO ONE DAY DIRECTORY: only a `claim_segment` call for
//! that exact directory parks, so concurrent tests using their own temp
//! trees are never affected and no global serialization is required.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// One armed gate: parks the FIRST `claim_segment` into `day_dir` after its
/// allocation scan chose a sequence but BEFORE it creates the candidate
/// partial — the deterministic stand-in for a stale non-atomic directory
/// snapshot (a competing namespace transition lands inside that window).
pub struct ClaimGateArm {
    pub day_dir: PathBuf,
    pub gate: Arc<ClaimIdentityGate>,
}

pub static CLAIM_IDENTITY_GATE: Mutex<Option<ClaimGateArm>> = Mutex::new(None);

/// Serializes arming/disarming against other hook-aware tests in the same
/// process (hooks are day-dir keyed, so unrelated claims stay lock-free).
pub static FAULT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct SqliteQuarantineInterruption {
    main: PathBuf,
    moves_remaining: usize,
}

static SQLITE_QUARANTINE_INTERRUPTION: Mutex<Option<SqliteQuarantineInterruption>> =
    Mutex::new(None);

/// Arms a one-shot interruption after exactly `moves` successful SQLite-family
/// member moves for `main`.
pub fn arm_sqlite_quarantine_interruption(main: &Path, moves: usize) -> SqliteQuarantineGuard {
    assert!(
        moves != 0,
        "SQLite quarantine interruption requires at least one move"
    );
    *SQLITE_QUARANTINE_INTERRUPTION
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(SqliteQuarantineInterruption {
        main: main.to_path_buf(),
        moves_remaining: moves,
    });
    SqliteQuarantineGuard
}

pub(crate) fn sqlite_quarantine_interrupt_after_move(main: &Path) -> bool {
    let mut armed = SQLITE_QUARANTINE_INTERRUPTION
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(state) = armed.as_mut().filter(|state| state.main == main) else {
        return false;
    };
    state.moves_remaining = state.moves_remaining.saturating_sub(1);
    if state.moves_remaining != 0 {
        return false;
    }
    *armed = None;
    true
}

pub struct SqliteQuarantineGuard;

impl Drop for SqliteQuarantineGuard {
    fn drop(&mut self) {
        *SQLITE_QUARANTINE_INTERRUPTION
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// One-shot arrival/release gate parked inside `claim_segment`.
pub struct ClaimIdentityGate {
    arrived: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
}

impl ClaimIdentityGate {
    fn new() -> Self {
        Self {
            arrived: (Mutex::new(false), Condvar::new()),
            released: (Mutex::new(false), Condvar::new()),
        }
    }

    /// Blocks until the claim has parked at the gate (test-side wait).
    pub fn wait_arrived(&self) {
        let mut arrived = self
            .arrived
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !*arrived {
            arrived = self
                .arrived
                .1
                .wait(arrived)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Parks the claim (pipeline-side): signals arrival, then blocks until
    /// the test releases.
    fn park(&self) {
        {
            let mut arrived = self
                .arrived
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *arrived = true;
        }
        self.arrived.1.notify_all();
        let mut released = self
            .released
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !*released {
            released = self
                .released
                .1
                .wait(released)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Releases the parked claim (test-side).
    pub fn release(&self) {
        {
            let mut released = self
                .released
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *released = true;
        }
        self.released.1.notify_all();
    }
}

/// Arms a one-shot gate for `day_dir`; the first `claim_segment` into that
/// directory parks after its allocation scan. Returns a panic-safe guard
/// plus the gate handle (`wait_arrived` / `release`).
pub fn arm_claim_identity_gate(day_dir: &Path) -> (Guard, Arc<ClaimIdentityGate>) {
    let gate = Arc::new(ClaimIdentityGate::new());
    {
        let mut armed = CLAIM_IDENTITY_GATE
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *armed = Some(ClaimGateArm {
            day_dir: day_dir.to_path_buf(),
            gate: Arc::clone(&gate),
        });
    }
    (Guard, gate)
}

/// Pipeline-side seam: consume-and-park if THIS claim's day directory is
/// the armed one. One-shot: the armed gate is consumed by the first
/// matching claim, so later claim attempts (retries in the same loop,
/// competing claims in the test's own scenario) proceed unimpeded.
pub fn claim_identity_gate_wait(day_dir: &Path) {
    let mut armed: MutexGuard<'_, Option<ClaimGateArm>> = CLAIM_IDENTITY_GATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(arm) = armed.take_if(|arm| arm.day_dir == day_dir) else {
        return;
    };
    drop(armed);
    arm.gate.park();
}

/// Armed one-shot day-directory loss (sub-second identity remediation §4):
/// the next `claim_segment` into `day_dir` has its directory REMOVED after
/// the candidate partial is created but BEFORE the post-claim identity fence
/// validates it. This is a real-world "storage vanished mid-claim" fault:
/// both the fence enumeration and the losing-candidate cleanup fail with
/// genuine OS errors (ENOENT) — no injected error values, so the typed
/// dual-context error is exercised end-to-end.
pub static CLAIM_DAY_DIR_LOSS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Arms a one-shot day-directory loss for `day_dir`. Panic-safe via the
/// returned guard, which disarms on drop.
pub fn arm_claim_day_dir_loss(day_dir: &Path) -> Guard {
    *CLAIM_DAY_DIR_LOSS
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(day_dir.to_path_buf());
    Guard
}

/// Pipeline-side seam: consume-and-fire if THIS claim's day directory is the
/// armed one. One-shot, keyed like the claim gate so unrelated claims stay
/// unaffected.
pub fn claim_day_dir_loss_fire(day_dir: &Path) {
    let mut armed = CLAIM_DAY_DIR_LOSS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(armed_dir) = armed.take_if(|armed_dir| armed_dir.as_path() == day_dir) else {
        return;
    };
    drop(armed);
    let _ = std::fs::remove_dir_all(armed_dir);
}

/// Panic-safe: disarms any hook this guard armed whenever it dies.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        *CLAIM_IDENTITY_GATE
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *CLAIM_DAY_DIR_LOSS
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
}
