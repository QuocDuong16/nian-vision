use std::ffi::OsString;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use nian_domain::CameraId;
use rusqlite::{Connection, params};

use crate::{BUSY_TIMEOUT, IndexError, enable_and_verify_wal, verify_foreign_keys};

const EVENT_SCHEMA_VERSION: i32 = 1;
pub const DEFAULT_EVENT_RETENTION_DAYS: u32 = 30;
pub const MAX_EVENT_ROWS: u64 = 250_000;
pub const EVENT_CLEANUP_BATCH: u32 = 500;
pub const MAX_RECENT_EVENTS: u32 = 100;
const MAX_SOURCE_KEY_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    MotionStarted,
    MotionEnded,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::MotionStarted => "motion_started",
            Self::MotionEnded => "motion_ended",
        }
    }

    fn parse(value: &str) -> Result<Self, IndexError> {
        match value {
            "motion_started" => Ok(Self::MotionStarted),
            "motion_ended" => Ok(Self::MotionEnded),
            _ => Err(IndexError::InvalidData("unknown event kind".to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInsert {
    pub camera_id: CameraId,
    pub kind: EventKind,
    pub source_key: Option<String>,
    pub device_time_utc: Option<DateTime<Utc>>,
    pub received_time_utc: DateTime<Utc>,
    pub fingerprint: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub event_id: u64,
    pub camera_id: CameraId,
    pub kind: EventKind,
    pub source_key: Option<String>,
    pub device_time_utc: Option<DateTime<Utc>>,
    pub received_time_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCleanupReport {
    pub age_deleted: u32,
    pub cap_deleted: u32,
}

#[derive(Debug)]
pub struct EventIndex {
    path: PathBuf,
    connection: Connection,
}

impl EventIndex {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, IndexError> {
        let path = path.into();
        let connection = Connection::open(&path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // Inspect schema compatibility before any persisted journal-mode change.
        // A future-schema file must remain authoritative and untouched.
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > EVENT_SCHEMA_VERSION {
            return Err(IndexError::FutureSchema {
                found: version,
                supported: EVENT_SCHEMA_VERSION,
            });
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        verify_foreign_keys(&connection)?;
        enable_and_verify_wal(&connection)?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        if version == 0 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;\
                 CREATE TABLE events (\
                    event_id INTEGER PRIMARY KEY AUTOINCREMENT,\
                    camera_id TEXT NOT NULL,\
                    kind TEXT NOT NULL CHECK(kind IN ('motion_started','motion_ended')),\
                    source_key TEXT NULL,\
                    device_time_utc_ms INTEGER NULL,\
                    received_time_utc_ms INTEGER NOT NULL,\
                    fingerprint BLOB NULL UNIQUE\
                 );\
                 CREATE INDEX events_camera_received \
                    ON events(camera_id, received_time_utc_ms DESC, event_id DESC);\
                 CREATE INDEX events_received \
                    ON events(received_time_utc_ms, event_id);\
                 PRAGMA user_version=1;\
                 COMMIT;",
            )?;
        }
        Ok(Self { path, connection })
    }

    /// Opens the rebuildable Event index and quarantines a corrupt SQLite
    /// family before creating a fresh index at the authoritative path.
    /// Future schemas and ordinary I/O failures remain explicit errors.
    pub fn open_with_recovery(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, Option<PathBuf>), IndexError> {
        let path = path.into();
        match Self::open(path.clone()) {
            Ok(index) => Ok((index, None)),
            Err(error) if error.is_corruption() => {
                let quarantined = quarantine_corrupt_sqlite_family(&path)?;
                Self::open(path).map(|index| (index, Some(quarantined)))
            }
            Err(error) => Err(error),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i32, IndexError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn insert(&mut self, event: &EventInsert) -> Result<Option<u64>, IndexError> {
        if event
            .source_key
            .as_ref()
            .is_some_and(|key| key.len() > MAX_SOURCE_KEY_BYTES || !key.is_ascii())
        {
            return Err(IndexError::InvalidData(
                "invalid event source key".to_owned(),
            ));
        }
        let device_ms = event.device_time_utc.map(|value| value.timestamp_millis());
        let received_ms = event.received_time_utc.timestamp_millis();
        let fingerprint = event.fingerprint.map(|value| value.to_vec());
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO events \
             (camera_id, kind, source_key, device_time_utc_ms, received_time_utc_ms, fingerprint) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.camera_id.as_str(),
                event.kind.as_str(),
                event.source_key,
                device_ms,
                received_ms,
                fingerprint,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let id = u64::try_from(self.connection.last_insert_rowid())
            .map_err(|_| IndexError::InvalidData("invalid event id".to_owned()))?;
        Ok(Some(id))
    }

    pub fn recent(&self, camera_id: &CameraId, limit: u32) -> Result<Vec<EventRecord>, IndexError> {
        let limit = limit.clamp(1, MAX_RECENT_EVENTS);
        let mut statement = self.connection.prepare(
            "SELECT event_id, camera_id, kind, source_key, device_time_utc_ms, received_time_utc_ms \
             FROM events WHERE camera_id=?1 \
             ORDER BY received_time_utc_ms DESC, event_id DESC LIMIT ?2",
        )?;
        let raws = statement
            .query_map(params![camera_id.as_str(), i64::from(limit)], |row| {
                Ok(RawEventRecord {
                    event_id: row.get(0)?,
                    camera_id: row.get(1)?,
                    kind: row.get(2)?,
                    source_key: row.get(3)?,
                    device_time_utc_ms: row.get(4)?,
                    received_time_utc_ms: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(raw_to_event).collect()
    }

    pub fn count(&self) -> Result<u64, IndexError> {
        let value: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        u64::try_from(value).map_err(|_| IndexError::InvalidData("invalid event count".to_owned()))
    }

    pub fn cleanup(
        &mut self,
        now: DateTime<Utc>,
        retention_days: Option<u32>,
    ) -> Result<EventCleanupReport, IndexError> {
        let days = retention_days
            .unwrap_or(DEFAULT_EVENT_RETENTION_DAYS)
            .max(1);
        let cutoff = now
            .checked_sub_signed(Duration::days(i64::from(days)))
            .ok_or_else(|| IndexError::InvalidData("invalid event retention cutoff".to_owned()))?
            .timestamp_millis();
        let transaction = self.connection.transaction()?;
        let age_deleted = transaction.execute(
            "DELETE FROM events WHERE event_id IN (\
                 SELECT event_id FROM events WHERE received_time_utc_ms < ?1 \
                 ORDER BY received_time_utc_ms, event_id LIMIT ?2\
             )",
            params![cutoff, i64::from(EVENT_CLEANUP_BATCH)],
        )?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        let excess = u64::try_from(count)
            .map_err(|_| IndexError::InvalidData("invalid event count".to_owned()))?
            .saturating_sub(MAX_EVENT_ROWS);
        let cap_batch = excess.min(u64::from(EVENT_CLEANUP_BATCH));
        let cap_deleted = if cap_batch == 0 {
            0
        } else {
            transaction.execute(
                "DELETE FROM events WHERE event_id IN (\
                     SELECT event_id FROM events ORDER BY received_time_utc_ms, event_id LIMIT ?1\
                 )",
                [i64::try_from(cap_batch)
                    .map_err(|_| IndexError::InvalidData("invalid cleanup batch".to_owned()))?],
            )?
        };
        transaction.commit()?;
        Ok(EventCleanupReport {
            age_deleted: u32::try_from(age_deleted)
                .map_err(|_| IndexError::InvalidData("cleanup count overflow".to_owned()))?,
            cap_deleted: u32::try_from(cap_deleted)
                .map_err(|_| IndexError::InvalidData("cleanup count overflow".to_owned()))?,
        })
    }
}

fn quarantine_corrupt_sqlite_family(path: &Path) -> Result<PathBuf, IndexError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| IndexError::InvalidData("event index path has no filename".to_owned()))?;
    let stamp = Utc::now().timestamp_millis();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    for attempt in 0_u16..100 {
        let mut candidate_name = OsString::from(file_name);
        candidate_name.push(format!(".corrupt-{stamp}-{attempt}"));
        let candidate = parent.join(candidate_name);
        if candidate.exists() || sqlite_sidecar(&candidate, "-wal").exists() {
            continue;
        }

        // Move sidecars first. If the main-file move then fails, the original
        // corrupt DB remains authoritative and a later recovery can retry; a
        // fresh DB is never opened beside stale WAL/SHM state.
        for suffix in ["-wal", "-shm"] {
            let source = sqlite_sidecar(path, suffix);
            if source.exists() {
                std::fs::rename(&source, sqlite_sidecar(&candidate, suffix))?;
            }
        }
        std::fs::rename(path, &candidate)?;
        return Ok(candidate);
    }

    Err(IndexError::InvalidData(
        "could not allocate event-index quarantine path".to_owned(),
    ))
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[derive(Debug)]
struct RawEventRecord {
    event_id: i64,
    camera_id: String,
    kind: String,
    source_key: Option<String>,
    device_time_utc_ms: Option<i64>,
    received_time_utc_ms: i64,
}

fn raw_to_event(raw: RawEventRecord) -> Result<EventRecord, IndexError> {
    let event_id = u64::try_from(raw.event_id)
        .map_err(|_| IndexError::InvalidData("invalid event id".to_owned()))?;
    let camera_id = CameraId::parse(raw.camera_id)
        .map_err(|error| IndexError::InvalidData(error.to_string()))?;
    if raw
        .source_key
        .as_ref()
        .is_some_and(|key| key.len() > MAX_SOURCE_KEY_BYTES || !key.is_ascii())
    {
        return Err(IndexError::InvalidData(
            "invalid event source key".to_owned(),
        ));
    }
    let device_time_utc = raw.device_time_utc_ms.map(parse_timestamp).transpose()?;
    Ok(EventRecord {
        event_id,
        camera_id,
        kind: EventKind::parse(&raw.kind)?,
        source_key: raw.source_key,
        device_time_utc,
        received_time_utc: parse_timestamp(raw.received_time_utc_ms)?,
    })
}

fn parse_timestamp(value: i64) -> Result<DateTime<Utc>, IndexError> {
    DateTime::from_timestamp_millis(value)
        .ok_or_else(|| IndexError::InvalidData("invalid event timestamp".to_owned()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn event(camera: &str, active: bool, second: i64) -> EventInsert {
        EventInsert {
            camera_id: CameraId::parse(camera).unwrap(),
            kind: if active {
                EventKind::MotionStarted
            } else {
                EventKind::MotionEnded
            },
            source_key: Some("aabbcc".to_owned()),
            device_time_utc: DateTime::from_timestamp(second, 0),
            received_time_utc: DateTime::from_timestamp(second + 1, 0).unwrap(),
            fingerprint: Some([u8::from(active); 32]),
        }
    }

    #[test]
    fn schema_insert_query_and_duplicate_fingerprint_are_stable() {
        let temp = tempdir().unwrap();
        let mut index = EventIndex::open(temp.path().join("event-index.sqlite3")).unwrap();
        assert_eq!(index.schema_version().unwrap(), EVENT_SCHEMA_VERSION);
        assert!(
            index
                .insert(&event("front-door", true, 10))
                .unwrap()
                .is_some()
        );
        assert!(
            index
                .insert(&event("front-door", true, 10))
                .unwrap()
                .is_none()
        );
        let rows = index
            .recent(&CameraId::parse("front-door").unwrap(), 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EventKind::MotionStarted);
    }

    #[test]
    fn cleanup_is_bounded_and_age_indexed() {
        let temp = tempdir().unwrap();
        let mut index = EventIndex::open(temp.path().join("event-index.sqlite3")).unwrap();
        for second in 0..600_i64 {
            let mut row = event("front-door", second % 2 == 0, second);
            row.fingerprint = None;
            index.insert(&row).unwrap();
        }
        let now = DateTime::from_timestamp(90 * 86_400, 0).unwrap();
        let first = index.cleanup(now, Some(1)).unwrap();
        assert_eq!(first.age_deleted, EVENT_CLEANUP_BATCH);
        assert_eq!(index.count().unwrap(), 100);
    }

    #[test]
    fn corrupt_event_index_is_quarantined_and_recreated_without_deleting_evidence() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.sqlite3");
        let corrupt = b"not-a-sqlite-event-index\0keep-evidence";
        std::fs::write(&path, corrupt).unwrap();

        let (index, quarantined) = EventIndex::open_with_recovery(&path).unwrap();
        let quarantined = quarantined.expect("corruption must be quarantined");

        assert_eq!(index.schema_version().unwrap(), EVENT_SCHEMA_VERSION);
        assert_eq!(std::fs::read(&quarantined).unwrap(), corrupt);
        assert!(path.is_file());
    }

    #[test]
    fn quarantine_moves_existing_sqlite_sidecars_without_deleting_them() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.sqlite3");
        std::fs::write(&path, b"main-evidence").unwrap();
        std::fs::write(sqlite_sidecar(&path, "-wal"), b"wal-evidence").unwrap();
        std::fs::write(sqlite_sidecar(&path, "-shm"), b"shm-evidence").unwrap();

        let quarantined = quarantine_corrupt_sqlite_family(&path).unwrap();

        assert_eq!(std::fs::read(&quarantined).unwrap(), b"main-evidence");
        assert_eq!(
            std::fs::read(sqlite_sidecar(&quarantined, "-wal")).unwrap(),
            b"wal-evidence"
        );
        assert_eq!(
            std::fs::read(sqlite_sidecar(&quarantined, "-shm")).unwrap(),
            b"shm-evidence"
        );
        assert!(!path.exists());
        assert!(!sqlite_sidecar(&path, "-wal").exists());
        assert!(!sqlite_sidecar(&path, "-shm").exists());
    }

    #[test]
    fn future_event_schema_is_not_quarantined_or_replaced() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("events.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection.pragma_update(None, "user_version", 99).unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let error = EventIndex::open_with_recovery(&path).unwrap_err();
        assert!(matches!(
            error,
            IndexError::FutureSchema {
                found: 99,
                supported: EVENT_SCHEMA_VERSION
            }
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
                .count(),
            0
        );
    }
}
