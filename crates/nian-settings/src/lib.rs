//! Authoritative desktop settings persistence.
//!
//! This database is user configuration, not a rebuildable media index. It is
//! intentionally FFmpeg-free and Tauri-free; the desktop host resolves the
//! platform app-data path and passes an absolute database path here.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Host,
    RetentionPolicy, StorageQuota,
};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

const SCHEMA_VERSION: i32 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("settings database schema {found} is newer than supported schema {supported}")]
    FutureSchema { found: i32, supported: i32 },
    #[error("invalid persisted settings: {0}")]
    InvalidData(String),
    #[error("camera already exists: {0}")]
    DuplicateCamera(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationSettings {
    pub storage_root: Option<PathBuf>,
    pub segment_target_secs: u64,
    pub retention: RetentionPolicy,
    pub quota: Option<StorageQuota>,
}

impl Default for ApplicationSettings {
    fn default() -> Self {
        Self {
            storage_root: None,
            segment_target_secs: 300,
            retention: RetentionPolicy::default(),
            quota: None,
        }
    }
}

#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
    connection: Connection,
}

impl SettingsStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(SettingsError::InvalidData(
                "settings database path must be absolute".to_owned(),
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SettingsError::InvalidData(format!("cannot create settings directory: {error}"))
            })?;
        }
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(SettingsError::FutureSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        migrate(&mut connection)?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i32, SettingsError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn list_cameras(&self) -> Result<Vec<CameraConfig>, SettingsError> {
        let mut statement = self.connection.prepare(
            "SELECT camera_id, display_name, host, port, rtsp_path, audio_policy, credential_ref \
             FROM cameras ORDER BY display_name COLLATE NOCASE, camera_id",
        )?;
        let raws = statement
            .query_map([], |row| {
                Ok(RawCamera {
                    camera_id: row.get(0)?,
                    display_name: row.get(1)?,
                    host: row.get(2)?,
                    port: row.get(3)?,
                    rtsp_path: row.get(4)?,
                    audio_policy: row.get(5)?,
                    credential_ref: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(raw_to_camera).collect()
    }

    pub fn get_camera(&self, camera_id: &CameraId) -> Result<Option<CameraConfig>, SettingsError> {
        let raw = self.connection.query_row(
            "SELECT camera_id, display_name, host, port, rtsp_path, audio_policy, credential_ref \
             FROM cameras WHERE camera_id=?1",
            [camera_id.as_str()],
            |row| {
                Ok(RawCamera {
                    camera_id: row.get(0)?,
                    display_name: row.get(1)?,
                    host: row.get(2)?,
                    port: row.get(3)?,
                    rtsp_path: row.get(4)?,
                    audio_policy: row.get(5)?,
                    credential_ref: row.get(6)?,
                })
            },
        ).optional()?;
        raw.map(raw_to_camera).transpose()
    }

    pub fn insert_camera(&mut self, camera: &CameraConfig) -> Result<(), SettingsError> {
        let row = camera_row(camera)?;
        let result = self.connection.execute(
            "INSERT INTO cameras \
             (camera_id, display_name, host, port, rtsp_path, audio_policy, credential_ref) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.camera_id,
                row.display_name,
                row.host,
                row.port,
                row.rtsp_path,
                row.audio_policy,
                row.credential_ref
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(error) if is_unique_constraint(&error) => Err(SettingsError::DuplicateCamera(
                camera.camera_id().to_string(),
            )),
            Err(error) => Err(error.into()),
        }
    }

    pub fn update_camera(&mut self, camera: &CameraConfig) -> Result<bool, SettingsError> {
        let row = camera_row(camera)?;
        Ok(self.connection.execute(
            "UPDATE cameras SET display_name=?2, host=?3, port=?4, rtsp_path=?5, \
             audio_policy=?6, credential_ref=?7 WHERE camera_id=?1",
            params![
                row.camera_id,
                row.display_name,
                row.host,
                row.port,
                row.rtsp_path,
                row.audio_policy,
                row.credential_ref
            ],
        )? > 0)
    }

    pub fn delete_camera(&mut self, camera_id: &CameraId) -> Result<bool, SettingsError> {
        Ok(self.connection.execute(
            "DELETE FROM cameras WHERE camera_id=?1",
            [camera_id.as_str()],
        )? > 0)
    }

    pub fn application_settings(&self) -> Result<ApplicationSettings, SettingsError> {
        self.connection
            .query_row(
                "SELECT storage_root, segment_target_secs, max_age_days, max_storage_bytes, cleanup_target_bytes \
             FROM application_settings WHERE singleton_id=1",
                [],
                |row| {
                    let storage_root: Option<String> = row.get(0)?;
                    let segment: i64 = row.get(1)?;
                    let max_age: Option<i64> = row.get(2)?;
                    let max_bytes: Option<i64> = row.get(3)?;
                    let cleanup_target: Option<i64> = row.get(4)?;
                    Ok((storage_root, segment, max_age, max_bytes, cleanup_target))
                },
            )
            .map_err(SettingsError::from)
            .and_then(|(root, segment, max_age, max_bytes, cleanup_target)| {
                let settings = ApplicationSettings {
                    storage_root: root.map(PathBuf::from),
                    segment_target_secs: u64::try_from(segment).map_err(|_| {
                        SettingsError::InvalidData("negative segment target".to_owned())
                    })?,
                    retention: RetentionPolicy {
                        max_age_days: max_age
                            .map(|v| {
                                u32::try_from(v).map_err(|_| {
                                    SettingsError::InvalidData("invalid max_age_days".to_owned())
                                })
                            })
                            .transpose()?,
                        max_storage_bytes: max_bytes
                            .map(|v| {
                                u64::try_from(v).map_err(|_| {
                                    SettingsError::InvalidData(
                                        "invalid max_storage_bytes".to_owned(),
                                    )
                                })
                            })
                            .transpose()?,
                    },
                    quota: match (max_bytes, cleanup_target) {
                        (Some(max), Some(target)) => Some(StorageQuota {
                            max_bytes: u64::try_from(max).map_err(|_| {
                                SettingsError::InvalidData("invalid max_storage_bytes".to_owned())
                            })?,
                            cleanup_target_bytes: u64::try_from(target).map_err(|_| {
                                SettingsError::InvalidData("invalid cleanup_target_bytes".to_owned())
                            })?,
                        }),
                        (None, None) => None,
                        _ => return Err(SettingsError::InvalidData("incomplete storage quota".to_owned())),
                    },
                };
                validate_application_settings(&settings)?;
                Ok(settings)
            })
    }

    pub fn save_application_settings(
        &mut self,
        settings: &ApplicationSettings,
    ) -> Result<(), SettingsError> {
        validate_application_settings(settings)?;
        let segment = i64::try_from(settings.segment_target_secs)
            .map_err(|_| SettingsError::InvalidData("segment target too large".to_owned()))?;
        let max_age = settings.retention.max_age_days.map(i64::from);
        let max_bytes = settings
            .retention
            .max_storage_bytes
            .map(|v| {
                i64::try_from(v).map_err(|_| {
                    SettingsError::InvalidData("retention byte limit too large".to_owned())
                })
            })
            .transpose()?;
        let cleanup_target = settings
            .quota
            .map(|quota| {
                i64::try_from(quota.cleanup_target_bytes)
                    .map_err(|_| SettingsError::InvalidData("cleanup target too large".to_owned()))
            })
            .transpose()?;
        let affected = self.connection.execute(
            "UPDATE application_settings SET storage_root=?1, segment_target_secs=?2, \
             max_age_days=?3, max_storage_bytes=?4, cleanup_target_bytes=?5 WHERE singleton_id=1",
            params![
                settings
                    .storage_root
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                segment,
                max_age,
                max_bytes,
                cleanup_target
            ],
        )?;
        if affected != 1 {
            return Err(SettingsError::InvalidData(format!(
                "application_settings singleton update affected {affected} rows"
            )));
        }
        Ok(())
    }
}

fn validate_application_settings(settings: &ApplicationSettings) -> Result<(), SettingsError> {
    settings.retention.validate().map_err(invalid_domain)?;
    match settings.quota {
        Some(quota) => {
            quota.validate().map_err(invalid_domain)?;
            if settings.retention.max_storage_bytes != Some(quota.max_bytes) {
                return Err(SettingsError::InvalidData(
                    "storage quota max_bytes must match retention max_storage_bytes".to_owned(),
                ));
            }
        }
        None if settings.retention.max_storage_bytes.is_some() => {
            return Err(SettingsError::InvalidData(
                "max_storage_bytes requires a cleanup target".to_owned(),
            ));
        }
        None => {}
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), SettingsError> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE cameras (\
                camera_id TEXT PRIMARY KEY NOT NULL,\
                display_name TEXT NOT NULL,\
                host TEXT NOT NULL,\
                port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),\
                rtsp_path TEXT NOT NULL,\
                audio_policy TEXT NOT NULL CHECK(audio_policy IN ('copy_all','exclude')),\
                credential_ref TEXT NOT NULL UNIQUE\
             );\
             CREATE TABLE application_settings (\
                singleton_id INTEGER PRIMARY KEY CHECK(singleton_id=1),\
                storage_root TEXT NULL,\
                segment_target_secs INTEGER NOT NULL,\
                max_age_days INTEGER NULL,\
                max_storage_bytes INTEGER NULL,\
                cleanup_target_bytes INTEGER NULL\
             );\
             INSERT INTO application_settings \
                (singleton_id, storage_root, segment_target_secs, max_age_days, max_storage_bytes, cleanup_target_bytes) \
                VALUES (1, NULL, 300, NULL, NULL, NULL);\
             PRAGMA user_version=1;",
        )?;
        transaction.commit()?;
    }
    Ok(())
}

#[derive(Debug)]
struct RawCamera {
    camera_id: String,
    display_name: String,
    host: String,
    port: i64,
    rtsp_path: String,
    audio_policy: String,
    credential_ref: String,
}

fn raw_to_camera(raw: RawCamera) -> Result<CameraConfig, SettingsError> {
    let camera_id = CameraId::parse(&raw.camera_id).map_err(invalid_domain)?;
    let host = Host::parse(&raw.host).map_err(invalid_domain)?;
    let port = u16::try_from(raw.port)
        .map_err(|_| SettingsError::InvalidData("invalid camera port".to_owned()))?;
    let endpoint = CameraEndpoint::new(host, port, &raw.rtsp_path).map_err(invalid_domain)?;
    let audio = AudioPolicy::from_wire(&raw.audio_policy)
        .ok_or_else(|| SettingsError::InvalidData("invalid audio_policy".to_owned()))?;
    let credential_ref = CredentialRef::parse(raw.credential_ref).map_err(invalid_domain)?;
    CameraConfig::new(
        camera_id,
        raw.display_name,
        CameraSource::Rtsp(endpoint),
        audio,
        credential_ref,
    )
    .map_err(invalid_domain)
}

fn camera_row(camera: &CameraConfig) -> Result<RawCamera, SettingsError> {
    let CameraSource::Rtsp(endpoint) = camera.source();
    Ok(RawCamera {
        camera_id: camera.camera_id().as_str().to_owned(),
        display_name: camera.display_name().to_owned(),
        host: endpoint.host().as_str().to_owned(),
        port: i64::from(endpoint.port()),
        rtsp_path: endpoint.path().to_owned(),
        audio_policy: camera.audio_policy().as_str().to_owned(),
        credential_ref: camera.credential_ref().as_str().to_owned(),
    })
}

fn invalid_domain(error: nian_domain::DomainError) -> SettingsError {
    SettingsError::InvalidData(error.to_string())
}

fn is_unique_constraint(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == 1555 || code.extended_code == 2067)
}
