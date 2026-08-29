//! Nian Vision application layer.
//!
//! Sits between the UI/Tauri host and the infrastructure crates (storage,
//! media). Owns configuration validation and cross-cutting policies; must not
//! depend on FFmpeg or any concrete media implementation.
//!
//! The crate forbids `unsafe`.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod error;
pub mod storage_manager;
pub mod supervisor;

pub use config::{AppConfig, SegmentTargetDuration};
pub use error::ApplicationError;
pub use storage_manager::{
    ArtifactCleanupReport, ReconciliationFailure, ReconciliationFailureKind, ReconciliationReport,
    RetentionFailure, RetentionFailureKind, RetentionReport, StorageManager, StorageManagerError,
};
pub use supervisor::{
    BinaryLauncher, DesiredRecording, JobTerminal, SupervisorDeadlines, WorkerEnd, WorkerLauncher,
    WorkerSupervisor,
};
