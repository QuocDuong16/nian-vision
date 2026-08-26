//! Nian Vision storage layer.
//!
//! Currently owns the on-disk recordings layout (master spec §10):
//!
//! ```text
//! <storage_root>/<camera-id>/<year>/<month>/<day>/HH-MM-SS.mkv
//! ```
//!
//! All names are derived from validated [`CameraId`] values and formatted
//! timestamps — user-provided strings never reach a path directly.
//!
//! `unsafe` policy: this crate contains exactly one sanctioned unsafe block,
//! the Windows no-replace publication primitive (`MoveFileExW`, see
//! `paths::publish_no_replace`); it is gated to `cfg(windows)` and each call
//! site documents its invariants. Every other platform builds with
//! `#![forbid(unsafe_code)]`.

#![cfg_attr(not(windows), forbid(unsafe_code))]
#![deny(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod paths;

pub use error::StorageError;
pub use paths::{RecordingsLayout, SEGMENT_EXTENSION, SEGMENT_PARTIAL_SUFFIX};
