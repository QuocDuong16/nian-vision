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

pub mod classification;
pub mod error;
pub mod inventory;
pub mod lease;
pub mod paths;
pub mod recovery;
#[cfg(any(test, feature = "test-hooks"))]
pub mod test_hooks;
pub mod transaction;

pub use classification::{
    RecordingFileKind, classify_recording_file, classify_recording_file_name,
};
pub use error::StorageError;
pub use inventory::{
    FilesystemInventory, InventoryArtifact, InventoryPartial, InventoryRecording,
    inventory_recordings,
};
pub use lease::{CAMERA_LEASE_FILE_NAME, CameraLease};
pub use paths::{
    RecordingsLayout, SEGMENT_EXTENSION, SEGMENT_PARTIAL_SUFFIX, filesystem_identity_datetime,
};
pub use recovery::{PartialDisposition, PartialFile, scan_camera_partials};
pub use transaction::{
    PathPresence, RECOVERY_TOMBSTONE_MAGIC, RecoveredRetentionState, RecoveryTombstone,
    RecoveryTransactionPaths, inspect_path_presence, inspect_recovered_retention,
    parse_recovery_tombstone, published_final_matches, recovery_tombstone_matches,
    recovery_tombstone_payload, recovery_transaction_paths,
};
