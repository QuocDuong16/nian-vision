//! Cancellation and deadline support for blocking FFmpeg operations.
//!
//! FFmpeg network calls block inside C; the only supported way to abort them
//! is the `AVIOInterruptCB` installed on the `AVFormatContext`. The callback
//! must return non-zero to abort, after which the blocking call fails with an
//! error code.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nian_ffmpeg_sys as sys;

/// Shared cancellation state behind an [`InterruptHandle`].
#[derive(Default)]
pub(crate) struct InterruptState {
    cancelled: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl InterruptState {
    /// What currently demands abortion of a blocking operation.
    ///
    /// Cancellation takes precedence over deadline expiry: when both hold
    /// (e.g. the operator pressed Ctrl+C just as the read deadline lapsed),
    /// the operator's intent wins and the failure is classified as
    /// cancellation, never as a retryable timeout.
    pub(crate) fn abort_cause(&self) -> AbortCause {
        if self.cancelled.load(Ordering::Relaxed) {
            return AbortCause::Cancelled;
        }
        match self.deadline.try_lock() {
            Ok(guard) if guard.is_some_and(|deadline| Instant::now() >= deadline) => {
                AbortCause::DeadlineExceeded
            }
            // A poisoned/contended mutex must never wedge the media loop;
            // treat as "no deadline".
            _ => AbortCause::None,
        }
    }

    pub(crate) fn should_abort(&self) -> bool {
        !matches!(self.abort_cause(), AbortCause::None)
    }
}

/// Why an interrupt state demands abortion of a blocking operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbortCause {
    /// Nothing demands abortion.
    None,
    /// Explicit cancellation was requested (operator intent).
    Cancelled,
    /// The installed operation deadline has passed.
    DeadlineExceeded,
}

/// Handle used to cancel or time-box in-flight media operations.
///
/// Clone it before handing ownership of the underlying operation to another
/// thread; cancellation is observed at the next interrupt-callback check
/// inside FFmpeg.
#[derive(Clone, Default)]
pub struct InterruptHandle {
    state: Arc<InterruptState>,
}

impl InterruptHandle {
    /// Creates a fresh handle that neither cancelled nor has a deadline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation of the associated operation(s).
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Relaxed);
    }

    /// Sets a deadline `timeout` from now; blocking calls abort once it
    /// passes.
    pub fn set_deadline_from_now(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        if let Ok(mut guard) = self.state.deadline.lock() {
            *guard = Some(deadline);
        }
    }

    /// Clears any previously set deadline.
    pub fn clear_deadline(&self) {
        if let Ok(mut guard) = self.state.deadline.lock() {
            *guard = None;
        }
    }

    /// Whether cancellation has been requested (deadlines excluded).
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn state(&self) -> &Arc<InterruptState> {
        &self.state
    }

    /// Installs this handle's state as the interrupt callback on `context`.
    ///
    /// # SAFETY (caller obligations)
    /// * `context` must be a valid, allocated `AVFormatContext`;
    /// * the `Arc<InterruptState>` behind this handle must outlive every use
    ///   of `context` — the owning wrappers guarantee this by holding the
    ///   `Arc` until after `avformat_close_input`/`avformat_free_context`.
    pub(crate) fn install_on(&self, context: *mut sys::AVFormatContext) {
        let opaque = Arc::as_ptr(&self.state) as *mut c_void;
        // SAFETY: the caller contract guarantees `context` points to a valid
        // allocated context; the callback struct is plain data copied by the
        // assignment.
        unsafe {
            (*context).interrupt_callback = sys::AVIOInterruptCB {
                callback: Some(interrupt_trampoline),
                opaque,
            };
        }
    }

    /// Arms a [`ScopedDeadline`] guard for one blocking operation.
    pub fn scoped_deadline(&self, timeout: Duration) -> ScopedDeadline<'_> {
        ScopedDeadline::new(self, timeout)
    }

    /// The currently installed deadline, if any. Used to detect that a
    /// caller already armed a deadline, in which case operation defaults
    /// must not override the caller's (shorter or longer) budget.
    pub(crate) fn installed_deadline(&self) -> Option<Instant> {
        self.state.deadline.lock().ok().and_then(|guard| *guard)
    }
}

impl std::fmt::Debug for InterruptHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// RAII guard that installs a deadline on an [`InterruptHandle`] for the
/// duration of ONE blocking operation and clears it on drop — including
/// error and panic paths.
///
/// This makes the M3 deadline invariant mechanical: `set_deadline_from_now`
/// without a matching clear would leave an expired deadline installed while
/// subsequent *unrelated* blocking operations (local mux writes, trailer
/// finalization) run, aborting them spuriously. With this guard the only way
/// to keep a deadline is to hold it deliberately:
///
/// ```text
/// let _guard = handle.scoped_deadline(READ_TIMEOUT);  // armed
/// input.next_packet()                                  // bounded
/// // `_guard` dropped here → deadline cleared, mux writes run unbounded
/// ```
///
/// Nesting is supported in LIFO order (an inner guard restores the outer
/// deadline it replaced on drop).
pub struct ScopedDeadline<'a> {
    handle: &'a InterruptHandle,
    /// Deadline this guard is responsible for clearing; `None` after Drop.
    restored: Option<Instant>,
    done: bool,
}

impl ScopedDeadline<'_> {
    /// Arms `timeout` from now on `handle`, remembering any outer deadline
    /// it displaces so [`Drop`] can restore it.
    pub fn new(handle: &InterruptHandle, timeout: Duration) -> ScopedDeadline<'_> {
        let restored = handle.installed_deadline();
        handle.set_deadline_from_now(timeout);
        ScopedDeadline {
            handle,
            restored,
            done: false,
        }
    }
}

impl Drop for ScopedDeadline<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        // Clear whatever is installed, then restore the displaced outer
        // deadline (if any) — plain clear() would forget the outer scope's
        // remaining time under nesting.
        self.handle.clear_deadline();
        if let Some(deadline) = self.restored.take()
            && let Ok(mut guard) = self.handle.state.deadline.lock()
        {
            *guard = Some(deadline);
        }
    }
}

/// C trampoline invoked by FFmpeg during blocking operations.
///
/// # SAFETY
/// * `opaque` is the `Arc::as_ptr` value installed by [`InterruptHandle::install_on`];
/// * the installing wrapper keeps a strong `Arc` reference alive for the
///   whole lifetime of the associated `AVFormatContext`, and drops the
///   context before dropping the `Arc`, so the reference is always valid
///   when FFmpeg invokes the callback;
/// * the callback only touches atomics/mutexes and must stay exception-free
///   (it cannot unwind into C; `should_abort` never panics).
pub unsafe extern "C" fn interrupt_trampoline(opaque: *mut c_void) -> c_int {
    // SAFETY: see function-level contract above.
    let state = unsafe { &*(opaque as *const InterruptState) };
    c_int::from(state.should_abort())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn fresh_state_aborts_for_no_reason() {
        let handle = InterruptHandle::new();
        assert_eq!(handle.state().abort_cause(), AbortCause::None);
        assert!(!handle.state().should_abort());
    }

    #[test]
    fn cancellation_is_reported_as_cancelled() {
        let handle = InterruptHandle::new();
        handle.cancel();
        assert_eq!(handle.state().abort_cause(), AbortCause::Cancelled);
        assert!(handle.is_cancelled());
    }

    #[test]
    fn expired_deadline_is_reported_without_cancellation() {
        let handle = InterruptHandle::new();
        // A zero-length deadline is already in the past the moment the
        // state is consulted (Instant::now() >= Instant::now()).
        handle.set_deadline_from_now(Duration::ZERO);
        assert_eq!(handle.state().abort_cause(), AbortCause::DeadlineExceeded);
        // Deadlines are not cancellations.
        assert!(!handle.is_cancelled());
    }

    #[test]
    fn cancellation_takes_precedence_over_deadline() {
        let handle = InterruptHandle::new();
        handle.set_deadline_from_now(Duration::ZERO);
        handle.cancel();
        assert_eq!(handle.state().abort_cause(), AbortCause::Cancelled);
    }

    #[test]
    fn future_deadline_does_not_abort() {
        let handle = InterruptHandle::new();
        handle.set_deadline_from_now(Duration::from_secs(60));
        assert_eq!(handle.state().abort_cause(), AbortCause::None);
        sleep(Duration::from_millis(2));
        assert!(!handle.state().should_abort());
    }

    #[test]
    fn scoped_deadline_is_cleared_on_drop_including_early_return_paths() {
        let handle = InterruptHandle::new();

        fn bounded_read(handle: &InterruptHandle) -> Result<(), ()> {
            let _guard = handle.scoped_deadline(Duration::from_secs(60));
            assert!(handle.installed_deadline().is_some());
            // Early return: the guard must still clear the deadline.
            Err(())
        }
        let _ = bounded_read(&handle);

        assert!(
            handle.installed_deadline().is_none(),
            "deadline leaked past its scope"
        );
        assert_eq!(handle.state().abort_cause(), AbortCause::None);
    }

    #[test]
    fn scoped_deadline_nesting_restores_the_outer_deadline() {
        let handle = InterruptHandle::new();
        let outer_instant = {
            let _outer = handle.scoped_deadline(Duration::from_secs(60));
            let outer_installed = handle.installed_deadline();
            assert!(outer_installed.is_some());
            {
                let _inner = handle.scoped_deadline(Duration::from_secs(120));
                assert!(handle.installed_deadline().is_some());
            }
            // Inner guard dropped: the OUTER deadline is restored, not lost.
            assert_eq!(handle.installed_deadline(), outer_installed);
            outer_installed
        };
        // Outer guard dropped: everything cleared.
        assert!(handle.installed_deadline().is_none());
        let _ = outer_instant;
    }

    #[test]
    fn scoped_deadline_expiry_is_a_timeout_not_a_cancellation() {
        let handle = InterruptHandle::new();
        {
            let _guard = handle.scoped_deadline(Duration::ZERO);
            assert_eq!(handle.state().abort_cause(), AbortCause::DeadlineExceeded);
            assert!(!handle.is_cancelled(), "deadline must not look cancelled");
        }
        assert_eq!(handle.state().abort_cause(), AbortCause::None);
    }
}
