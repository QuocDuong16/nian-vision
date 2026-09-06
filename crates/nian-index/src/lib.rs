//! Rebuildable SQLite recording index.
//!
//! Filesystem media is authoritative. This crate stores query metadata only
//! and never owns recording publication or deletion semantics.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod events;

pub use events::*;

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use nian_domain::{CameraId, RecordingId, RecordingState};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

pub const SCHEMA_VERSION: i32 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

#[cfg(any(test, feature = "test-hooks"))]
static FAIL_NEXT_REMOVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(any(test, feature = "test-hooks"))]
static FAIL_NEXT_LIST_ALL_CORRUPTION: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);
#[cfg(any(test, feature = "test-hooks"))]
static FAIL_NEXT_LIST_ALL_ERROR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-hooks"))]
pub mod test_hooks {
    use std::sync::atomic::Ordering;

    /// Makes the next index-row removal fail before touching SQLite.
    pub fn fail_next_remove() {
        super::FAIL_NEXT_REMOVE.store(true, Ordering::SeqCst);
    }

    pub fn fail_next_list_all_with_corruption(path: &std::path::Path) {
        let mut armed = match super::FAIL_NEXT_LIST_ALL_CORRUPTION.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *armed = Some(path.to_path_buf());
    }

    pub fn fail_next_list_all(path: &std::path::Path) {
        let mut armed = match super::FAIL_NEXT_LIST_ALL_ERROR.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *armed = Some(path.to_path_buf());
    }

    pub(crate) fn take_remove_failure() -> bool {
        super::FAIL_NEXT_REMOVE.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn take_list_all_corruption(path: &std::path::Path) -> bool {
        let mut armed = match super::FAIL_NEXT_LIST_ALL_CORRUPTION.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if armed.as_deref() != Some(path) {
            return false;
        }
        *armed = None;
        true
    }

    pub(crate) fn take_list_all_error(path: &std::path::Path) -> bool {
        let mut armed = match super::FAIL_NEXT_LIST_ALL_ERROR.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if armed.as_deref() != Some(path) {
            return false;
        }
        *armed = None;
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("index schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: i32, supported: i32 },
    #[error("SQLite WAL journal mode is unavailable (actual mode: {mode})")]
    WalUnavailable { mode: String },
    #[error("SQLite foreign_keys pragma could not be enabled")]
    ForeignKeysUnavailable,
    #[cfg(any(test, feature = "test-hooks"))]
    #[error("injected SQLite corruption")]
    InjectedCorruption,
    #[cfg(any(test, feature = "test-hooks"))]
    #[error("injected SQLite operation failure")]
    InjectedFailure,
    #[error("invalid indexed data: {0}")]
    InvalidData(String),
}

impl IndexError {
    pub fn is_corruption(&self) -> bool {
        match self {
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => {
                matches!(
                    error.code,
                    ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                )
            }
            #[cfg(any(test, feature = "test-hooks"))]
            Self::InjectedCorruption => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingKind {
    Normal,
    Recovered,
}

impl RecordingKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Recovered => "recovered",
        }
    }

    fn parse(value: &str) -> Result<Self, IndexError> {
        match value {
            "normal" => Ok(Self::Normal),
            "recovered" => Ok(Self::Recovered),
            _ => Err(IndexError::InvalidData(format!(
                "unknown recording kind {value:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingUpsert {
    pub camera_id: CameraId,
    pub relative_path: String,
    pub kind: RecordingKind,
    pub state: RecordingState,
    /// Local naive wall-clock time encoded by the canonical filesystem identity.
    pub started_at: NaiveDateTime,
    pub sequence: u32,
    pub size_bytes: u64,
    pub media_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRecording {
    pub id: RecordingId,
    pub camera_id: CameraId,
    pub relative_path: String,
    pub kind: RecordingKind,
    pub state: RecordingState,
    pub started_at: NaiveDateTime,
    pub sequence: u32,
    pub size_bytes: u64,
    pub media_duration_ms: Option<u64>,
}

#[derive(Debug)]
pub struct RecordingIndex {
    path: PathBuf,
    connection: Connection,
}

impl RecordingIndex {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, IndexError> {
        let path = path.into();
        let connection = Connection::open(&path)?;
        reject_future_schema(&connection)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        verify_foreign_keys(&connection)?;
        enable_and_verify_wal(&connection)?;
        // Rebuildable cache: NORMAL is sufficient; media files remain authoritative.
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&connection)?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i32, IndexError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn upsert(&mut self, recording: &RecordingUpsert) -> Result<bool, IndexError> {
        let duration = recording
            .media_duration_ms
            .map(|value| checked_i64(value, "media_duration_ms"))
            .transpose()?;
        let changed = self.connection.execute(
            "INSERT INTO recordings (
                camera_id, relative_path, kind, state, started_at, sequence,
                size_bytes, media_duration_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(relative_path) DO UPDATE SET
                camera_id=excluded.camera_id,
                kind=excluded.kind,
                state=excluded.state,
                started_at=excluded.started_at,
                sequence=excluded.sequence,
                size_bytes=excluded.size_bytes,
                media_duration_ms=COALESCE(excluded.media_duration_ms, recordings.media_duration_ms)
             WHERE camera_id != excluded.camera_id
                OR kind != excluded.kind
                OR state != excluded.state
                OR started_at != excluded.started_at
                OR sequence != excluded.sequence
                OR size_bytes != excluded.size_bytes
                OR (excluded.media_duration_ms IS NOT NULL
                    AND media_duration_ms IS NOT excluded.media_duration_ms)",
            params![
                recording.camera_id.as_str(),
                recording.relative_path,
                recording.kind.as_str(),
                recording.state.as_str(),
                encode_time(recording.started_at),
                i64::from(recording.sequence),
                checked_i64(recording.size_bytes, "size_bytes")?,
                duration,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn remove_relative_path(&mut self, relative_path: &str) -> Result<bool, IndexError> {
        #[cfg(any(test, feature = "test-hooks"))]
        if test_hooks::take_remove_failure() {
            return Err(IndexError::InvalidData(
                "injected index-row removal failure".to_owned(),
            ));
        }
        Ok(self.connection.execute(
            "DELETE FROM recordings WHERE relative_path=?1",
            [relative_path],
        )? > 0)
    }

    pub fn get_by_relative_path(
        &self,
        relative_path: &str,
    ) -> Result<Option<IndexedRecording>, IndexError> {
        let raw = self
            .connection
            .query_row(
                &format!("{SELECT_SQL} WHERE relative_path=?1"),
                [relative_path],
                raw_row,
            )
            .optional()?;
        raw.map(IndexedRecording::try_from).transpose()
    }

    pub fn list_all(&self) -> Result<Vec<IndexedRecording>, IndexError> {
        #[cfg(any(test, feature = "test-hooks"))]
        if test_hooks::take_list_all_corruption(&self.path) {
            return Err(IndexError::InjectedCorruption);
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if test_hooks::take_list_all_error(&self.path) {
            return Err(IndexError::InjectedFailure);
        }
        self.query_many(
            &format!("{SELECT_SQL} ORDER BY camera_id, started_at, sequence, relative_path"),
            [],
        )
    }

    pub fn list_camera(&self, camera: &CameraId) -> Result<Vec<IndexedRecording>, IndexError> {
        self.query_many(
            &format!(
                "{SELECT_SQL} WHERE camera_id=?1 ORDER BY started_at, sequence, relative_path"
            ),
            [camera.as_str()],
        )
    }

    pub fn query_time_range(
        &self,
        camera: &CameraId,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<IndexedRecording>, IndexError> {
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_SQL} WHERE camera_id=?1 AND state='complete' AND started_at>=?2 AND started_at<?3
             ORDER BY started_at, sequence, relative_path"
        ))?;
        let rows = statement.query_map(
            params![camera.as_str(), encode_time(start), encode_time(end)],
            raw_row,
        )?;
        collect_rows(rows)
    }

    /// Finds a finalized recording whose known media interval contains the
    /// supplied local wall-clock timestamp. Unknown durations are deliberately
    /// not guessed: returning no match is safer than opening unrelated footage.
    pub fn find_recording_at(
        &self,
        camera: &CameraId,
        timestamp: NaiveDateTime,
    ) -> Result<Option<IndexedRecording>, IndexError> {
        let raw = self
            .connection
            .query_row(
                &format!(
                    "{SELECT_SQL} WHERE camera_id=?1 AND state='complete' AND media_duration_ms IS NOT NULL AND started_at<=?2 \
                     ORDER BY started_at DESC, sequence DESC, relative_path DESC LIMIT 1"
                ),
                params![camera.as_str(), encode_time(timestamp)],
                raw_row,
            )
            .optional()?;
        let Some(recording) = raw.map(IndexedRecording::try_from).transpose()? else {
            return Ok(None);
        };
        let Some(duration_ms) = recording.media_duration_ms else {
            return Ok(None);
        };
        let duration_ms = i64::try_from(duration_ms).map_err(|_| {
            IndexError::InvalidData("media duration exceeds chrono range".to_owned())
        })?;
        let Some(end) = recording
            .started_at
            .checked_add_signed(chrono::Duration::milliseconds(duration_ms))
        else {
            return Err(IndexError::InvalidData(
                "recording end timestamp overflow".to_owned(),
            ));
        };
        Ok((timestamp < end).then_some(recording))
    }

    pub fn available_days(&self, camera: &CameraId) -> Result<Vec<NaiveDate>, IndexError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT substr(started_at, 1, 10)
             FROM recordings
             WHERE camera_id=?1 AND state='complete'
             ORDER BY 1",
        )?;
        let rows = statement.query_map([camera.as_str()], |row| row.get::<_, String>(0))?;
        let mut days = Vec::new();
        for raw in rows {
            let raw = raw?;
            days.push(
                NaiveDate::parse_from_str(&raw, "%Y-%m-%d").map_err(|error| {
                    IndexError::InvalidData(format!(
                        "invalid indexed recording day {raw:?}: {error}"
                    ))
                })?,
            );
        }
        Ok(days)
    }

    pub fn previous_recording(
        &self,
        recording: &IndexedRecording,
    ) -> Result<Option<IndexedRecording>, IndexError> {
        let raw = self
            .connection
            .query_row(
                &format!(
                    "{SELECT_SQL}
                     WHERE camera_id=?1 AND state='complete' AND (
                        started_at < ?2 OR
                        (started_at = ?2 AND sequence < ?3) OR
                        (started_at = ?2 AND sequence = ?3 AND relative_path < ?4)
                     )
                     ORDER BY started_at DESC, sequence DESC, relative_path DESC
                     LIMIT 1"
                ),
                params![
                    recording.camera_id.as_str(),
                    encode_time(recording.started_at),
                    i64::from(recording.sequence),
                    recording.relative_path.as_str(),
                ],
                raw_row,
            )
            .optional()?;
        raw.map(IndexedRecording::try_from).transpose()
    }

    pub fn next_recording(
        &self,
        recording: &IndexedRecording,
    ) -> Result<Option<IndexedRecording>, IndexError> {
        let raw = self
            .connection
            .query_row(
                &format!(
                    "{SELECT_SQL}
                     WHERE camera_id=?1 AND state='complete' AND (
                        started_at > ?2 OR
                        (started_at = ?2 AND sequence > ?3) OR
                        (started_at = ?2 AND sequence = ?3 AND relative_path > ?4)
                     )
                     ORDER BY started_at, sequence, relative_path
                     LIMIT 1"
                ),
                params![
                    recording.camera_id.as_str(),
                    encode_time(recording.started_at),
                    i64::from(recording.sequence),
                    recording.relative_path.as_str(),
                ],
                raw_row,
            )
            .optional()?;
        raw.map(IndexedRecording::try_from).transpose()
    }

    /// Writes media duration only while the indexed filesystem identity still
    /// matches the object that was inspected. A replacement at the same path
    /// therefore cannot inherit stale probe metadata.
    pub fn update_duration_if_identity_matches(
        &mut self,
        recording: &IndexedRecording,
        media_duration_ms: u64,
    ) -> Result<bool, IndexError> {
        let changed = self.connection.execute(
            "UPDATE recordings SET media_duration_ms=?1
             WHERE relative_path=?2 AND camera_id=?3 AND kind=?4 AND state='complete'
               AND started_at=?5 AND sequence=?6 AND size_bytes=?7",
            params![
                checked_i64(media_duration_ms, "media_duration_ms")?,
                recording.relative_path.as_str(),
                recording.camera_id.as_str(),
                recording.kind.as_str(),
                encode_time(recording.started_at),
                i64::from(recording.sequence),
                checked_i64(recording.size_bytes, "size_bytes")?,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn total_recording_bytes(&self) -> Result<u64, IndexError> {
        let value: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM recordings WHERE state='complete'",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(value)
            .map_err(|_| IndexError::InvalidData("negative indexed byte total".to_owned()))
    }

    pub fn replace_from_snapshot(&mut self, rows: &[RecordingUpsert]) -> Result<(), IndexError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM recordings", [])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO recordings (
                    camera_id, relative_path, kind, state, started_at, sequence,
                    size_bytes, media_duration_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for recording in rows {
                let duration = recording
                    .media_duration_ms
                    .map(|value| checked_i64(value, "media_duration_ms"))
                    .transpose()?;
                insert.execute(params![
                    recording.camera_id.as_str(),
                    recording.relative_path,
                    recording.kind.as_str(),
                    recording.state.as_str(),
                    encode_time(recording.started_at),
                    i64::from(recording.sequence),
                    checked_i64(recording.size_bytes, "size_bytes")?,
                    duration,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
    /// Applies a reconciliation plan in one SQLite transaction.
    pub fn apply_reconciliation(
        &mut self,
        upserts: &[RecordingUpsert],
        remove_relative_paths: &[String],
    ) -> Result<(), IndexError> {
        let transaction = self.connection.transaction()?;
        for recording in upserts {
            let duration = recording
                .media_duration_ms
                .map(|value| checked_i64(value, "media_duration_ms"))
                .transpose()?;
            transaction.execute(
                "INSERT INTO recordings (
                    camera_id, relative_path, kind, state, started_at, sequence,
                    size_bytes, media_duration_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(relative_path) DO UPDATE SET
                    camera_id=excluded.camera_id,
                    kind=excluded.kind,
                    state=excluded.state,
                    started_at=excluded.started_at,
                    sequence=excluded.sequence,
                    size_bytes=excluded.size_bytes,
                    media_duration_ms=excluded.media_duration_ms
                 WHERE camera_id != excluded.camera_id
                    OR kind != excluded.kind
                    OR state != excluded.state
                    OR started_at != excluded.started_at
                    OR sequence != excluded.sequence
                    OR size_bytes != excluded.size_bytes
                    OR media_duration_ms IS NOT excluded.media_duration_ms",
                params![
                    recording.camera_id.as_str(),
                    recording.relative_path,
                    recording.kind.as_str(),
                    recording.state.as_str(),
                    encode_time(recording.started_at),
                    i64::from(recording.sequence),
                    checked_i64(recording.size_bytes, "size_bytes")?,
                    duration,
                ],
            )?;
        }
        for relative_path in remove_relative_paths {
            transaction.execute(
                "DELETE FROM recordings WHERE relative_path=?1",
                [relative_path],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn query_many<P>(&self, sql: &str, params: P) -> Result<Vec<IndexedRecording>, IndexError>
    where
        P: rusqlite::Params,
    {
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(params, raw_row)?;
        collect_rows(rows)
    }
}

const SELECT_SQL: &str = "SELECT id, camera_id, relative_path, kind, state, started_at,
    sequence, size_bytes, media_duration_ms FROM recordings";

#[derive(Debug)]
struct RawRecording {
    id: i64,
    camera_id: String,
    relative_path: String,
    kind: String,
    state: String,
    started_at: String,
    sequence: i64,
    size_bytes: i64,
    media_duration_ms: Option<i64>,
}

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecording> {
    Ok(RawRecording {
        id: row.get(0)?,
        camera_id: row.get(1)?,
        relative_path: row.get(2)?,
        kind: row.get(3)?,
        state: row.get(4)?,
        started_at: row.get(5)?,
        sequence: row.get(6)?,
        size_bytes: row.get(7)?,
        media_duration_ms: row.get(8)?,
    })
}

impl TryFrom<RawRecording> for IndexedRecording {
    type Error = IndexError;

    fn try_from(raw: RawRecording) -> Result<Self, Self::Error> {
        Ok(Self {
            id: RecordingId::new(checked_u64(raw.id, "row id")?)
                .map_err(|error| IndexError::InvalidData(error.to_string()))?,
            camera_id: CameraId::parse(raw.camera_id)
                .map_err(|error| IndexError::InvalidData(error.to_string()))?,
            relative_path: raw.relative_path,
            kind: RecordingKind::parse(&raw.kind)?,
            state: parse_state(&raw.state)?,
            started_at: decode_time(&raw.started_at)?,
            sequence: u32::try_from(raw.sequence)
                .map_err(|_| IndexError::InvalidData("invalid sequence".to_owned()))?,
            size_bytes: checked_u64(raw.size_bytes, "size_bytes")?,
            media_duration_ms: raw
                .media_duration_ms
                .map(|value| checked_u64(value, "media_duration_ms"))
                .transpose()?,
        })
    }
}

fn collect_rows<I>(rows: I) -> Result<Vec<IndexedRecording>, IndexError>
where
    I: Iterator<Item = rusqlite::Result<RawRecording>>,
{
    let mut recordings = Vec::new();
    for raw in rows {
        recordings.push(IndexedRecording::try_from(raw?)?);
    }
    Ok(recordings)
}

fn parse_state(value: &str) -> Result<RecordingState, IndexError> {
    match value {
        "active" => Ok(RecordingState::Active),
        "complete" => Ok(RecordingState::Complete),
        "recovering" => Ok(RecordingState::Recovering),
        "corrupted" => Ok(RecordingState::Corrupted),
        "missing" => Ok(RecordingState::Missing),
        _ => Err(IndexError::InvalidData(format!(
            "unknown recording state {value:?}"
        ))),
    }
}

fn verify_foreign_keys(connection: &Connection) -> Result<(), IndexError> {
    let enabled: i64 = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if enabled == 1 {
        Ok(())
    } else {
        Err(IndexError::ForeignKeysUnavailable)
    }
}

fn enable_and_verify_wal(connection: &Connection) -> Result<(), IndexError> {
    let requested: String =
        connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    verify_wal_mode(&requested)?;
    let current: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    verify_wal_mode(&current)
}

fn verify_wal_mode(mode: &str) -> Result<(), IndexError> {
    if mode.eq_ignore_ascii_case("wal") {
        Ok(())
    } else {
        Err(IndexError::WalUnavailable {
            mode: mode.to_owned(),
        })
    }
}

fn schema_version(connection: &Connection) -> Result<i32, IndexError> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn reject_future_schema(connection: &Connection) -> Result<(), IndexError> {
    let version = schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(IndexError::FutureSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn migrate(connection: &Connection) -> Result<(), IndexError> {
    let version = schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(IndexError::FutureSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "CREATE TABLE recordings (
            id INTEGER PRIMARY KEY,
            camera_id TEXT NOT NULL,
            relative_path TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL CHECK(kind IN ('normal','recovered')),
            state TEXT NOT NULL CHECK(state IN ('active','complete','recovering','corrupted','missing')),
            started_at TEXT NOT NULL,
            sequence INTEGER NOT NULL CHECK(sequence >= 1),
            size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
            media_duration_ms INTEGER NULL CHECK(media_duration_ms IS NULL OR media_duration_ms >= 0),
            discovered_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now'))
         );
         CREATE INDEX recordings_timeline_idx
            ON recordings(camera_id, started_at, sequence);
         PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn encode_time(value: NaiveDateTime) -> String {
    value.format(TIME_FORMAT).to_string()
}

fn decode_time(value: &str) -> Result<NaiveDateTime, IndexError> {
    NaiveDateTime::parse_from_str(value, TIME_FORMAT).map_err(|error| {
        IndexError::InvalidData(format!(
            "invalid local wall-clock timestamp {value:?}: {error}"
        ))
    })
}

fn checked_i64(value: u64, field: &str) -> Result<i64, IndexError> {
    i64::try_from(value)
        .map_err(|_| IndexError::InvalidData(format!("{field} exceeds SQLite INTEGER range")))
}

fn checked_u64(value: i64, field: &str) -> Result<u64, IndexError> {
    u64::try_from(value).map_err(|_| IndexError::InvalidData(format!("{field} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecordingUpsert {
        RecordingUpsert {
            camera_id: CameraId::parse("cam-a").unwrap(),
            relative_path: "cam-a/2026/08/29/08-30-00.mkv".to_owned(),
            kind: RecordingKind::Normal,
            state: RecordingState::Complete,
            started_at: NaiveDateTime::parse_from_str("2026-08-29T08:30:00", TIME_FORMAT).unwrap(),
            sequence: 1,
            size_bytes: 123,
            media_duration_ms: Some(10_000),
        }
    }

    fn sample_at(
        relative_path: &str,
        kind: RecordingKind,
        started_at: &str,
        sequence: u32,
        duration: Option<u64>,
    ) -> RecordingUpsert {
        RecordingUpsert {
            camera_id: CameraId::parse("cam-a").unwrap(),
            relative_path: relative_path.to_owned(),
            kind,
            state: RecordingState::Complete,
            started_at: NaiveDateTime::parse_from_str(started_at, TIME_FORMAT).unwrap(),
            sequence,
            size_bytes: 100 + u64::from(sequence),
            media_duration_ms: duration,
        }
    }

    #[test]
    fn fresh_database_migrates_to_v1_and_current_reopen_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite3");
        let index = RecordingIndex::open(&path).unwrap();
        assert_eq!(index.schema_version().unwrap(), 1);
        drop(index);
        assert_eq!(
            RecordingIndex::open(path)
                .unwrap()
                .schema_version()
                .unwrap(),
            1
        );
    }

    #[test]
    fn future_schema_fails_safely() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("future.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(matches!(
            RecordingIndex::open(path),
            Err(IndexError::FutureSchema { found: 99, .. })
        ));
    }

    #[test]
    fn wal_and_timeline_index_are_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        let mode: String = index
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let foreign_keys: i64 = index
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let count: i64 = index
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='recordings_timeline_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn wal_mode_verifier_rejects_any_non_wal_result() {
        assert!(verify_wal_mode("wal").is_ok());
        assert!(verify_wal_mode("WAL").is_ok());
        assert!(matches!(
            verify_wal_mode("delete"),
            Err(IndexError::WalUnavailable { ref mode }) if mode == "delete"
        ));
    }

    #[test]
    fn upsert_is_idempotent_and_queries_round_trip_local_time() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        assert!(index.upsert(&sample()).unwrap());
        assert!(!index.upsert(&sample()).unwrap());
        let rows = index.list_camera(&sample().camera_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].started_at, sample().started_at);
        assert_eq!(index.total_recording_bytes().unwrap(), 123);
    }

    #[test]
    fn timeline_queries_order_normal_recovered_and_same_second_sequences_stably() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        let rows = [
            sample_at(
                "cam-a/2026/08/29/08-30-00-2.recovered.mkv",
                RecordingKind::Recovered,
                "2026-08-29T08:30:00",
                2,
                Some(1_000),
            ),
            sample_at(
                "cam-a/2026/08/29/08-30-00.mkv",
                RecordingKind::Normal,
                "2026-08-29T08:30:00",
                1,
                None,
            ),
            sample_at(
                "cam-a/2026/08/29/09-00-00.mkv",
                RecordingKind::Normal,
                "2026-08-29T09:00:00",
                1,
                Some(2_000),
            ),
        ];
        for row in &rows {
            index.upsert(row).unwrap();
        }

        let queried = index
            .query_time_range(
                &CameraId::parse("cam-a").unwrap(),
                NaiveDateTime::parse_from_str("2026-08-29T08:00:00", TIME_FORMAT).unwrap(),
                NaiveDateTime::parse_from_str("2026-08-29T10:00:00", TIME_FORMAT).unwrap(),
            )
            .unwrap();
        assert_eq!(
            queried
                .iter()
                .map(|row| row.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "cam-a/2026/08/29/08-30-00.mkv",
                "cam-a/2026/08/29/08-30-00-2.recovered.mkv",
                "cam-a/2026/08/29/09-00-00.mkv",
            ]
        );
        assert!(queried[0].media_duration_ms.is_none());
        assert_eq!(queried[1].kind, RecordingKind::Recovered);
    }

    #[test]
    fn recording_lookup_at_timestamp_is_camera_local_and_interval_exact() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        let cam_a = CameraId::parse("cam-a").unwrap();
        let cam_b = CameraId::parse("cam-b").unwrap();

        let first = sample_at(
            "cam-a/2026/08/29/10-00-00.mkv",
            RecordingKind::Normal,
            "2026-08-29T10:00:00",
            1,
            Some(60_000),
        );
        let second = sample_at(
            "cam-a/2026/08/29/10-02-00.mkv",
            RecordingKind::Normal,
            "2026-08-29T10:02:00",
            1,
            Some(60_000),
        );
        let unknown = sample_at(
            "cam-a/2026/08/29/10-04-00.mkv",
            RecordingKind::Normal,
            "2026-08-29T10:04:00",
            1,
            None,
        );
        let mut other_camera = sample_at(
            "cam-b/2026/08/29/10-00-00.mkv",
            RecordingKind::Normal,
            "2026-08-29T10:00:00",
            1,
            Some(180_000),
        );
        other_camera.camera_id = cam_b.clone();
        for row in [&first, &second, &unknown, &other_camera] {
            index.upsert(row).unwrap();
        }

        let at = |value: &str| NaiveDateTime::parse_from_str(value, TIME_FORMAT).unwrap();
        assert!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T09:59:59"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T10:00:30"))
                .unwrap()
                .unwrap()
                .relative_path,
            first.relative_path
        );
        assert!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T10:01:30"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T10:02:30"))
                .unwrap()
                .unwrap()
                .relative_path,
            second.relative_path
        );
        assert!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T10:03:00"))
                .unwrap()
                .is_none()
        );
        assert!(
            index
                .find_recording_at(&cam_a, at("2026-08-29T10:04:10"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            index
                .find_recording_at(&cam_b, at("2026-08-29T10:00:30"))
                .unwrap()
                .unwrap()
                .relative_path,
            other_camera.relative_path
        );
    }

    #[test]
    fn range_boundaries_days_and_adjacency_are_database_queries() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        for row in [
            sample_at(
                "cam-a/2026/08/28/23-59-59.mkv",
                RecordingKind::Normal,
                "2026-08-28T23:59:59",
                1,
                None,
            ),
            sample_at(
                "cam-a/2026/08/29/00-00-00.mkv",
                RecordingKind::Normal,
                "2026-08-29T00:00:00",
                1,
                None,
            ),
            sample_at(
                "cam-a/2026/08/30/00-00-00.mkv",
                RecordingKind::Normal,
                "2026-08-30T00:00:00",
                1,
                None,
            ),
        ] {
            index.upsert(&row).unwrap();
        }
        let camera = CameraId::parse("cam-a").unwrap();
        let start = NaiveDateTime::parse_from_str("2026-08-29T00:00:00", TIME_FORMAT).unwrap();
        let end = NaiveDateTime::parse_from_str("2026-08-30T00:00:00", TIME_FORMAT).unwrap();
        let queried = index.query_time_range(&camera, start, end).unwrap();
        assert_eq!(queried.len(), 1, "start inclusive, end exclusive");
        assert_eq!(queried[0].started_at, start);

        assert_eq!(index.available_days(&camera).unwrap().len(), 3);
        assert!(
            index
                .available_days(&CameraId::parse("missing-camera").unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            index
                .previous_recording(&queried[0])
                .unwrap()
                .unwrap()
                .started_at,
            NaiveDateTime::parse_from_str("2026-08-28T23:59:59", TIME_FORMAT).unwrap()
        );
        assert_eq!(
            index
                .next_recording(&queried[0])
                .unwrap()
                .unwrap()
                .started_at,
            end
        );
    }

    #[test]
    fn duration_writeback_requires_the_same_filesystem_identity() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        let row = sample_at(
            "cam-a/2026/08/29/08-30-00.mkv",
            RecordingKind::Normal,
            "2026-08-29T08:30:00",
            1,
            None,
        );
        index.upsert(&row).unwrap();
        let indexed = index
            .get_by_relative_path(&row.relative_path)
            .unwrap()
            .unwrap();
        assert!(
            index
                .update_duration_if_identity_matches(&indexed, 9_876)
                .unwrap()
        );
        assert_eq!(
            index
                .get_by_relative_path(&row.relative_path)
                .unwrap()
                .unwrap()
                .media_duration_ms,
            Some(9_876)
        );

        let mut stale = indexed;
        stale.size_bytes += 1;
        assert!(
            !index
                .update_duration_if_identity_matches(&stale, 12_345)
                .unwrap()
        );
        assert_eq!(
            index
                .get_by_relative_path(&row.relative_path)
                .unwrap()
                .unwrap()
                .media_duration_ms,
            Some(9_876)
        );
    }

    #[test]
    fn replace_snapshot_is_atomic_at_database_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let mut index = RecordingIndex::open(temp.path().join("index.sqlite3")).unwrap();
        index.replace_from_snapshot(&[sample()]).unwrap();
        assert_eq!(index.list_all().unwrap().len(), 1);
        index.replace_from_snapshot(&[]).unwrap();
        assert!(index.list_all().unwrap().is_empty());
    }

    #[test]
    fn failed_migration_rolls_back_and_does_not_advance_user_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failed-migration.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE recordings (wrong_shape TEXT NOT NULL);\n                 PRAGMA user_version=0;",
            )
            .unwrap();
        drop(connection);

        assert!(RecordingIndex::open(&path).is_err());

        let connection = Connection::open(&path).unwrap();
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let timeline_index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master\n                 WHERE type='index' AND name='recordings_timeline_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 0);
        assert_eq!(timeline_index_count, 0);
    }

    #[test]
    fn random_bytes_are_detected_as_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("broken.sqlite3");
        std::fs::write(&path, b"definitely not sqlite").unwrap();
        let error = RecordingIndex::open(path).unwrap_err();
        assert!(error.is_corruption(), "{error:?}");
    }
}
