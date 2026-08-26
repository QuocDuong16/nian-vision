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
    pub(crate) fn should_abort(&self) -> bool {
        if self.cancelled.load(Ordering::Relaxed) {
            return true;
        }
        match self.deadline.try_lock() {
            Ok(guard) => guard.is_some_and(|deadline| Instant::now() >= deadline),
            // A poisoned/contended mutex must never wedge the media loop;
            // treat as "no deadline".
            Err(_) => false,
        }
    }
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
}

impl std::fmt::Debug for InterruptHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
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
