//! FFmpeg native logging policy — the secret boundary Rust cannot see.
//!
//! libav* writes diagnostics through its own pipeline straight to stderr;
//! Rust-side redaction (`RtspUrl::redacted`, `MediaError` message hygiene)
//! never observes that traffic. At `AV_LOG_INFO` and above — and verbosely
//! at `DEBUG`/`TRACE` — the RTSP implementation echoes handshake and request
//! details that include the **full connect URL**, credentials included.
//!
//! Production policy: [`install_production_policy`] pins the native level to
//! `AV_LOG_QUIET` immediately after the ABI check succeeds, before any
//! network operation can run. FFmpeg therefore emits no native diagnostics
//! in production processes, which guarantees camera usernames/passwords can
//! never reach journals or terminal scrollback through this channel.
//!
//! # Re-enabling diagnostics safely (future work)
//!
//! Do **not** simply raise the level via `av_log_set_level`: that reopens the
//! credential leak. The supported path is an `av_log_set_callback` trampoline
//! that filters every line (drop credential-bearing patterns, cap severity)
//! before forwarding to the Rust `tracing` subscriber. That callback receives
//! a C `va_list`; stable safe FFI for variadic arguments does not exist, so
//! the trampoline is an `unsafe` feature that needs its own design review
//! before it may ship. Until then QUIET is the only correct default.

use nian_ffmpeg_sys as sys;

/// Pins the process-global FFmpeg native log level to [`sys::AV_LOG_QUIET`].
///
/// Called once by startup init before any media I/O; see the module docs for
/// why this is a security boundary rather than a noise preference.
pub(crate) fn install_production_policy() {
    // SAFETY: av_log_set_level writes one process-global integer guarded by
    // libavutil; it takes no pointers, allocates nothing, and cannot fail.
    unsafe { sys::av_log_set_level(sys::AV_LOG_QUIET) };
}

/// Whether the native FFmpeg log policy is currently the quiet production
/// default.
///
/// Introspection for host diagnostics and regression tests; it reports state,
/// it does not change it.
pub fn native_logging_quiet() -> bool {
    // SAFETY: av_log_get_level only reads the same process-global integer.
    unsafe { sys::av_log_get_level() == sys::AV_LOG_QUIET }
}
