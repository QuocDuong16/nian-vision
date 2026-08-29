#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use nian_application::StorageManager;
use nian_domain::{CameraId, RetentionPolicy};
use nian_storage::{CameraLease, RecordingsLayout};

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

fn manager(layout: RecordingsLayout) -> StorageManager {
    StorageManager::open(layout, RetentionPolicy::default(), None).unwrap()
}

#[test]
fn reconciliation_indexes_normal_and_recovered_then_becomes_idempotent() {
    let (_temp, layout, camera) = fixture();
    recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        11,
    );
    recording(
        &layout,
        &camera,
        at("2026-08-29T08:31:00"),
        "08-31-00.recovered.mkv",
        13,
    );

    let mut manager = manager(layout);
    let first = manager.reconcile().unwrap();
    assert_eq!(first.inserted, 2);
    assert_eq!(first.database_mutations(), 2);
    assert_eq!(manager.list_camera(&camera).unwrap().len(), 2);
    assert_eq!(manager.total_indexed_recording_bytes().unwrap(), 24);

    let second = manager.reconcile().unwrap();
    assert_eq!(second.database_mutations(), 0);
}

#[test]
fn reconciliation_updates_changed_size_and_removes_missing_row() {
    let (_temp, layout, camera) = fixture();
    let first_path = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        10,
    );
    let second_path = recording(
        &layout,
        &camera,
        at("2026-08-29T08:31:00"),
        "08-31-00.mkv",
        20,
    );
    let mut manager = manager(layout);
    manager.reconcile().unwrap();

    write_file(&first_path, 15);
    std::fs::remove_file(&second_path).unwrap();
    let report = manager.reconcile().unwrap();
    assert_eq!(report.updated, 1);
    assert_eq!(report.removed_missing, 1);
    let rows = manager.list_camera(&camera).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size_bytes, 15);
}

#[test]
fn partial_is_recovery_pending_when_camera_lease_is_acquirable() {
    let (_temp, layout, camera) = fixture();
    let partial = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.partial.mkv",
        17,
    );
    let day = partial.parent().unwrap();
    std::fs::write(day.join("08-30-00.recovery-1-2-3.tmp"), b"scratch").unwrap();
    std::fs::write(day.join("notes.txt"), b"foreign").unwrap();

    let mut manager = manager(layout);
    let report = manager.reconcile().unwrap();
    assert_eq!(report.recovery_pending_partials, 1);
    assert_eq!(report.active_partials, 0);
    assert!(manager.list_camera(&camera).unwrap().is_empty());
    assert!(partial.exists());
}

#[test]
fn partial_is_active_and_untouched_when_another_owner_holds_camera_lease() {
    let (_temp, layout, camera) = fixture();
    let partial = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.partial.mkv",
        17,
    );
    let lease = CameraLease::try_acquire(&layout, &camera).unwrap();

    let mut manager = manager(layout.clone());
    let report = manager.reconcile().unwrap();
    assert_eq!(report.active_partials, 1);
    assert_eq!(report.recovery_pending_partials, 0);
    assert!(partial.exists());
    drop(lease);
}

#[test]
fn explicit_rebuild_restores_index_after_database_deletion() {
    let (_temp, layout, camera) = fixture();
    let media = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        99,
    );
    {
        let mut manager = manager(layout.clone());
        manager.reconcile().unwrap();
    }
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!(
            "{}{suffix}",
            layout.recording_index_path().to_string_lossy()
        ));
        let _ = std::fs::remove_file(path);
    }

    let mut manager = manager(layout);
    assert_eq!(manager.rebuild().unwrap(), 1);
    let rows = manager.list_camera(&camera).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].media_duration_ms, None);
    assert!(media.exists());
}

#[test]
fn corrupt_database_is_quarantined_and_rebuilt_without_touching_media() {
    let (_temp, layout, camera) = fixture();
    let media = recording(
        &layout,
        &camera,
        at("2026-08-29T08:30:00"),
        "08-30-00.mkv",
        77,
    );
    layout.ensure_control_dir().unwrap();
    std::fs::write(layout.recording_index_path(), b"not sqlite at all").unwrap();

    let manager = manager(layout.clone());
    assert!(manager.was_rebuilt_after_corruption());
    assert_eq!(manager.list_camera(&camera).unwrap().len(), 1);
    assert_eq!(std::fs::metadata(&media).unwrap().len(), 77);
    assert!(
        layout
            .control_dir()
            .join("recordings.sqlite3.corrupt-1")
            .exists()
    );
}

#[test]
fn incremental_upsert_records_trusted_duration_without_owning_publication() {
    let (_temp, layout, camera) = fixture();
    let started_at = at("2026-08-29T08:30:00");
    let media = recording(&layout, &camera, started_at, "08-30-00.mkv", 123);
    let mut manager = manager(layout);

    assert!(
        manager
            .upsert_finalized(&camera, &media, started_at, 123, Some(9_876))
            .unwrap()
    );
    let rows = manager.list_camera(&camera).unwrap();
    assert_eq!(rows[0].media_duration_ms, Some(9_876));
    assert!(media.exists());
}
