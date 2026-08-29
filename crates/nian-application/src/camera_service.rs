//! Camera-management application service and secret-store boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Credentials,
    Host, RetentionPolicy, StorageQuota,
};
use nian_settings::{ApplicationSettings, SettingsStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AppConfig, DesiredRecording};

static NEXT_CREDENTIAL_REF: AtomicU64 = AtomicU64::new(1);

/// Native-secret-store abstraction. Implementations must keep all error text
/// secret-safe: errors may describe an operation but never echo credentials.
pub trait CredentialStore: Send + Sync {
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

/// CI/test credential store that never reaches a graphical/native keychain.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    entries: Mutex<HashMap<String, Credentials>>,
}

impl CredentialStore for MemoryCredentialStore {
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
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, String>;
    fn get_camera(&self, camera_id: &CameraId) -> Result<Option<CameraConfig>, String>;
    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), String>;
    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, String>;
    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, String>;
    fn application_settings(&self) -> Result<ApplicationSettings, String>;
    fn save_application_settings(&mut self, settings: &ApplicationSettings) -> Result<(), String>;
}

impl SettingsRepository for SettingsStore {
    fn list_cameras(&self) -> Result<Vec<CameraConfig>, String> {
        SettingsStore::list_cameras(self).map_err(|error| error.to_string())
    }
    fn get_camera(&self, camera_id: &CameraId) -> Result<Option<CameraConfig>, String> {
        SettingsStore::get_camera(self, camera_id).map_err(|error| error.to_string())
    }
    fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), String> {
        SettingsStore::insert_camera(self, camera).map_err(|error| error.to_string())
    }
    fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, String> {
        SettingsStore::update_camera(self, camera).map_err(|error| error.to_string())
    }
    fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, String> {
        SettingsStore::delete_camera(self, camera_id).map_err(|error| error.to_string())
    }
    fn application_settings(&self) -> Result<ApplicationSettings, String> {
        SettingsStore::application_settings(self).map_err(|error| error.to_string())
    }
    fn save_application_settings(&mut self, settings: &ApplicationSettings) -> Result<(), String> {
        SettingsStore::save_application_settings(self, settings).map_err(|error| error.to_string())
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
    #[error("camera is actively recording")]
    CameraBusy,
    #[error("credential store unavailable")]
    CredentialStore(#[from] CredentialStoreError),
    #[error("settings persistence failed")]
    Settings,
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
        Self {
            repository,
            credentials,
        }
    }

    pub fn list_cameras(&self) -> Result<Vec<CameraSummary>, CameraServiceError> {
        self.repository
            .list_cameras()
            .map_err(|_| CameraServiceError::Settings)
            .map(|rows| rows.iter().map(CameraSummary::from).collect())
    }

    pub fn get_camera(&self, camera_id: &str) -> Result<CameraSummary, CameraServiceError> {
        let id = parse_camera_id(camera_id)?;
        self.repository
            .get_camera(&id)
            .map_err(|_| CameraServiceError::Settings)?
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
        if credentials.username.trim().is_empty() {
            return Err(CameraServiceError::Validation(
                "username must not be empty".to_owned(),
            ));
        }
        let camera_id = parse_camera_id(&draft.camera_id)?;
        let credential_ref = next_credential_ref(&camera_id)?;
        let config = config_from_draft(&draft, camera_id, credential_ref.clone())?;

        self.credentials.put(&credential_ref, &credentials)?;
        if self.repository.insert_camera(&config).is_err() {
            let _ = self.credentials.delete(&credential_ref);
            return Err(CameraServiceError::Settings);
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
            .map_err(|_| CameraServiceError::Settings)?
            .ok_or(CameraServiceError::CameraNotFound)?;

        let endpoint = endpoint_from_draft(&draft)?;
        let critical_change = previous.source() != &CameraSource::Rtsp(endpoint.clone())
            || previous.audio_policy() != draft.audio_policy
            || draft.replacement_credentials.is_some();
        if active_camera == Some(&camera_id) && critical_change {
            return Err(CameraServiceError::CameraBusy);
        }

        let (credential_ref, old_ref, wrote_new_secret) =
            if let Some(credentials) = &draft.replacement_credentials {
                if credentials.username.trim().is_empty() {
                    return Err(CameraServiceError::Validation(
                        "username must not be empty".to_owned(),
                    ));
                }
                let new_ref = next_credential_ref(&camera_id)?;
                self.credentials.put(&new_ref, credentials)?;
                (new_ref, Some(previous.credential_ref().clone()), true)
            } else {
                (previous.credential_ref().clone(), None, false)
            };

        let updated = CameraConfig::new(
            camera_id.clone(),
            draft.display_name,
            CameraSource::Rtsp(endpoint),
            draft.audio_policy,
            credential_ref.clone(),
        )
        .map_err(|error| CameraServiceError::Validation(error.to_string()))?;

        match self.repository.update_camera(&updated) {
            Ok(true) => {}
            Ok(false) => {
                if wrote_new_secret {
                    let _ = self.credentials.delete(&credential_ref);
                }
                return Err(CameraServiceError::CameraNotFound);
            }
            Err(_) => {
                if wrote_new_secret {
                    let _ = self.credentials.delete(&credential_ref);
                }
                return Err(CameraServiceError::Settings);
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
            .map_err(|_| CameraServiceError::Settings)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        if !self
            .repository
            .delete_camera(&camera_id)
            .map_err(|_| CameraServiceError::Settings)?
        {
            return Err(CameraServiceError::CameraNotFound);
        }
        let warning = self
            .credentials
            .delete(existing.credential_ref())
            .err()
            .map(|_| CameraWarning::OrphanCredentialCleanupFailed);
        Ok(CameraMutation {
            value: CameraSummary::from(&existing),
            warning,
        })
    }

    pub fn application_settings(&self) -> Result<ApplicationSettingsDto, CameraServiceError> {
        self.repository
            .application_settings()
            .map(ApplicationSettingsDto::from)
            .map_err(|_| CameraServiceError::Settings)
    }

    pub fn save_application_settings(
        &mut self,
        dto: ApplicationSettingsDto,
        recording_active: bool,
    ) -> Result<ApplicationSettingsDto, CameraServiceError> {
        if recording_active {
            return Err(CameraServiceError::CameraBusy);
        }
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
        };
        self.repository
            .save_application_settings(&settings)
            .map_err(|_| CameraServiceError::Settings)?;
        Ok(settings.into())
    }

    pub fn prepare_recording(
        &self,
        camera_id: &str,
    ) -> Result<DesiredRecording, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        let camera = self
            .repository
            .get_camera(&camera_id)
            .map_err(|_| CameraServiceError::Settings)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let credentials = self.credentials.get(camera.credential_ref())?;
        let settings = self
            .repository
            .application_settings()
            .map_err(|_| CameraServiceError::Settings)?;
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

    pub fn prepare_probe(
        &self,
        camera_id: &str,
        timeout_ms: u64,
    ) -> Result<PreparedProbe, CameraServiceError> {
        let camera_id = parse_camera_id(camera_id)?;
        let camera = self
            .repository
            .get_camera(&camera_id)
            .map_err(|_| CameraServiceError::Settings)?
            .ok_or(CameraServiceError::CameraNotFound)?;
        let credentials = self.credentials.get(camera.credential_ref())?;
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
                if credentials.username.trim().is_empty() {
                    return Err(CameraServiceError::Validation(
                        "username must not be empty when testing replacement credentials"
                            .to_owned(),
                    ));
                }
                credentials.clone()
            }
            None => {
                let existing = self
                    .repository
                    .get_camera(&camera_id)
                    .map_err(|_| CameraServiceError::Settings)?
                    .ok_or_else(|| {
                        CameraServiceError::Validation(
                            "username/password are required when testing a new camera".to_owned(),
                        )
                    })?;
                self.credentials.get(existing.credential_ref())?
            }
        };
        prepared_probe(camera_id.as_str(), &endpoint, &credentials, timeout_ms)
    }
}

impl From<crate::ApplicationError> for CameraServiceError {
    fn from(value: crate::ApplicationError) -> Self {
        Self::Validation(value.to_string())
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

fn config_from_draft(
    draft: &CameraDraft,
    camera_id: CameraId,
    credential_ref: CredentialRef,
) -> Result<CameraConfig, CameraServiceError> {
    CameraConfig::new(
        camera_id,
        draft.display_name.clone(),
        CameraSource::Rtsp(endpoint_from_draft(draft)?),
        draft.audio_policy,
        credential_ref,
    )
    .map_err(|error| CameraServiceError::Validation(error.to_string()))
}

fn next_credential_ref(camera_id: &CameraId) -> Result<CredentialRef, CameraServiceError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_CREDENTIAL_REF.fetch_add(1, Ordering::Relaxed);
    CredentialRef::parse(format!(
        "nian-vision/{}/{nanos:x}-{counter:x}",
        camera_id.as_str()
    ))
    .map_err(|error| CameraServiceError::Validation(error.to_string()))
}
