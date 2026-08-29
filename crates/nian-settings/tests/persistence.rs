// Integration tests use panicking assertions/setup helpers deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Credentials,
    Host, RetentionPolicy, StorageQuota,
};
use nian_settings::{ApplicationSettings, SettingsError, SettingsStore};
use rusqlite::Connection;
use tempfile::tempdir;

fn camera(name: &str, credential_ref: &str) -> CameraConfig {
    CameraConfig::new(
        CameraId::parse("front-door").unwrap(),
        name,
        CameraSource::Rtsp(
            CameraEndpoint::new(Host::parse("192.168.1.50").unwrap(), 554, "/stream1").unwrap(),
        ),
        AudioPolicy::CopyAll,
        CredentialRef::parse(credential_ref).unwrap(),
    )
    .unwrap()
}

#[test]
fn fresh_database_creates_schema_v1() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let store = SettingsStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
    assert!(path.exists());
}

#[test]
fn camera_persists_across_reopen_and_display_name_keeps_camera_id() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    {
        let mut store = SettingsStore::open(&path).unwrap();
        store
            .insert_camera(&camera("Front door", "cred-v1"))
            .unwrap();
    }
    {
        let mut store = SettingsStore::open(&path).unwrap();
        let mut saved = store.list_cameras().unwrap().remove(0);
        assert_eq!(saved.camera_id().as_str(), "front-door");
        saved = CameraConfig::new(
            saved.camera_id().clone(),
            "Renamed camera",
            saved.source().clone(),
            saved.audio_policy(),
            saved.credential_ref().clone(),
        )
        .unwrap();
        assert!(store.update_camera(&saved).unwrap());
    }
    let store = SettingsStore::open(&path).unwrap();
    let saved = store.list_cameras().unwrap().remove(0);
    assert_eq!(saved.camera_id().as_str(), "front-door");
    assert_eq!(saved.display_name(), "Renamed camera");
}

#[test]
fn duplicate_camera_id_is_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let mut store = SettingsStore::open(&path).unwrap();
    store
        .insert_camera(&camera("Front door", "cred-v1"))
        .unwrap();
    let error = store
        .insert_camera(&camera("Another name", "cred-v2"))
        .unwrap_err();
    assert!(matches!(error, SettingsError::DuplicateCamera(_)));
}

#[test]
fn future_schema_fails_without_replacing_database() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE evidence(value TEXT); INSERT INTO evidence VALUES ('keep-me'); PRAGMA user_version=99;").unwrap();
    }
    let before = fs::read(&path).unwrap();
    let error = SettingsStore::open(&path).unwrap_err();
    assert!(matches!(
        error,
        SettingsError::FutureSchema {
            found: 99,
            supported: 1
        }
    ));
    let after = fs::read(&path).unwrap();
    assert_eq!(before, after);
}

#[test]
fn failed_migration_does_not_advance_schema_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        // Force schema-v1 migration to fail at CREATE TABLE cameras.
        connection
            .execute_batch("CREATE TABLE cameras(dummy TEXT); PRAGMA user_version=0;")
            .unwrap();
    }
    assert!(SettingsStore::open(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    let version: i32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0);
}

#[test]
fn credential_ref_debug_and_settings_persistence_remain_non_secret() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let sentinel = "UNIQUE_SENTINEL_PASSWORD_DO_NOT_PERSIST";
    let reference = "nian-vision/front-door/00000000-0000-4000-8000-000000000006";
    let credentials = Credentials::new("admin", sentinel);
    let configured = camera("Front door", reference);

    let debug = format!("{:?}", configured.credential_ref());
    assert!(debug.contains(reference));
    assert!(!debug.contains(credentials.password()));

    {
        let mut store = SettingsStore::open(&path).unwrap();
        // Settings accepts only the opaque ref; the Credentials value above has
        // no persistence path through this API.
        store.insert_camera(&configured).unwrap();
        let saved = store.get_camera(configured.camera_id()).unwrap().unwrap();
        assert_eq!(saved.credential_ref().as_str(), reference);
    }
    let bytes = fs::read(&path).unwrap();
    assert!(
        bytes
            .windows(reference.len())
            .any(|window| window == reference.as_bytes())
    );
    assert!(
        !bytes
            .windows(sentinel.len())
            .any(|window| window == sentinel.as_bytes())
    );
}

#[test]
fn deleting_camera_configuration_never_deletes_footage() {
    let dir = tempdir().unwrap();
    let settings_path = dir.path().join("app-data/settings.sqlite3");
    let footage = dir
        .path()
        .join("recordings/front-door/2026/08/29/12-00-00.mkv");
    fs::create_dir_all(footage.parent().unwrap()).unwrap();
    fs::write(&footage, b"historical-footage").unwrap();

    let mut store = SettingsStore::open(&settings_path).unwrap();
    store
        .insert_camera(&camera("Front door", "cred-v1"))
        .unwrap();
    assert!(
        store
            .delete_camera(&CameraId::parse("front-door").unwrap())
            .unwrap()
    );

    assert!(store.list_cameras().unwrap().is_empty());
    assert_eq!(fs::read(&footage).unwrap(), b"historical-footage");
}

#[test]
fn application_quota_round_trips_with_matching_high_and_low_watermarks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let mut store = SettingsStore::open(&path).unwrap();
    let settings = ApplicationSettings {
        storage_root: Some(dir.path().join("recordings")),
        segment_target_secs: 120,
        retention: RetentionPolicy {
            max_age_days: Some(14),
            max_storage_bytes: Some(10 * 1024 * 1024),
        },
        quota: Some(StorageQuota {
            max_bytes: 10 * 1024 * 1024,
            cleanup_target_bytes: 8 * 1024 * 1024,
        }),
    };

    store.save_application_settings(&settings).unwrap();
    assert_eq!(store.application_settings().unwrap(), settings);
}

#[test]
fn mismatched_retention_and_quota_high_watermarks_are_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let mut store = SettingsStore::open(&path).unwrap();
    let settings = ApplicationSettings {
        storage_root: Some(dir.path().join("recordings")),
        segment_target_secs: 120,
        retention: RetentionPolicy {
            max_age_days: None,
            max_storage_bytes: Some(10 * 1024 * 1024),
        },
        quota: Some(StorageQuota {
            max_bytes: 11 * 1024 * 1024,
            cleanup_target_bytes: 8 * 1024 * 1024,
        }),
    };

    assert!(matches!(
        store.save_application_settings(&settings),
        Err(SettingsError::InvalidData(_))
    ));
}

#[test]
fn incomplete_persisted_quota_is_rejected_instead_of_silently_accepted() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    drop(SettingsStore::open(&path).unwrap());

    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE application_settings SET max_storage_bytes=?1, cleanup_target_bytes=NULL WHERE singleton_id=1",
            [10_i64 * 1024 * 1024],
        )
        .unwrap();
    drop(connection);

    let store = SettingsStore::open(&path).unwrap();
    assert!(matches!(
        store.application_settings(),
        Err(SettingsError::InvalidData(_))
    ));
}

#[test]
fn missing_singleton_row_makes_save_fail() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    drop(SettingsStore::open(&path).unwrap());

    let connection = Connection::open(&path).unwrap();
    connection
        .execute("DELETE FROM application_settings WHERE singleton_id=1", [])
        .unwrap();
    drop(connection);

    let mut store = SettingsStore::open(&path).unwrap();
    let error = store
        .save_application_settings(&ApplicationSettings::default())
        .unwrap_err();
    assert!(matches!(error, SettingsError::InvalidData(_)));
}

#[test]
fn corrupt_database_bytes_survive_failed_open_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.sqlite3");
    let original = b"not-a-sqlite-database\0authoritative-user-data".to_vec();
    fs::write(&path, &original).unwrap();

    assert!(SettingsStore::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}
