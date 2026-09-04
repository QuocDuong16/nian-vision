//! Camera-management application service and secret-store boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Credentials,
    Host, PtzBinding, RetentionPolicy, StorageQuota,
};
use nian_settings::{ApplicationSettings, SettingsError, SettingsStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{AppConfig, DesiredRecording, PreparedLive};

const MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS: usize = 8;

/// Native-secret-store abstraction. Implementations must keep all error text
/// secret-safe: errors may describe an operation but never echo credentials.
pub trait CredentialStore: Send + Sync {
    /// Returns whether an identity is already occupied without exposing its secret.
    fn exists(&self, reference: &CredentialRef) -> Result<bool, CredentialStoreError>;
    fn put(
        &self,
        reference: &CredentialRef,
        credentials: &Credentials,
    ) -> Result<(), CredentialStoreError>;
    fn get(&self, reference: &CredentialRef) -> Result<Credentials, CredentialStoreError>;
    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError>;
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("credential store operation failed: {operation}")]
pub struct CredentialStoreError {
    operation: &'static str,
}

impl CredentialStoreError {
    pub const fn new(operation: &'static str) -> Self {
        Self { operation }
    }
}

/// Secret-safe failure from the credential-reference identity source.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("credential reference generation failed")]
pub struct CredentialRefGeneratorError;

/// Application-owned credential identity source. Reference uniqueness is part
/// of the credential transaction contract because native keyring identities
/// are mutable keys: writing an existing identity replaces its secret.
pub trait CredentialRefGenerator: Send + Sync {
    fn generate(&self, camera_id: &CameraId) -> Result<CredentialRef, CredentialRefGeneratorError>;
}

/// Production generator. UUID v4 identity does not depend on wall-clock, PID,
/// process lifetime, or a process-local counter.
#[derive(Debug, Default)]
pub struct RandomCredentialRefGenerator;

impl CredentialRefGenerator for RandomCredentialRefGenerator {
    fn generate(&self, camera_id: &CameraId) -> Result<CredentialRef, CredentialRefGeneratorError> {
        CredentialRef::parse(format!(
            "nian-vision/{}/{}",
            camera_id.as_str(),
            Uuid::new_v4()
        ))
        .map_err(|_| CredentialRefGeneratorError)
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SettingsRepositoryError {
    #[error("camera already exists")]
    DuplicateCamera,
    #[error("settings repository operation failed")]
    Persistence,
}

/// CI/test credential store that never reaches a graphical/native keychain.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    entries: Mutex<HashMap<String, Credentials>>,
}

impl CredentialStore for MemoryCredentialStore {
    fn exists(&self, reference: &CredentialRef) -> Result<bool, CredentialStoreError> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| CredentialStoreError::new("lock"))?
            .contains_key(reference.as_str()))
    }

    fn put(
        &self,
        reference: &CredentialRef,
        credentials: &Credentials,
    ) -> Result<(), CredentialStoreError> {
        self.entries
            .lock()
            .map_err(|_| CredentialStoreError::new("lock"))?
            .insert(reference.as_str().to_owned(), credentials.clone());
        Ok(())
    }

    fn get(&self, reference: &CredentialRef) -> Result<Credentials, CredentialStoreError> {
        self.entries
            .lock()
            .map_err(|_| CredentialStoreError::new("lock"))?
            .get(reference.as_str())
            .cloned()
            .ok_or_else(|| CredentialStoreError::new("get"))
    }

    fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialStoreError> {
        self.entries
            .lock()
            .map_err(|_| CredentialStoreError::new("lock"))?
            .remove(reference.as_str());
        Ok(())
    }
}

/// Persistence seam used by fault-injection tests. Production is backed by
/// `nian-settings`, whose database lives in platform app-data, not footage.
pub trait SettingsRepository: Send {
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, SettingsRepositoryError>;
    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError>;
    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), SettingsRepositoryError>;
    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, SettingsRepositoryError>;
    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, SettingsRepositoryError>;
    fn get_ptz_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<PtzBinding>, SettingsRepositoryError>;
    fn application_settings(&self) -> Result<ApplicationSettings, SettingsRepositoryError>;
    fn save_application_settings(
        &mut self,
        settings: &ApplicationSettings,
    ) -> Result<(), SettingsRepositoryError>;
    fn recording_enabled_cameras(&self) -> Result<Vec<CameraId>, SettingsRepositoryError>;
    fn set_recording_enabled(
        &mut self,
        camera_id: &CameraId,
        enabled: bool,
    ) -> Result<bool, SettingsRepositoryError>;
    fn set_all_recording_enabled(&mut self, enabled: bool) -> Result<(), SettingsRepositoryError>;
}

impl SettingsRepository for SettingsStore {
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, SettingsRepositoryError> {
        SettingsStore::list_cameras(self).map_err(repository_error)
    }

    fn get_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<CameraConfig>, SettingsRepositoryError> {
        SettingsStore::get_camera(self, camera_id).map_err(repository_error)
    }

    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), SettingsRepositoryError> {
        SettingsStore::insert_camera(self, camera).map_err(repository_error)
    }

    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::update_camera(self, camera).map_err(repository_error)
    }

    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::delete_camera(self, camera_id).map_err(repository_error)
    }

    fn get_ptz_binding(
        &self,
        camera_id: &CameraId,
    ) -> Result<Option<PtzBinding>, SettingsRepositoryError> {
        SettingsStore::get_ptz_binding(self, camera_id).map_err(repository_error)
    }

    fn application_settings(&self) -> Result<ApplicationSettings, SettingsRepositoryError> {
        SettingsStore::application_settings(self).map_err(repository_error)
    }

    fn save_application_settings(
        &mut self,
        settings: &ApplicationSettings,
    ) -> Result<(), SettingsRepositoryError> {
        SettingsStore::save_application_settings(self, settings).map_err(repository_error)
    }

    fn recording_enabled_cameras(&self) -> Result<Vec<CameraId>, SettingsRepositoryError> {
        SettingsStore::recording_enabled_cameras(self).map_err(repository_error)
    }

    fn set_recording_enabled(
        &mut self,
        camera_id: &CameraId,
        enabled: bool,
    ) -> Result<bool, SettingsRepositoryError> {
        SettingsStore::set_recording_enabled(self, camera_id, enabled).map_err(repository_error)
    }
    fn set_all_recording_enabled(&mut self, enabled: bool) -> Result<(), SettingsRepositoryError> {
        SettingsStore::set_all_recording_enabled(self, enabled).map_err(repository_error)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CameraSummary {
    pub camera_id: String,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub audio_policy: AudioPolicy,
}

impl From<&CameraConfig> for CameraSummary {
    fn from(camera: &CameraConfig) -> Self {
        let CameraSource::Rtsp(endpoint) = camera.source();
        Self {
            camera_id: camera.camera_id().as_str().to_owned(),
            display_name: camera.display_name().to_owned(),
            host: endpoint.host().as_str().to_owned(),
            port: endpoint.port(),
            path: endpoint.path().to_owned(),
            audio_policy: camera.audio_policy(),
        }
    }
}

/// Command-side camera data. Credentials are optional for updates so the UI
/// can explicitly leave the current secret untouched. This type intentionally
/// does not implement `Debug` or `Serialize`.
#[derive(Clone)]
pub struct CameraDraft {
    pub camera_id: String,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub audio_policy: AudioPolicy,
    pub replacement_credentials: Option<Credentials>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CameraWarning {
    OrphanCredentialCleanupFailed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CameraMutation<T> {
    pub value: T,
    pub warning: Option<CameraWarning>,
}

#[derive(Debug, Error)]
pub enum CameraServiceError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("camera not found")]
    CameraNotFound,
    #[error("camera already exists")]
    DuplicateCamera,
    #[error("camera is actively recording")]
    CameraBusy,
    #[error("camera identity or shared credentials cannot change while PTZ is paired")]
    PtzBindingRequiresUnpair,
    #[error("credential store unavailable")]
    CredentialStore(#[from] CredentialStoreError),
    #[error("settings persistence failed")]
    Settings,
    #[error("credential rollback cleanup failed after {operation}")]
    CredentialRollbackCleanup { operation: &'static str },
    #[error("credential reference generation failed")]
    CredentialRefGeneration(#[from] CredentialRefGeneratorError),
    #[error("could not allocate a distinct credential reference")]
    CredentialRefCollision,
    #[error("recording storage is not configured")]
    StorageNotConfigured,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplicationSettingsDto {
    pub storage_root: Option<String>,
    pub segment_target_secs: u64,
    pub max_age_days: Option<u32>,
    pub max_storage_bytes: Option<u64>,
    pub cleanup_target_bytes: Option<u64>,
    pub launch_at_login: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedApplicationSettings {
    settings: ApplicationSettings,
}

impl PreparedApplicationSettings {
    pub fn dto(&self) -> ApplicationSettingsDto {
        self.settings.clone().into()
    }
}

impl From<ApplicationSettings> for ApplicationSettingsDto {
    fn from(value: ApplicationSettings) -> Self {
        Self {
            storage_root: value
                .storage_root
                .map(|path| path.to_string_lossy().into_owned()),
            segment_target_secs: value.segment_target_secs,
            max_age_days: value.retention.max_age_days,
            max_storage_bytes: value.retention.max_storage_bytes,
            cleanup_target_bytes: value.quota.map(|quota| quota.cleanup_target_bytes),
            launch_at_login: value.launch_at_login,
        }
    }
}

/// Secret-bearing probe request. `Debug` deliberately omits `source_json`.
#[derive(Clone)]
pub struct PreparedProbe {
    pub camera_id: String,
    pub source_json: serde_json::Value,
    pub timeout_ms: u64,
}

impl std::fmt::Debug for PreparedProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedProbe")
            .field("camera_id", &self.camera_id)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}

pub struct CameraService {
    repository: Box<dyn SettingsRepository>,
    credentials: Arc<dyn CredentialStore>,
    credential_refs: Arc<dyn CredentialRefGenerator>,
}

impl std::fmt::Debug for CameraService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraService").finish_non_exhaustive()
    }
}

impl CameraService {
    pub fn new(
        repository: Box<dyn SettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self::with_credential_ref_generator(
            repository,
            credentials,
            Arc::new(RandomCredentialRefGenerator),
        )
    }

    pub fn with_credential_ref_generator(
        repository: Box<dyn SettingsRepository>,
        credentials: Arc<dyn CredentialStore>,
        credential_refs: Arc<dyn CredentialRefGenerator>,
    ) -> Self {
        Self {
            repository,
            credentials,
            credential_refs,
        }
    }

    pub fn list_cameras(&self) -> Result<Vec<CameraSummary>, CameraServiceError> {
        self.repository
            .list_cameras()
            .map_err(map_repository_service_error)
            .map(|rows| rows.iter().map(CameraSummary::from).collect())
    }

    pub fn get_camera(&self, camera_id: &str) -> Result<CameraSummary, CameraServiceError> {
        let id = parse_camera_id(camera_id)?;
        self.repository
            .get_camera(&id)
            .map_err(map_repository_service_error)?
            .as_ref()
            .map(CameraSummary::from)
            .ok_or(CameraServiceError::CameraNotFound)
    }

    pub fn create_camera(
        &mut self,
        draft: CameraDraft,
    ) -> Result<CameraMutation<CameraSummary>, CameraServiceError> {
        let credentials = draft.replacement_credentials.clone().ok_or_else(|| {
            CameraServiceError::Validation(
                "username/password are required when creating a camera".to_owned(),
            )
        })?;
        validate_credentials(&credentials)?;

        let camera_id = parse_camera_id(&draft.camera_id)?;
        let endpoint = endpoint_from_draft(&draft)?;
        validate_display_name(&draft.display_name)?;

        // Safety optimization only. The settings DB UNIQUE constraint remains
        // authoritative for cross-process races, but a locally-known duplicate
        // must never write or later roll back a credential owned by that row.
        if self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .is_some()
        {
            return Err(CameraServiceError::DuplicateCamera);
        }

        let credential_ref = self.allocate_credential_ref(&camera_id, None)?;
        let config = CameraConfig::new(
            camera_id,
            draft.display_name,
            CameraSource::Rtsp(endpoint),
            draft.audio_policy,
            credential_ref.clone(),
        )
        .map_err(|error| CameraServiceError::Validation(error.to_string()))?;

        self.credentials.put(&credential_ref, &credentials)?;
        if let Err(error) = self.repository.insert_camera(&config) {
            let original = map_repository_service_error(error);
            return Err(rollback_new_credential(
                self.credentials.as_ref(),
                &credential_ref,
                "create",
                original,
            ));
        }

        Ok(CameraMutation {
            value: CameraSummary::from(&config),
            warning: None,
        })
    }

    pub fn update_camera(
        &mut self,
        draft: CameraDraft,
        active_camera: Option<&CameraId>,
    ) -> Result<CameraMutation<CameraSummary>, CameraServiceError> {
        let camera_id = parse_camera_id(&draft.camera_id)?;
        let previous = self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .ok_or(CameraServiceError::CameraNotFound)?;

        let endpoint = endpoint_from_draft(&draft)?;
        validate_display_name(&draft.display_name)?;
        if let Some(credentials) = &draft.replacement_credentials {
            validate_credentials(credentials)?;
        }

        let critical_change = previous.source() != &CameraSource::Rtsp(endpoint.clone())
            || previous.audio_policy() != draft.audio_policy
            || draft.replacement_credentials.is_some();
        if active_camera == Some(&camera_id) && critical_change {
            return Err(CameraServiceError::CameraBusy);
        }

        if let Some(binding) = self
            .repository
            .get_ptz_binding(&camera_id)
            .map_err(map_repository_service_error)?
        {
            let CameraSource::Rtsp(previous_endpoint) = previous.source();
            let host_changed = previous_endpoint.host() != endpoint.host();
            let replacing_shared_credentials =
                draft.replacement_credentials.is_some() && !binding.owns_credential();
            if host_changed || replacing_shared_credentials {
                return Err(CameraServiceError::PtzBindingRequiresUnpair);
            }
        }

        let replacing_credentials = draft.replacement_credentials.is_some();
        let credential_ref = if replacing_credentials {
            self.allocate_credential_ref(&camera_id, Some(previous.credential_ref()))?
        } else {
            previous.credential_ref().clone()
        };
        // Validate every ordinary camera field before mutating the credential
        // store, so malformed requests cannot orphan a freshly-written secret.
        let updated = CameraConfig::new(
            camera_id,
            draft.display_name.clone(),
            CameraSource::Rtsp(endpoint),
            draft.audio_policy,
            credential_ref.clone(),
        )
        .map_err(|error| CameraServiceError::Validation(error.to_string()))?;

        let old_ref = replacing_credentials.then(|| previous.credential_ref().clone());
        if let Some(credentials) = &draft.replacement_credentials {
            self.credentials.put(&credential_ref, credentials)?;
        }

        match self.repository.update_camera(&updated) {
            Ok(true) => {}
            Ok(false) => {
                let error = CameraServiceError::CameraNotFound;
                return Err(if replacing_credentials {
                    rollback_new_credential(
                        self.credentials.as_ref(),
                        &credential_ref,
                        "update",
                        error,
                    )
                } else {
                    error
                });
            }
            Err(repository_error) => {
                let error = map_repository_service_error(repository_error);
                return Err(if replacing_credentials {
                    rollback_new_credential(
                        self.credentials.as_ref(),
                        &credential_ref,
                        "update",
                        error,
                    )
                } else {
                    error
                });
            }
        }

        let warning = old_ref.and_then(|old| {
            self.credentials
                .delete(&old)
                .err()
                .map(|_| CameraWarning::OrphanCredentialCleanupFailed)
        });
        Ok(CameraMutation {
            value: CameraSummary::from(&updated),
            warning,
        })
    }

    pub fn delete_camera(
        &mut self,
        camera_id: &str,
        active_camera: Option<&CameraId>,
    ) -> Result<CameraMutation<CameraSummary>, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        if active_camera == Some(&camera_id) {
            return Err(CameraServiceError::CameraBusy);
        }
        let existing = self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let ptz_binding = self
            .repository
            .get_ptz_binding(&camera_id)
            .map_err(map_repository_service_error)?;
        if !self
            .repository
            .delete_camera(&camera_id)
            .map_err(map_repository_service_error)?
        {
            return Err(CameraServiceError::CameraNotFound);
        }
        let mut cleanup_failed = self.credentials.delete(existing.credential_ref()).is_err();
        if let Some(binding) = ptz_binding
            && binding.owns_credential()
            && binding.credential_ref() != existing.credential_ref()
            && self.credentials.delete(binding.credential_ref()).is_err()
        {
            cleanup_failed = true;
        }
        let warning = cleanup_failed.then_some(CameraWarning::OrphanCredentialCleanupFailed);
        Ok(CameraMutation {
            value: CameraSummary::from(&existing),
            warning,
        })
    }

    pub fn application_settings(&self) -> Result<ApplicationSettingsDto, CameraServiceError> {
        self.repository
            .application_settings()
            .map(ApplicationSettingsDto::from)
            .map_err(map_repository_service_error)
    }

    pub fn save_application_settings(
        &mut self,
        dto: ApplicationSettingsDto,
        recording_active: bool,
    ) -> Result<ApplicationSettingsDto, CameraServiceError> {
        let prepared = self.prepare_application_settings(dto, recording_active)?;
        self.commit_application_settings(prepared)
    }

    pub fn prepare_application_settings(
        &self,
        dto: ApplicationSettingsDto,
        recording_active: bool,
    ) -> Result<PreparedApplicationSettings, CameraServiceError> {
        let storage_root = dto.storage_root.as_ref().map(PathBuf::from);
        let retention = RetentionPolicy {
            max_age_days: dto.max_age_days,
            max_storage_bytes: dto.max_storage_bytes,
        };
        retention
            .validate()
            .map_err(|error| CameraServiceError::Validation(error.to_string()))?;
        let quota = match (dto.max_storage_bytes, dto.cleanup_target_bytes) {
            (Some(max_bytes), Some(cleanup_target_bytes)) => {
                let quota = StorageQuota {
                    max_bytes,
                    cleanup_target_bytes,
                };
                quota
                    .validate()
                    .map_err(|error| CameraServiceError::Validation(error.to_string()))?;
                Some(quota)
            }
            (None, None) => None,
            _ => {
                return Err(CameraServiceError::Validation(
                    "max storage bytes and cleanup target bytes must be configured together"
                        .to_owned(),
                ));
            }
        };
        if let Some(root) = &storage_root {
            AppConfig::builder(root)
                .segment_target_duration(std::time::Duration::from_secs(dto.segment_target_secs))?
                .retention(retention)?
                .build()?;
        } else if dto.segment_target_secs < 5 || dto.segment_target_secs > 3600 {
            return Err(CameraServiceError::Validation(
                "segment target duration must be between 5 and 3600 seconds".to_owned(),
            ));
        }
        let settings = ApplicationSettings {
            storage_root,
            segment_target_secs: dto.segment_target_secs,
            retention,
            quota,
            launch_at_login: dto.launch_at_login,
        };
        if recording_active {
            let current = self
                .repository
                .application_settings()
                .map_err(map_repository_service_error)?;
            if settings.storage_root != current.storage_root
                || settings.segment_target_secs != current.segment_target_secs
                || settings.retention != current.retention
                || settings.quota != current.quota
            {
                return Err(CameraServiceError::CameraBusy);
            }
        }
        Ok(PreparedApplicationSettings { settings })
    }

    pub fn commit_application_settings(
        &mut self,
        prepared: PreparedApplicationSettings,
    ) -> Result<ApplicationSettingsDto, CameraServiceError> {
        let settings = prepared.settings;
        self.repository
            .save_application_settings(&settings)
            .map_err(map_repository_service_error)?;
        Ok(settings.into())
    }

    pub fn recording_enabled_cameras(&self) -> Result<Vec<CameraId>, CameraServiceError> {
        self.repository
            .recording_enabled_cameras()
            .map_err(map_repository_service_error)
    }

    pub fn set_all_recording_enabled(&mut self, enabled: bool) -> Result<(), CameraServiceError> {
        self.repository
            .set_all_recording_enabled(enabled)
            .map_err(map_repository_service_error)
    }

    pub fn set_recording_enabled(
        &mut self,
        camera_id: &str,
        enabled: bool,
    ) -> Result<(), CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        if !self
            .repository
            .set_recording_enabled(&camera_id, enabled)
            .map_err(map_repository_service_error)?
        {
            return Err(CameraServiceError::CameraNotFound);
        }
        Ok(())
    }

    pub fn prepare_recording(
        &self,
        camera_id: &str,
    ) -> Result<DesiredRecording, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        let camera = self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let credentials = self.credentials.get(camera.credential_ref())?;
        validate_credentials(&credentials)?;
        let settings = self
            .repository
            .application_settings()
            .map_err(map_repository_service_error)?;
        let storage_root = settings
            .storage_root
            .ok_or(CameraServiceError::StorageNotConfigured)?;
        let config = AppConfig::builder(&storage_root)
            .segment_target_duration(std::time::Duration::from_secs(settings.segment_target_secs))?
            .retention(settings.retention)?
            .build()?;
        let CameraSource::Rtsp(endpoint) = camera.source();
        Ok(DesiredRecording {
            camera: camera_id.as_str().to_owned(),
            storage_root: config.storage_root().to_string_lossy().into_owned(),
            source_json: serde_json::json!({
                "kind": "rtsp",
                "url": endpoint.url_with(Some(&credentials)),
            }),
            segment_target_secs: config.segment_target_duration().get().as_secs(),
            copy_audio: camera.audio_policy() == AudioPolicy::CopyAll,
        })
    }

    pub fn prepare_live(&self, camera_id: &str) -> Result<PreparedLive, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        let camera = self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let credentials = self.credentials.get(camera.credential_ref())?;
        validate_credentials(&credentials)?;
        let CameraSource::Rtsp(endpoint) = camera.source();
        Ok(PreparedLive {
            camera_id,
            source_json: serde_json::json!({
                "kind": "rtsp",
                "url": endpoint.url_with(Some(&credentials)),
            }),
        })
    }

    pub fn prepare_probe(
        &self,
        camera_id: &str,
        timeout_ms: u64,
    ) -> Result<PreparedProbe, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        let camera = self
            .repository
            .get_camera(&camera_id)
            .map_err(map_repository_service_error)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let credentials = self.credentials.get(camera.credential_ref())?;
        validate_credentials(&credentials)?;
        let CameraSource::Rtsp(endpoint) = camera.source();
        prepared_probe(camera_id.as_str(), endpoint, &credentials, timeout_ms)
    }

    /// Prepares a source-only probe from an unsaved/edited form. New cameras
    /// must supply credentials; existing cameras may leave password blank and
    /// reuse the committed secret while testing changed non-secret endpoint
    /// fields. Nothing is persisted by this operation.
    pub fn prepare_probe_draft(
        &self,
        draft: &CameraDraft,
        timeout_ms: u64,
    ) -> Result<PreparedProbe, CameraServiceError> {
        let camera_id = parse_camera_id(&draft.camera_id)?;
        let endpoint = endpoint_from_draft(draft)?;
        let credentials = match &draft.replacement_credentials {
            Some(credentials) => {
                validate_credentials(credentials)?;
                credentials.clone()
            }
            None => {
                let existing = self
                    .repository
                    .get_camera(&camera_id)
                    .map_err(map_repository_service_error)?
                    .ok_or_else(|| {
                        CameraServiceError::Validation(
                            "username/password are required when testing a new camera".to_owned(),
                        )
                    })?;
                let credentials = self.credentials.get(existing.credential_ref())?;
                validate_credentials(&credentials)?;
                credentials
            }
        };
        prepared_probe(camera_id.as_str(), &endpoint, &credentials, timeout_ms)
    }

    fn allocate_credential_ref(
        &self,
        camera_id: &CameraId,
        forbidden: Option<&CredentialRef>,
    ) -> Result<CredentialRef, CameraServiceError> {
        for _ in 0..MAX_CREDENTIAL_REF_GENERATION_ATTEMPTS {
            let candidate = self.credential_refs.generate(camera_id)?;
            if forbidden == Some(&candidate) {
                continue;
            }
            // Native credential identities are mutable keys. Never call put on
            // a candidate that is already occupied, even if a deterministic
            // generator or astronomically unlikely UUID collision produces it.
            if self.credentials.exists(&candidate)? {
                continue;
            }
            return Ok(candidate);
        }
        Err(CameraServiceError::CredentialRefCollision)
    }
}

impl From<crate::ApplicationError> for CameraServiceError {
    fn from(value: crate::ApplicationError) -> Self {
        Self::Validation(value.to_string())
    }
}

fn validate_credentials(credentials: &Credentials) -> Result<(), CameraServiceError> {
    credentials
        .validate()
        .map_err(|error| CameraServiceError::Validation(error.to_string()))
}

fn rollback_new_credential(
    store: &dyn CredentialStore,
    reference: &CredentialRef,
    operation: &'static str,
    original: CameraServiceError,
) -> CameraServiceError {
    if store.delete(reference).is_err() {
        CameraServiceError::CredentialRollbackCleanup { operation }
    } else {
        original
    }
}

fn repository_error(error: SettingsError) -> SettingsRepositoryError {
    match error {
        SettingsError::DuplicateCamera(_) => SettingsRepositoryError::DuplicateCamera,
        SettingsError::Database(_)
        | SettingsError::FutureSchema { .. }
        | SettingsError::InvalidData(_) => SettingsRepositoryError::Persistence,
    }
}

fn map_repository_service_error(error: SettingsRepositoryError) -> CameraServiceError {
    match error {
        SettingsRepositoryError::DuplicateCamera => CameraServiceError::DuplicateCamera,
        SettingsRepositoryError::Persistence => CameraServiceError::Settings,
    }
}

fn prepared_probe(
    camera_id: &str,
    endpoint: &CameraEndpoint,
    credentials: &Credentials,
    timeout_ms: u64,
) -> Result<PreparedProbe, CameraServiceError> {
    if !(100..=60_000).contains(&timeout_ms) {
        return Err(CameraServiceError::Validation(
            "probe timeout must be between 100 and 60000 ms".to_owned(),
        ));
    }
    Ok(PreparedProbe {
        camera_id: camera_id.to_owned(),
        source_json: serde_json::json!({
            "kind": "rtsp",
            "url": endpoint.url_with(Some(credentials)),
        }),
        timeout_ms,
    })
}

fn parse_camera_id(value: &str) -> Result<CameraId, CameraServiceError> {
    CameraId::parse(value).map_err(|error| CameraServiceError::Validation(error.to_string()))
}

fn endpoint_from_draft(draft: &CameraDraft) -> Result<CameraEndpoint, CameraServiceError> {
    let host = Host::parse(&draft.host)
        .map_err(|error| CameraServiceError::Validation(error.to_string()))?;
    CameraEndpoint::new(host, draft.port, &draft.path)
        .map_err(|error| CameraServiceError::Validation(error.to_string()))
}

fn validate_display_name(display_name: &str) -> Result<(), CameraServiceError> {
    CameraConfig::validate_display_name(display_name)
        .map_err(|error| CameraServiceError::Validation(error.to_string()))
}
