#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use chrono::NaiveDateTime;
use nian_application::{RetentionFailureKind, StorageManager};
use nian_domain::{CameraId, RetentionPolicy};
use nian_storage::RecordingsLayout;

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

fn recording(
    layout: &RecordingsLayout,
    camera: &CameraId,
    started_at: NaiveDateTime,
    filename: &str,
) -> PathBuf {
    let path = layout.day_dir(camera, started_at.date()).join(filename);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"footage").unwrap();
    path
}

fn age_manager(layout: RecordingsLayout) -> StorageManager {
    StorageManager::open(
        layout,
        RetentionPolicy {
            max_age_days: Some(1),
            max_storage_bytes: None,
        },
        None,
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn filesystem_deletion_failure_keeps_index_row_and_media() {
    use std::os::unix::fs::PermissionsExt;

    let (_temp, layout, camera) = fixture();
    let media = recording(&layout, &camera, at("2026-08-20T08:30:00"), "08-30-00.mkv");
    let day = media.parent().unwrap().to_path_buf();
    let mut manager = age_manager(layout);
    manager.reconcile().unwrap();

    let original_mode = std::fs::metadata(&day).unwrap().permissions().mode();
    std::fs::set_permissions(&day, std::fs::Permissions::from_mode(0o500)).unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    std::fs::set_permissions(&day, std::fs::Permissions::from_mode(original_mode)).unwrap();

    assert_eq!(report.deleted, 0);
    assert!(!report.failed.is_empty());
    assert!(media.exists());
    assert_eq!(manager.list_camera(&camera).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn recording_looking_symlink_is_not_counted_or_deleted() {
    use std::os::unix::fs::symlink;

    let (temp, layout, camera) = fixture();
    let day = layout.day_dir(&camera, at("2026-08-20T08:30:00").date());
    std::fs::create_dir_all(&day).unwrap();
    let outside = temp.path().join("outside.mkv");
    std::fs::write(&outside, b"foreign target").unwrap();
    let fake = day.join("08-30-00.mkv");
    symlink(&outside, &fake).unwrap();

    let mut manager = age_manager(layout);
    manager.reconcile().unwrap();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(std::fs::read(&outside).unwrap(), b"foreign target");
    assert!(fake.symlink_metadata().unwrap().file_type().is_symlink());
}

#[test]
fn index_delete_failure_after_media_delete_converges_on_next_reconciliation() {
    let (_temp, layout, camera) = fixture();
    let media = recording(&layout, &camera, at("2026-08-20T08:30:00"), "08-30-00.mkv");
    let mut manager = age_manager(layout);
    manager.reconcile().unwrap();

    nian_index::test_hooks::fail_next_remove();
    let report = manager.run_retention(at("2026-08-29T12:00:00")).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!media.exists());
    assert!(
        report
            .failed
            .iter()
            .any(|failure| failure.kind == RetentionFailureKind::IndexDelete)
    );
    assert_eq!(manager.list_camera(&camera).unwrap().len(), 1);

    let reconciliation = manager.reconcile().unwrap();
    assert_eq!(reconciliation.removed_missing, 1);
    assert!(manager.list_camera(&camera).unwrap().is_empty());
}
