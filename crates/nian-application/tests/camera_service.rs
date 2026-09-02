// Integration tests use panicking assertions/setup helpers deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nian_application::{
    ApplicationSettingsDto, CameraDraft, CameraService, CameraServiceError, CameraWarning,
    CredentialRefGenerator, CredentialRefGeneratorError, CredentialStore, CredentialStoreError,
    RandomCredentialRefGenerator, SettingsRepository, SettingsRepositoryError,
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
    hide_get_camera_once: bool,
    desired_cameras: BTreeSet<String>,
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
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, SettingsRepositoryError> {
        Ok(self.0.lock().unwrap().cameras.values().cloned().collect())
    }
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if state.hide_get_camera_once {
            state.hide_get_camera_once = false;
            return Ok(None);
        }
        Ok(state.cameras.get(camera_id.as_str()).cloned())
    }
    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_insert {
            return Err(SettingsRepositoryError::Persistence);
        }
        if state.cameras.contains_key(camera.camera_id().as_str()) {
            return Err(SettingsRepositoryError::DuplicateCamera);
        }
        state
            .cameras
            .insert(camera.camera_id().as_str().to_owned(), camera.clone());
        Ok(())
    }
    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_update {
            return Err(SettingsRepositoryError::Persistence);
        }
        if !state.cameras.contains_key(camera.camera_id().as_str()) {
            return Ok(false);
        }
        state
            .cameras
            .insert(camera.camera_id().as_str().to_owned(), camera.clone());
        Ok(true)
    }
    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_delete {
            return Err(SettingsRepositoryError::Persistence);
        }
        Ok(state.cameras.remove(camera_id.as_str()).is_some())
    }
    fn application_settings(&self) -> Result<ApplicationSettings, SettingsRepositoryError> {
        Ok(ApplicationSettings {
            storage_root: Some(PathBuf::from("/tmp/nian-camera-service-test")),
            segment_target_secs: 300,
            retention: RetentionPolicy::default(),
            quota: None,
            launch_at_login: false,
        })
    }
    fn save_application_settings(
        &mut self,
        _settings: &ApplicationSettings,
    ) -> Result<(), SettingsRepositoryError> {
        Ok(())
    }
    fn recording_enabled_cameras(&self) -> Result<Vec<CameraId>, SettingsRepositoryError> {
        self.0
            .lock()
            .unwrap()
            .desired_cameras
            .iter()
            .map(|camera| CameraId::parse(camera).map_err(|_| SettingsRepositoryError::Persistence))
            .collect()
    }
    fn set_recording_enabled(
        &mut self,
        camera_id: &CameraId,
        enabled: bool,
    ) -> Result<bool, SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if !state.cameras.contains_key(camera_id.as_str()) {
            return Ok(false);
        }
        if enabled {
            state.desired_cameras.insert(camera_id.as_str().to_owned());
        } else {
            state.desired_cameras.remove(camera_id.as_str());
        }
        Ok(true)
    }
    fn set_all_recording_enabled(&mut self, enabled: bool) -> Result<(), SettingsRepositoryError> {
        let mut state = self.0.lock().unwrap();
        if enabled {
            state.desired_cameras = state.cameras.keys().cloned().collect();
        } else {
            state.desired_cameras.clear();
        }
        Ok(())
    }
}

#[derive(Default)]
struct SecretState {
    entries: HashMap<String, Credentials>,
    puts: Vec<String>,
    deletes: Vec<String>,
    fail_delete_refs: Vec<String>,
    fail_all_deletes: bool,
}

#[derive(Default)]
struct FakeSecrets(Mutex<SecretState>);

struct DeterministicCredentialRefGenerator {
    candidates: Mutex<VecDeque<CredentialRef>>,
    calls: AtomicUsize,
}

impl DeterministicCredentialRefGenerator {
    fn new(candidates: &[&str]) -> Self {
        assert!(!candidates.is_empty());
        Self {
            candidates: Mutex::new(
                candidates
                    .iter()
                    .map(|value| CredentialRef::parse(*value).unwrap())
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CredentialRefGenerator for DeterministicCredentialRefGenerator {
    fn generate(
        &self,
        _camera_id: &CameraId,
    ) -> Result<CredentialRef, CredentialRefGeneratorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut candidates = self
            .candidates
            .lock()
            .map_err(|_| CredentialRefGeneratorError)?;
        match candidates.len() {
            0 => Err(CredentialRefGeneratorError),
            1 => candidates
                .front()
                .cloned()
                .ok_or(CredentialRefGeneratorError),
            _ => candidates.pop_front().ok_or(CredentialRefGeneratorError),
        }
    }
}

impl CredentialStore for FakeSecrets {
    fn exists(&self, reference: &CredentialRef) -> Result<bool, CredentialStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .entries
            .contains_key(reference.as_str()))
    }

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
        if state.fail_all_deletes
            || state
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
        launch_at_login: false,
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
fn invalid_display_name_with_replacement_password_writes_no_new_secret() {
    let (mut service, _repo, secrets) = seeded();
    let invalid_name = "x".repeat(nian_domain::MAX_DISPLAY_NAME_LEN + 1);

    let error = service
        .update_camera(draft(&invalid_name, Some("new-password")), None)
        .unwrap_err();
    assert!(matches!(error, CameraServiceError::Validation(_)));
    let state = secrets.0.lock().unwrap();
    assert!(state.puts.is_empty());
    assert_eq!(state.entries.len(), 1);
    assert!(state.entries.contains_key("old-ref"));
}

#[test]
fn create_db_failure_and_secret_rollback_failure_is_observable() {
    let (repo, repo_state) = FakeRepo::new();
    repo_state.lock().unwrap().fail_insert = true;
    let secrets = Arc::new(FakeSecrets::default());
    secrets.0.lock().unwrap().fail_all_deletes = true;
    let mut service = CameraService::new(Box::new(repo), secrets.clone());

    let error = service
        .create_camera(draft("Front door", Some("new-password")))
        .unwrap_err();
    assert!(matches!(
        error,
        CameraServiceError::CredentialRollbackCleanup {
            operation: "create"
        }
    ));
    assert!(repo_state.lock().unwrap().cameras.is_empty());
    assert_eq!(secrets.0.lock().unwrap().entries.len(), 1);
}

#[test]
fn update_db_failure_and_secret_rollback_failure_is_observable() {
    let (mut service, repo, secrets) = seeded();
    repo.lock().unwrap().fail_update = true;
    secrets.0.lock().unwrap().fail_all_deletes = true;

    let error = service
        .update_camera(draft("Front door", Some("new-password")), None)
        .unwrap_err();
    assert!(matches!(
        error,
        CameraServiceError::CredentialRollbackCleanup {
            operation: "update"
        }
    ));
    assert_eq!(
        repo.lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        "old-ref"
    );
}

#[test]
fn duplicate_camera_id_remains_a_typed_application_error() {
    let (repo, repo_state) = FakeRepo::new();
    repo_state
        .lock()
        .unwrap()
        .cameras
        .insert("front-door".into(), existing("existing-ref"));
    let secrets = Arc::new(FakeSecrets::default());
    let mut service = CameraService::new(Box::new(repo), secrets);

    assert!(matches!(
        service.create_camera(draft("Duplicate", Some("new-password"))),
        Err(CameraServiceError::DuplicateCamera)
    ));
}

#[test]
fn one_sided_replacement_credentials_are_rejected() {
    let (mut service, _repo, secrets) = seeded();
    let mut missing_password = draft("Front door", None);
    missing_password.replacement_credentials = Some(Credentials::new("new-user", ""));
    assert!(matches!(
        service.update_camera(missing_password, None),
        Err(CameraServiceError::Validation(_))
    ));

    let mut missing_username = draft("Front door", None);
    missing_username.replacement_credentials = Some(Credentials::new("", "new-password"));
    assert!(matches!(
        service.update_camera(missing_username.clone(), None),
        Err(CameraServiceError::Validation(_))
    ));
    assert!(matches!(
        service.prepare_probe_draft(&missing_username, 2_000),
        Err(CameraServiceError::Validation(_))
    ));
    assert!(secrets.0.lock().unwrap().puts.is_empty());
}

#[test]
fn oversized_rtsp_path_is_rejected_before_persistence_or_secret_write() {
    let (repo, repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());
    let mut service = CameraService::new(Box::new(repo), secrets.clone());
    let mut oversized = draft("Front door", Some("password"));
    oversized.path = format!("/{}", "a".repeat(nian_domain::MAX_RTSP_PATH_LEN));

    assert!(matches!(
        service.create_camera(oversized),
        Err(CameraServiceError::Validation(_))
    ));
    assert!(repo_state.lock().unwrap().cameras.is_empty());
    assert!(secrets.0.lock().unwrap().puts.is_empty());
}

#[test]
fn generated_replacement_ref_equal_to_committed_ref_never_touches_old_secret() {
    let committed_ref = "nian-vision/front-door/collision";
    let (repo, repo_state) = FakeRepo::new();
    repo_state
        .lock()
        .unwrap()
        .cameras
        .insert("front-door".into(), existing(committed_ref));
    let secrets = Arc::new(FakeSecrets::default());
    secrets.0.lock().unwrap().entries.insert(
        committed_ref.into(),
        Credentials::new("admin", "OLD_SECRET"),
    );
    let generator = Arc::new(DeterministicCredentialRefGenerator::new(&[committed_ref]));
    let mut service = CameraService::with_credential_ref_generator(
        Box::new(repo),
        secrets.clone(),
        generator.clone(),
    );

    let error = service
        .update_camera(draft("Front door", Some("NEW_SECRET")), None)
        .unwrap_err();
    assert!(matches!(error, CameraServiceError::CredentialRefCollision));
    assert!(
        generator.calls() > 1,
        "collision candidates should be retried"
    );

    let repo = repo_state.lock().unwrap();
    assert_eq!(
        repo.cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        committed_ref
    );
    drop(repo);
    let state = secrets.0.lock().unwrap();
    assert!(state.puts.is_empty());
    assert!(state.deletes.is_empty());
    let old = state.entries.get(committed_ref).unwrap();
    assert_eq!(old.password(), "OLD_SECRET");
}

#[test]
fn replacement_generator_retries_collision_and_commits_distinct_ref() {
    let committed_ref = "nian-vision/front-door/collision";
    let distinct_ref = "nian-vision/front-door/00000000-0000-4000-8000-000000000002";
    let (repo, repo_state) = FakeRepo::new();
    repo_state
        .lock()
        .unwrap()
        .cameras
        .insert("front-door".into(), existing(committed_ref));
    let secrets = Arc::new(FakeSecrets::default());
    secrets.0.lock().unwrap().entries.insert(
        committed_ref.into(),
        Credentials::new("admin", "OLD_SECRET"),
    );
    let generator = Arc::new(DeterministicCredentialRefGenerator::new(&[
        committed_ref,
        distinct_ref,
    ]));
    let mut service = CameraService::with_credential_ref_generator(
        Box::new(repo),
        secrets.clone(),
        generator.clone(),
    );

    service
        .update_camera(draft("Front door", Some("NEW_SECRET")), None)
        .unwrap();
    assert_eq!(generator.calls(), 2);
    assert_eq!(
        repo_state
            .lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        distinct_ref
    );
    let state = secrets.0.lock().unwrap();
    assert_eq!(state.puts, vec![distinct_ref]);
    assert_eq!(state.deletes, vec![committed_ref]);
    assert_eq!(
        state.entries.get(distinct_ref).unwrap().password(),
        "NEW_SECRET"
    );
}

#[test]
fn failed_update_rollback_deletes_only_distinct_transaction_owned_ref() {
    let committed_ref = "nian-vision/front-door/collision";
    let distinct_ref = "nian-vision/front-door/00000000-0000-4000-8000-000000000003";
    let (repo, repo_state) = FakeRepo::new();
    {
        let mut repo = repo_state.lock().unwrap();
        repo.cameras
            .insert("front-door".into(), existing(committed_ref));
        repo.fail_update = true;
    }
    let secrets = Arc::new(FakeSecrets::default());
    secrets.0.lock().unwrap().entries.insert(
        committed_ref.into(),
        Credentials::new("admin", "OLD_SECRET"),
    );
    let generator = Arc::new(DeterministicCredentialRefGenerator::new(&[
        committed_ref,
        distinct_ref,
    ]));
    let mut service =
        CameraService::with_credential_ref_generator(Box::new(repo), secrets.clone(), generator);

    assert!(matches!(
        service.update_camera(draft("Front door", Some("NEW_SECRET")), None),
        Err(CameraServiceError::Settings)
    ));
    assert_eq!(
        repo_state
            .lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        committed_ref
    );
    let state = secrets.0.lock().unwrap();
    assert_eq!(state.puts, vec![distinct_ref]);
    assert_eq!(state.deletes, vec![distinct_ref]);
    assert_eq!(
        state.entries.get(committed_ref).unwrap().password(),
        "OLD_SECRET"
    );
    assert!(!state.entries.contains_key(distinct_ref));
}

#[test]
fn duplicate_create_collision_never_deletes_winning_committed_credential() {
    let collision_ref = "nian-vision/front-door/00000000-0000-4000-8000-000000000004";
    let (repo, repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());

    let winner_generator = Arc::new(DeterministicCredentialRefGenerator::new(&[collision_ref]));
    let mut winner = CameraService::with_credential_ref_generator(
        Box::new(repo.clone()),
        secrets.clone(),
        winner_generator,
    );
    winner
        .create_camera(draft("Front door", Some("OLD_SECRET")))
        .unwrap();

    let loser_generator = Arc::new(DeterministicCredentialRefGenerator::new(&[collision_ref]));
    let mut loser = CameraService::with_credential_ref_generator(
        Box::new(repo),
        secrets.clone(),
        loser_generator.clone(),
    );
    assert!(matches!(
        loser.create_camera(draft("Duplicate", Some("NEW_SECRET"))),
        Err(CameraServiceError::DuplicateCamera)
    ));
    assert_eq!(
        loser_generator.calls(),
        0,
        "a locally-known duplicate must fail before allocating or writing a credential ref"
    );

    assert_eq!(
        repo_state
            .lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        collision_ref
    );
    let state = secrets.0.lock().unwrap();
    assert_eq!(state.puts, vec![collision_ref]);
    assert!(state.deletes.is_empty());
    assert_eq!(
        state.entries.get(collision_ref).unwrap().password(),
        "OLD_SECRET"
    );
}

#[test]
fn duplicate_create_race_with_forced_ref_collision_never_overwrites_or_deletes_winner() {
    let collision_ref = "nian-vision/front-door/00000000-0000-4000-8000-000000000007";
    let loser_ref = "nian-vision/front-door/00000000-0000-4000-8000-000000000008";
    let (repo, repo_state) = FakeRepo::new();
    {
        let mut state = repo_state.lock().unwrap();
        state
            .cameras
            .insert("front-door".into(), existing(collision_ref));
        // Model another process committing after this process's CREATE pre-check
        // but before the authoritative INSERT. The INSERT still sees duplicate.
        state.hide_get_camera_once = true;
    }
    let secrets = Arc::new(FakeSecrets::default());
    secrets.0.lock().unwrap().entries.insert(
        collision_ref.into(),
        Credentials::new("admin", "OLD_SECRET"),
    );
    let generator = Arc::new(DeterministicCredentialRefGenerator::new(&[
        collision_ref,
        loser_ref,
    ]));
    let mut loser = CameraService::with_credential_ref_generator(
        Box::new(repo),
        secrets.clone(),
        generator.clone(),
    );

    assert!(matches!(
        loser.create_camera(draft("Duplicate", Some("NEW_SECRET"))),
        Err(CameraServiceError::DuplicateCamera)
    ));
    assert_eq!(generator.calls(), 2);
    assert_eq!(
        repo_state
            .lock()
            .unwrap()
            .cameras
            .get("front-door")
            .unwrap()
            .credential_ref()
            .as_str(),
        collision_ref
    );
    let state = secrets.0.lock().unwrap();
    assert_eq!(state.puts, vec![loser_ref]);
    assert_eq!(state.deletes, vec![loser_ref]);
    assert_eq!(
        state.entries.get(collision_ref).unwrap().password(),
        "OLD_SECRET"
    );
    assert!(!state.entries.contains_key(loser_ref));
}

#[test]
fn random_production_credential_refs_use_camera_scoped_uuid_v4_grammar() {
    let camera = CameraId::parse("front-door").unwrap();
    let generator = RandomCredentialRefGenerator;
    let first = generator.generate(&camera).unwrap();
    let second = generator.generate(&camera).unwrap();
    let prefix = "nian-vision/front-door/";

    for reference in [&first, &second] {
        let suffix = reference.as_str().strip_prefix(prefix).unwrap();
        let parsed = uuid::Uuid::parse_str(suffix).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
    }
    assert_ne!(first, second);
}

#[test]
fn persisted_camera_debug_contains_only_opaque_ref_not_credential_secret() {
    let reference = "nian-vision/front-door/00000000-0000-4000-8000-000000000005";
    let (repo, repo_state) = FakeRepo::new();
    let secrets = Arc::new(FakeSecrets::default());
    let generator = Arc::new(DeterministicCredentialRefGenerator::new(&[reference]));
    let mut service =
        CameraService::with_credential_ref_generator(Box::new(repo), secrets, generator);

    service
        .create_camera(draft("Front door", Some("DEBUG_SECRET_SENTINEL")))
        .unwrap();
    let saved = repo_state
        .lock()
        .unwrap()
        .cameras
        .get("front-door")
        .cloned()
        .unwrap();
    let debug = format!("{saved:?}");
    assert!(debug.contains(reference));
    assert!(!debug.contains("DEBUG_SECRET_SENTINEL"));
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
