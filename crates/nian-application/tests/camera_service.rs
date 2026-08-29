// Integration tests use panicking assertions/setup helpers deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nian_application::{
    ApplicationSettingsDto, CameraDraft, CameraService, CameraServiceError, CameraWarning,
    CredentialStore, CredentialStoreError, SettingsRepository,
};
use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Credentials,
    Host, RetentionPolicy,
};
use nian_index::{RecordingIndex, RecordingKind, RecordingUpsert};
use nian_settings::ApplicationSettings;
use tempfile::tempdir;

#[derive(Default)]
struct RepoState {
    cameras: HashMap<String, CameraConfig>,
    fail_insert: bool,
    fail_update: bool,
    fail_delete: bool,
}

#[derive(Clone)]
struct FakeRepo(Arc<Mutex<RepoState>>);

impl FakeRepo {
    fn new() -> (Self, Arc<Mutex<RepoState>>) {
        let state = Arc::new(Mutex::new(RepoState::default()));
        (Self(state.clone()), state)
    }
}

impl SettingsRepository for FakeRepo {
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, String> {
        Ok(self.0.lock().unwrap().cameras.values().cloned().collect())
    }
    fn get_camera(&self, camera_id: &CameraId) -> Result<Option<CameraConfig>, String> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .cameras
            .get(camera_id.as_str())
            .cloned())
    }
    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), String> {
        let mut state = self.0.lock().unwrap();
        if state.fail_insert {
            return Err("injected insert failure".into());
        }
        if state.cameras.contains_key(camera.camera_id().as_str()) {
            return Err("duplicate".into());
        }
        state
            .cameras
            .insert(camera.camera_id().as_str().to_owned(), camera.clone());
        Ok(())
    }
    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, String> {
        let mut state = self.0.lock().unwrap();
        if state.fail_update {
            return Err("injected update failure".into());
        }
        if !state.cameras.contains_key(camera.camera_id().as_str()) {
            return Ok(false);
        }
        state
            .cameras
            .insert(camera.camera_id().as_str().to_owned(), camera.clone());
        Ok(true)
    }
    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, String> {
        let mut state = self.0.lock().unwrap();
        if state.fail_delete {
            return Err("injected delete failure".into());
        }
        Ok(state.cameras.remove(camera_id.as_str()).is_some())
    }
    fn application_settings(&self) -> Result<ApplicationSettings, String> {
        Ok(ApplicationSettings {
            storage_root: Some(PathBuf::from("/tmp/nian-camera-service-test")),
            segment_target_secs: 300,
            retention: RetentionPolicy::default(),
            quota: None,
        })
    }
    fn save_application_settings(&mut self, _settings: &ApplicationSettings) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct SecretState {
    entries: HashMap<String, Credentials>,
    puts: Vec<String>,
    deletes: Vec<String>,
    fail_delete_refs: Vec<String>,
}

#[derive(Default)]
struct FakeSecrets(Mutex<SecretState>);

impl CredentialStore for FakeSecrets {
    fn put(
        &self,
        reference: &CredentialRef,
        credentials: &Credentials,
    ) -> Result<(), CredentialStoreError> {
        let mut state = self.0.lock().unwrap();
        state.puts.push(reference.as_str().to_owned());
        state
            .entries
            .insert(reference.as_str().to_owned(), credentials.clone());
        Ok(())
    }
    fn get(&self, reference: &CredentialRef) -> Result<Credentials, CredentialStoreError> {
        self.0
            .lock()
            .unwrap()
            .entries
            .get(reference.as_str())
            .cloned()
            .ok_or_else(|| CredentialStoreError::new("get"))
    }
    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError> {
        let mut state = self.0.lock().unwrap();
        state.deletes.push(reference.as_str().to_owned());
        if state
            .fail_delete_refs
            .iter()
            .any(|value| value == reference.as_str())
        {
            return Err(CredentialStoreError::new("delete"));
        }
        state.entries.remove(reference.as_str());
        Ok(())
    }
}

fn draft(name: &str, password: Option<&str>) -> CameraDraft {
    CameraDraft {
        camera_id: "front-door".to_owned(),
        display_name: name.to_owned(),
        host: "192.168.1.50".to_owned(),
        port: 554,
        path: "/stream1".to_owned(),
        audio_policy: AudioPolicy::CopyAll,
        replacement_credentials: password.map(|password| Credentials::new("admin", password)),
    }
}

fn existing(ref_text: &str) -> CameraConfig {
    CameraConfig::new(
        CameraId::parse("front-door").unwrap(),
        "Front door",
        CameraSource::Rtsp(
            CameraEndpoint::new(Host::parse("192.168.1.50").unwrap(), 554, "/stream1").unwrap(),
        ),
        AudioPolicy::CopyAll,
        CredentialRef::parse(ref_text).unwrap(),
    )
    .unwrap()
}

fn seeded() -> (CameraService, Arc<Mutex<RepoState>>, Arc<FakeSecrets>) {
    let (repo, repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());
    repo_state
        .lock()
        .unwrap()
        .cameras
        .insert("front-door".into(), existing("old-ref"));
    secrets
        .0
        .lock()
        .unwrap()
        .entries
        .insert("old-ref".into(), Credentials::new("admin", "old-password"));
    (
        CameraService::new(Box::new(repo), secrets.clone()),
        repo_state,
        secrets,
    )
}

#[test]
fn create_db_failure_cleans_new_secret_and_commits_no_camera() {
    let (repo, repo_state) = FakeRepo::new();
    repo_state.lock().unwrap().fail_insert = true;
    let secrets = Arc::new(FakeSecrets::default());
    let mut service = CameraService::new(Box::new(repo), secrets.clone());

    let error = service
        .create_camera(draft("Front door", Some("new-password")))
        .unwrap_err();
    assert!(matches!(error, CameraServiceError::Settings));
    assert!(repo_state.lock().unwrap().cameras.is_empty());
    let state = secrets.0.lock().unwrap();
    assert_eq!(state.puts.len(), 1);
    assert_eq!(state.deletes, state.puts);
    assert!(state.entries.is_empty());
}

#[test]
fn update_db_failure_keeps_old_ref_authoritative_and_cleans_new_secret() {
    let (mut service, repo, secrets) = seeded();
    repo.lock().unwrap().fail_update = true;

    let error = service
        .update_camera(draft("Front door", Some("new-password")), None)
        .unwrap_err();
    assert!(matches!(error, CameraServiceError::Settings));
    let camera = repo
        .lock()
        .unwrap()
        .cameras
        .get("front-door")
        .cloned()
        .unwrap();
    assert_eq!(camera.credential_ref().as_str(), "old-ref");
    let state = secrets.0.lock().unwrap();
    assert!(state.entries.contains_key("old-ref"));
    assert_eq!(state.puts.len(), 1);
    assert_eq!(state.deletes.last(), state.puts.last());
}

#[test]
fn update_commit_survives_old_secret_cleanup_failure_with_warning() {
    let (mut service, repo, secrets) = seeded();
    secrets
        .0
        .lock()
        .unwrap()
        .fail_delete_refs
        .push("old-ref".into());

    let outcome = service
        .update_camera(draft("Renamed", Some("new-password")), None)
        .unwrap();
    assert_eq!(
        outcome.warning,
        Some(CameraWarning::OrphanCredentialCleanupFailed)
    );
    let camera = repo
        .lock()
        .unwrap()
        .cameras
        .get("front-door")
        .cloned()
        .unwrap();
    assert_ne!(camera.credential_ref().as_str(), "old-ref");
    assert_eq!(camera.display_name(), "Renamed");
    assert!(
        secrets
            .0
            .lock()
            .unwrap()
            .entries
            .contains_key(camera.credential_ref().as_str())
    );
}

#[test]
fn delete_commit_survives_secret_cleanup_failure() {
    let (mut service, repo, secrets) = seeded();
    secrets
        .0
        .lock()
        .unwrap()
        .fail_delete_refs
        .push("old-ref".into());

    let outcome = service.delete_camera("front-door", None).unwrap();
    assert_eq!(
        outcome.warning,
        Some(CameraWarning::OrphanCredentialCleanupFailed)
    );
    assert!(!repo.lock().unwrap().cameras.contains_key("front-door"));
}

#[test]
fn active_camera_rejects_critical_edit_and_delete_but_allows_display_name_only() {
    let (mut service, repo, _secrets) = seeded();
    let active = CameraId::parse("front-door").unwrap();

    let mut critical = draft("Front door", None);
    critical.path = "/stream2".into();
    assert!(matches!(
        service.update_camera(critical, Some(&active)),
        Err(CameraServiceError::CameraBusy)
    ));
    assert!(matches!(
        service.delete_camera("front-door", Some(&active)),
        Err(CameraServiceError::CameraBusy)
    ));

    let outcome = service
        .update_camera(draft("New display name", None), Some(&active))
        .unwrap();
    assert_eq!(outcome.value.display_name, "New display name");
    assert_eq!(
        repo.lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .camera_id()
            .as_str(),
        "front-door"
    );
}

#[test]
fn secret_never_appears_in_safe_dtos_or_prepared_debug_output() {
    let sentinel = "SENTINEL-password-7db84b";
    let (mut service, _repo, _secrets) = seeded();
    service
        .update_camera(draft("Front door", Some(sentinel)), None)
        .unwrap();

    let list = service.list_cameras().unwrap();
    let list_json = serde_json::to_string(&list).unwrap();
    assert!(!list_json.contains(sentinel));
    assert!(!format!("{list:?}").contains(sentinel));

    let recording = service.prepare_recording("front-door").unwrap();
    assert!(!format!("{recording:?}").contains(sentinel));
    let probe = service.prepare_probe("front-door", 2_000).unwrap();
    assert!(!format!("{probe:?}").contains(sentinel));
}

#[test]
fn settings_validation_reuses_application_policy() {
    let (mut service, _repo, _secrets) = seeded();
    let bad = ApplicationSettingsDto {
        storage_root: Some("relative/path".into()),
        segment_target_secs: 300,
        max_age_days: None,
        max_storage_bytes: None,
        cleanup_target_bytes: None,
    };
    assert!(matches!(
        service.save_application_settings(bad, false),
        Err(CameraServiceError::Validation(_))
    ));
}

#[test]
fn unsaved_probe_uses_form_credentials_without_persisting_camera() {
    let (repo, repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());
    let service = CameraService::new(Box::new(repo), secrets);
    let sentinel = "probe-only-secret-91a";

    let probe = service
        .prepare_probe_draft(&draft("Unsaved", Some(sentinel)), 2_000)
        .unwrap();
    assert_eq!(probe.camera_id, "front-door");
    assert!(probe.source_json.to_string().contains(sentinel));
    assert!(!format!("{probe:?}").contains(sentinel));
    assert!(repo_state.lock().unwrap().cameras.is_empty());
}

#[test]
fn edited_probe_with_blank_password_reuses_committed_secret_for_new_endpoint() {
    let (service, _repo, _secrets) = seeded();
    let mut edited = draft("Front door", None);
    edited.path = "/stream2".into();

    let probe = service.prepare_probe_draft(&edited, 2_000).unwrap();
    let wire = probe.source_json.to_string();
    assert!(wire.contains("/stream2"));
    assert!(wire.contains("old-password"));
    assert!(!format!("{probe:?}").contains("old-password"));
}

#[test]
fn camera_password_never_enters_recording_index_sqlite_family() {
    let sentinel = "SENTINEL-index-password-never-store";
    let (repo, _repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());
    let mut service = CameraService::new(Box::new(repo), secrets);
    service
        .create_camera(draft("Front door", Some(sentinel)))
        .unwrap();

    let dir = tempdir().unwrap();
    let index_path = dir.path().join("recordings.sqlite3");
    let mut index = RecordingIndex::open(&index_path).unwrap();
    index
        .upsert(&RecordingUpsert {
            camera_id: CameraId::parse("front-door").unwrap(),
            relative_path: "front-door/2026/08/29/12-00-00.mkv".to_owned(),
            kind: RecordingKind::Normal,
            state: nian_domain::RecordingState::Complete,
            started_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 29)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
            sequence: 1,
            size_bytes: 1_234,
            media_duration_ms: None,
        })
        .unwrap();
    drop(index);

    let paths = [
        index_path.clone(),
        PathBuf::from(format!("{}-wal", index_path.display())),
        PathBuf::from(format!("{}-shm", index_path.display())),
    ];
    for path in paths.into_iter().filter(|path| path.exists()) {
        let bytes = fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes()),
            "credential sentinel leaked into {}",
            path.display()
        );
    }
}
