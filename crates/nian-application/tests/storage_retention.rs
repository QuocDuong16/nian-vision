#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use nian_application::{StorageManager, StorageManagerError};
use nian_domain::{CameraId, RetentionPolicy, StorageQuota};
use nian_storage::{CameraLease, RecordingsLayout, recovery_tombstone_payload};

const MIB: u64 = 1024 * 1024;
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

fn at(value: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(value, TIME_FORMAT).unwrap()
}

fn fixture() -> (tempfile::TempDir, RecordingsLayout, CameraId) {
    let temp = tempfile::tempdir().unwrap();
    let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
    let camera = CameraId::parse("cam-a").unwrap();
    (temp, layout, camera)
}

fn write_file(path: &Path, size: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_len(size).unwrap();
}

fn recording(
    layout: &RecordingsLayout,
    camera: &CameraId,
    started_at: NaiveDateTime,
    filename: &str,
    size: u64,
) -> PathBuf {
    let path = layout.day_dir(camera, started_at.date()).join(filename);
    write_file(&path, size);
    path
}

fn age_manager(layout: RecordingsLayout, days: u32) -> StorageManager {
    StorageManager::open(
        layout,
        RetentionPolicy {
            max_age_days: Some(days),
            max_storage_bytes: None,
        },
        None,
    )
    .unwrap()
}

fn quota_manager(layout: RecordingsLayout, high: u64, low: u64) -> StorageManager {
    StorageManager::open(
        layout,
        RetentionPolicy {
            max_age_days: None,
            max_storage_bytes: Some(high),
        },
        Some(StorageQuota {
            max_bytes: high,
            cleanup_target_bytes: low,
        }),
    )
    .unwrap()
}

#[test]
fn retention_requires_reconciliation_before_deletion() {
    let (_temp, layout, _camera) = fixture();
    let mut manager = age_manager(layout, 1);
    assert!(matches!(
        manager.run_retention(at("2026-08-29T12:00:00")),
        Err(StorageManagerError::NotReconciled)
    ));
}

#[test]
fn age_only_deletes_old_finalized_recording_but_never_partial() {
    let (_temp, layout, camera) = fixture();
    let old = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        10,
    );
    let recent = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        20,
    );
    let partial = recording(
        &layout,
        &camera,
        at("2026-08-20T08:31:00"),
        "08-31-00.partial.mkv",
        30,
    );

    let mut manager = age_manager(layout, 7);
    manager.reconcile().unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.age_deleted, 1);
    assert!(!old.exists());
    assert!(recent.exists());
    assert!(partial.exists());
}

#[test]
fn quota_below_high_watermark_does_not_delete() {
    let (_temp, layout, camera) = fixture();
    let a = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        MIB,
    );
    let b = recording(
        &layout,
        &camera,
        at("2026-08-21T08:30:00"),
        "08-30-00.mkv",
        MIB,
    );
    let mut manager = quota_manager(layout, 4 * MIB, 2 * MIB);
    manager.reconcile().unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(a.exists() && b.exists());
}

#[test]
fn quota_crossing_deletes_oldest_until_low_watermark() {
    let (_temp, layout, camera) = fixture();
    let oldest = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        2 * MIB,
    );
    let middle = recording(
        &layout,
        &camera,
        at("2026-08-21T08:30:00"),
        "08-30-00.mkv",
        2 * MIB,
    );
    let newest = recording(
        &layout,
        &camera,
        at("2026-08-22T08:30:00"),
        "08-30-00.mkv",
        2 * MIB,
    );
    let mut manager = quota_manager(layout, 4 * MIB, 2 * MIB);
    manager.reconcile().unwrap();

    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 2);
    assert_eq!(report.quota_deleted, 2);
    assert!(!oldest.exists());
    assert!(!middle.exists());
    assert!(newest.exists());
}

#[test]
fn age_and_quota_use_or_semantics() {
    let (_temp, layout, camera) = fixture();
    let old = recording(
        &layout,
        &camera,
        at("2026-08-01T08:30:00"),
        "08-30-00.mkv",
        MIB,
    );
    let recent_a = recording(
        &layout,
        &camera,
        at("2026-08-28T08:30:00"),
        "08-30-00.mkv",
        2 * MIB,
    );
    let recent_b = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        2 * MIB,
    );
    let mut manager = StorageManager::open(
        layout,
        RetentionPolicy {
            max_age_days: Some(7),
            max_storage_bytes: Some(4 * MIB),
        },
        Some(StorageQuota {
            max_bytes: 4 * MIB,
            cleanup_target_bytes: 2 * MIB,
        }),
    )
    .unwrap();
    manager.reconcile().unwrap();

    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 2);
    assert!(!old.exists());
    assert!(!recent_a.exists());
    assert!(recent_b.exists());
    assert!(report.age_deleted >= 1);
    assert!(report.quota_deleted >= 1);
}

#[test]
fn active_camera_lease_does_not_block_old_normal_final_retention() {
    let (_temp, layout, camera) = fixture();
    let old = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        10,
    );
    let mut manager = age_manager(layout.clone(), 1);
    manager.reconcile().unwrap();
    let lease = CameraLease::try_acquire(&layout, &camera).unwrap();

    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!old.exists());
    drop(lease);
}

#[test]
fn recovered_recordings_count_toward_recording_usage() {
    let (_temp, layout, camera) = fixture();
    recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        10,
    );
    recording(
        &layout,
        &camera,
        at("2026-08-20T08:31:00"),
        "08-31-00.recovered.mkv",
        20,
    );
    let mut manager = StorageManager::open(layout, RetentionPolicy::default(), None).unwrap();
    manager.reconcile().unwrap();
    assert_eq!(manager.total_indexed_recording_bytes().unwrap(), 30);
}

#[test]
fn unresolved_recovered_transaction_is_preserved_to_prevent_resurrection() {
    let (_temp, layout, camera) = fixture();
    let recovered = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.recovered.mkv",
        10,
    );
    let original = recovered.parent().unwrap().join("08-30-00.partial.mkv");
    std::fs::write(&original, b"still here").unwrap();

    let mut manager = age_manager(layout, 1);
    manager.reconcile().unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(report.blocked_recovery_transactions, 1);
    assert!(recovered.exists());
    assert!(original.exists());
}

#[test]
fn settled_recovered_transaction_deletes_final_tombstone_then_index_row() {
    let (_temp, layout, camera) = fixture();
    let recovered = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.recovered.mkv",
        10,
    );
    let tombstone = recovered.with_file_name("08-30-00.recovered.mkv.done");
    std::fs::write(
        &tombstone,
        recovery_tombstone_payload("08-30-00.partial.mkv", "08-30-00.recovered.mkv", 10),
    )
    .unwrap();

    let mut manager = age_manager(layout, 1);
    manager.reconcile().unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!recovered.exists());
    assert!(!tombstone.exists());
    assert!(manager.list_camera(&camera).unwrap().is_empty());
}

#[test]
fn filesystem_first_crash_boundary_converges_on_reconciliation() {
    let (_temp, layout, camera) = fixture();
    let media = recording(
        &layout,
        &camera,
        at("2026-08-20T08:30:00"),
        "08-30-00.mkv",
        10,
    );
    let mut manager = StorageManager::open(layout, RetentionPolicy::default(), None).unwrap();
    manager.reconcile().unwrap();
    std::fs::remove_file(&media).unwrap();

    let report = manager.reconcile().unwrap();
    assert_eq!(report.removed_missing, 1);
    assert!(manager.list_camera(&camera).unwrap().is_empty());
}

#[test]
fn stale_scratch_cleanup_requires_camera_lease_and_is_not_retention_accounting() {
    let (_temp, layout, camera) = fixture();
    let day = layout.day_dir(&camera, NaiveDate::from_ymd_opt(2026, 8, 29).unwrap());
    std::fs::create_dir_all(&day).unwrap();
    let scratch = day.join("08-30-00.recovery-1-2-3.tmp");
    std::fs::write(&scratch, b"scratch").unwrap();
    let manager = StorageManager::open(layout.clone(), RetentionPolicy::default(), None).unwrap();

    let lease = CameraLease::try_acquire(&layout, &camera).unwrap();
    let blocked = manager.cleanup_stale_recovery_artifacts().unwrap();
    assert_eq!(blocked.skipped_owned_elsewhere, 1);
    assert!(scratch.exists());
    drop(lease);

    let cleaned = manager.cleanup_stale_recovery_artifacts().unwrap();
    assert_eq!(cleaned.removed_scratch, 1);
    assert!(!scratch.exists());
}

#[test]
fn quota_policy_requires_explicit_matching_high_and_low_watermarks() {
    let (_temp, layout, _camera) = fixture();
    let error = StorageManager::open(
        layout.clone(),
        RetentionPolicy {
            max_age_days: None,
            max_storage_bytes: Some(4 * MIB),
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(error, StorageManagerError::Policy(_)));

    let error = StorageManager::open(
        layout,
        RetentionPolicy {
            max_age_days: None,
            max_storage_bytes: Some(4 * MIB),
        },
        Some(StorageQuota {
            max_bytes: 5 * MIB,
            cleanup_target_bytes: 2 * MIB,
        }),
    )
    .unwrap_err();
    assert!(matches!(error, StorageManagerError::Policy(_)));
}
