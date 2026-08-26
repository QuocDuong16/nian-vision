//! FFmpeg-backed media implementation.
//!
//! This is the only application crate allowed to contain `unsafe`. Its public
//! API is safe; every unsafe block documents its invariants. Raw FFI lives in
//! `nian-ffmpeg-sys` and never leaks past this boundary.
//!
//! Ownership model:
//!
//! * `MediaInput` owns an `AVFormatContext` + scratch `AVPacket` and frees
//!   them in `Drop` via `avformat_close_input` / `av_packet_free`;
//! * `MatroskaMuxer` owns an output `AVFormatContext` (+ `AVIOContext`) and
//!   frees it in `Drop` via `avio_closep` / `avformat_free_context`;
//! * interrupt callbacks receive a `Arc::as_ptr` to an `InterruptState` that
//!   the owning wrapper keeps alive for the entire context lifetime;
//! * contexts are not thread-safe: neither wrapper is `Send`/`Sync`
//!   (enforced by the raw pointer fields), matching the one-thread-per-worker
//!   design.

// Unit tests assert with panicking macros by design (integration tests
// declare the same exemption locally).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backend;
mod error_util;
mod input;
mod interrupt;
mod logging;
mod muxer;
mod version;

pub use backend::FfmpegBackend;
pub use input::MediaInput;
pub use interrupt::InterruptHandle;
pub use logging::native_logging_quiet;
pub use muxer::MatroskaMuxer;
pub use version::{RuntimeVersions, runtime_versions};
