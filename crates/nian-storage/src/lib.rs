//! Nian Vision storage layer.
//!
//! Currently owns the on-disk recordings layout (master spec §10):
//!
//! ```text
//! <storage_root>/<camera-id>/<year>/<month>/<day>/HH-MM-SS.mkv
//! ```
//!
//! All names are derived from validated [`CameraId`] values and formatted
//! timestamps — user-provided strings never reach a path directly. The crate
//! forbids `unsafe` and performs no I/O beyond what future milestones add.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod paths;

pub use error::StorageError;
pub use paths::{RecordingsLayout, SEGMENT_EXTENSION, SEGMENT_PARTIAL_SUFFIX};
