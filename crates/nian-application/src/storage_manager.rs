//! Filesystem/SQLite reconciliation and retention orchestration for M4.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use nian_domain::{CameraId, RetentionPolicy, StorageQuota};
use nian_index::{IndexError, IndexedRecording, RecordingIndex, RecordingKind, RecordingUpsert};
use nian_storage::classification::owned_recording_name;
use nian_storage::{
    CameraLease, FilesystemInventory, InventoryRecording, RecordingFileKind, RecordingsLayout,
    RecoveredRetentionState, StorageError, classify_recording_file, inspect_recovered_retention,
    inventory_recordings, parse_recovery_tombstone, recovery_tombstone_matches,
    recovery_transaction_paths,
};

#[derive(Debug, thiserror::Error)]
pub enum StorageManagerError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("invalid retention configuration: {0}")]
    Policy(String),
    #[error("retention requires a successful reconciliation or rebuild first")]
    NotReconciled,
    #[error("invalid finalized recording metadata: {0}")]
    InvalidFinalizedRecording(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationFailureKind {
    LeaseInspection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationFailure {
    pub camera_id: CameraId,
    pub kind: ReconciliationFailureKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconciliationReport {
    pub inserted: usize,
    pub updated: usize,
    pub removed_missing: usize,
    pub active_partials: usize,
    pub recovery_pending_partials: usize,
    pub ignored_foreign: usize,
    pub errors: Vec<ReconciliationFailure>,
}

impl ReconciliationReport {
    pub fn database_mutations(&self) -> usize {
        self.inserted + self.updated + self.removed_missing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionFailureKind {
    Revalidation,
    RecoveryOwnership,
    FilesystemDelete,
    TombstoneRevalidation,
    TombstoneCleanup,
    IndexDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionFailure {
    pub relative_path: String,
    pub kind: RetentionFailureKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionReport {
    pub examined: usize,
    pub deleted: usize,
    pub bytes_freed: u64,
    pub age_deleted: usize,
    pub quota_deleted: usize,
    pub skipped_active: usize,
    pub blocked_recovery_transactions: usize,
    pub missing: usize,
    pub failed: Vec<RetentionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactCleanupReport {
    pub removed_scratch: usize,
    pub removed_tombstones: usize,
    pub skipped_owned_elsewhere: usize,
    pub preserved_ambiguous: usize,
    pub failed: usize,
}

/// Coordinates the rebuildable index with authoritative filesystem state.
///
/// `reconciled` is a safety gate: retention is unavailable until filesystem
/// truth has been successfully applied to the SQLite cache.
#[derive(Debug)]
pub struct StorageManager {
    layout: RecordingsLayout,
    index: RecordingIndex,
    retention_policy: RetentionPolicy,
    storage_quota: Option<StorageQuota>,
    reconciled: bool,
    rebuilt_after_corruption: bool,
}

impl StorageManager {
    pub fn open(
        layout: RecordingsLayout,
        retention_policy: RetentionPolicy,
        storage_quota: Option<StorageQuota>,
    ) -> Result<Self, StorageManagerError> {
        validate_retention(retention_policy, storage_quota)?;
        layout.ensure_control_dir()?;
        let index_path = layout.recording_index_path();

        match RecordingIndex::open(&index_path) {
            Ok(index) => Ok(Self {
                layout,
                index,
                retention_policy,
                storage_quota,
                reconciled: false,
                rebuilt_after_corruption: false,
            }),
            Err(error) if error.is_corruption() => {
                quarantine_corrupt_index(&index_path)?;
                let index = RecordingIndex::open(&index_path)?;
                let mut manager = Self {
                    layout,
                    index,
                    retention_policy,
                    storage_quota,
                    reconciled: false,
                    rebuilt_after_corruption: true,
                };
                manager.rebuild()?;
                Ok(manager)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn layout(&self) -> &RecordingsLayout {
        &self.layout
    }

    pub fn was_rebuilt_after_corruption(&self) -> bool {
        self.rebuilt_after_corruption
    }

    pub fn reconcile(&mut self) -> Result<ReconciliationReport, StorageManagerError> {
        let inventory = inventory_recordings(&self.layout)?;
        let indexed = self.index.list_all()?;
        let indexed_by_path: HashMap<&str, &IndexedRecording> = indexed
            .iter()
            .map(|recording| (recording.relative_path.as_str(), recording))
            .collect();

        let mut report = ReconciliationReport {
            ignored_foreign: inventory.ignored_foreign,
            ..ReconciliationReport::default()
        };
        let mut upserts = Vec::new();
        let mut filesystem_paths = HashSet::new();

        for recording in &inventory.recordings {
            filesystem_paths.insert(recording.relative_path.as_str());
            let existing = indexed_by_path
                .get(recording.relative_path.as_str())
                .copied();
            let upsert =
                recording_to_upsert(recording, existing.and_then(|row| row.media_duration_ms));
            match existing {
                None => {
                    report.inserted += 1;
                    upserts.push(upsert);
                }
                Some(row) if !same_indexed_recording(row, &upsert) => {
                    report.updated += 1;
                    upserts.push(upsert);
                }
                Some(_) => {}
            }
        }

        let mut removals = Vec::new();
        for row in &indexed {
            if !filesystem_paths.contains(row.relative_path.as_str()) {
                report.removed_missing += 1;
                removals.push(row.relative_path.clone());
            }
        }

        classify_partials_by_lease(&self.layout, &inventory, &mut report);
        self.index.apply_reconciliation(&upserts, &removals)?;
        self.reconciled = true;
        Ok(report)
    }

    /// Reconstructs the complete recording index from filesystem facts alone.
    ///
    /// Media duration is deliberately left NULL because filenames and file
    /// sizes do not prove duration.
    pub fn rebuild(&mut self) -> Result<usize, StorageManagerError> {
        let inventory = inventory_recordings(&self.layout)?;
        let rows: Vec<_> = inventory
            .recordings
            .iter()
            .map(|recording| recording_to_upsert(recording, None))
            .collect();
        self.index.replace_from_snapshot(&rows)?;
        self.reconciled = true;
        Ok(rows.len())
    }

    pub fn list_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Vec<IndexedRecording>, StorageManagerError> {
        Ok(self.index.list_camera(camera_id)?)
    }

    pub fn query_time_range(
        &self,
        camera_id: &CameraId,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<IndexedRecording>, StorageManagerError> {
        Ok(self.index.query_time_range(camera_id, start, end)?)
    }

    pub fn total_indexed_recording_bytes(&self) -> Result<u64, StorageManagerError> {
        Ok(self.index.total_recording_bytes()?)
    }

    /// Incrementally indexes a finalized file without making publication depend
    /// on SQLite health. Any error leaves the already-published media untouched.
    pub fn upsert_finalized(
        &mut self,
        camera_id: &CameraId,
        final_path: &Path,
        started_at: NaiveDateTime,
        size_bytes: u64,
        media_duration_ms: Option<u64>,
    ) -> Result<bool, StorageManagerError> {
        let kind = match classify_recording_file(final_path) {
            RecordingFileKind::NormalRecording => RecordingKind::Normal,
            RecordingFileKind::RecoveredRecording => RecordingKind::Recovered,
            other => {
                return Err(StorageManagerError::InvalidFinalizedRecording(format!(
                    "path {final_path:?} has non-final kind {other:?}"
                )));
            }
        };
        let expected_parent = self.layout.day_dir(camera_id, started_at.date());
        if final_path.parent() != Some(expected_parent.as_path()) {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "final path does not match camera/date layout".to_owned(),
            ));
        }
        let Some(name) = final_path.file_name().and_then(|name| name.to_str()) else {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "final filename is not UTF-8".to_owned(),
            ));
        };
        let Some(identity) = owned_recording_name(name) else {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "final filename has no canonical identity".to_owned(),
            ));
        };
        if identity.started_at != started_at.time() {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "started_at does not match canonical filename".to_owned(),
            ));
        }
        let metadata = std::fs::symlink_metadata(final_path).map_err(|source| {
            StorageManagerError::Storage(StorageError::Io {
                path: final_path.to_path_buf(),
                source,
            })
        })?;
        if !metadata.is_file() {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "finalized recording is not a regular file".to_owned(),
            ));
        }
        if metadata.len() != size_bytes {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "trusted size_bytes does not match the finalized file".to_owned(),
            ));
        }

        let upsert = RecordingUpsert {
            camera_id: camera_id.clone(),
            relative_path: relative_path(&self.layout, final_path)?,
            kind,
            state: nian_domain::RecordingState::Complete,
            started_at,
            sequence: identity.sequence,
            size_bytes,
            media_duration_ms,
        };
        Ok(self.index.upsert(&upsert)?)
    }

    /// Runs age/quota retention using an injected local naive wall-clock time.
    ///
    /// Age is interpreted in the same local wall-clock domain encoded by
    /// recording filenames. No UTC claim is made.
    pub fn run_retention(
        &mut self,
        now: NaiveDateTime,
    ) -> Result<RetentionReport, StorageManagerError> {
        if !self.reconciled {
            return Err(StorageManagerError::NotReconciled);
        }

        let inventory = inventory_recordings(&self.layout)?;
        let mut recordings = inventory.recordings;
        recordings.sort_by(|a, b| {
            (a.started_at, a.sequence, &a.relative_path).cmp(&(
                b.started_at,
                b.sequence,
                &b.relative_path,
            ))
        });

        let mut report = RetentionReport {
            examined: recordings.len(),
            ..RetentionReport::default()
        };
        let mut usage: u64 = recordings
            .iter()
            .map(|recording| recording.size_bytes)
            .sum();
        let quota_triggered = self
            .storage_quota
            .is_some_and(|quota| usage > quota.max_bytes);
        let age_cutoff = self
            .retention_policy
            .max_age_days
            .map(|days| now - ChronoDuration::days(i64::from(days)));

        for candidate in recordings {
            let age_eligible = age_cutoff.is_some_and(|cutoff| candidate.started_at < cutoff);
            let quota_eligible = quota_triggered
                && self
                    .storage_quota
                    .is_some_and(|quota| usage > quota.cleanup_target_bytes);
            if !age_eligible && !quota_eligible {
                continue;
            }

            match revalidate_candidate(&self.layout, &candidate) {
                Ok(()) => {}
                Err(Revalidation::Missing) => {
                    report.missing += 1;
                    continue;
                }
                Err(Revalidation::Changed(detail)) => {
                    report.failed.push(RetentionFailure {
                        relative_path: candidate.relative_path.clone(),
                        kind: RetentionFailureKind::Revalidation,
                        detail,
                    });
                    continue;
                }
            }

            let (tombstone, recovery_lease) =
                if candidate.kind == RecordingFileKind::RecoveredRecording {
                    let lease = match CameraLease::try_acquire(&self.layout, &candidate.camera_id) {
                        Ok(lease) => lease,
                        Err(StorageError::CameraAlreadyActive { .. }) => {
                            report.skipped_active += 1;
                            continue;
                        }
                        Err(error) => {
                            report.failed.push(RetentionFailure {
                                relative_path: candidate.relative_path.clone(),
                                kind: RetentionFailureKind::RecoveryOwnership,
                                detail: error.to_string(),
                            });
                            continue;
                        }
                    };
                    match inspect_recovered_retention(&candidate.path) {
                        RecoveredRetentionState::Settled {
                            tombstone,
                            evidence,
                        } => (Some((tombstone, evidence)), Some(lease)),
                        RecoveredRetentionState::Blocked { .. } => {
                            report.blocked_recovery_transactions += 1;
                            continue;
                        }
                    }
                } else {
                    (None, None)
                };

            if let Err(error) = std::fs::remove_file(&candidate.path) {
                report.failed.push(RetentionFailure {
                    relative_path: candidate.relative_path.clone(),
                    kind: RetentionFailureKind::FilesystemDelete,
                    detail: error.to_string(),
                });
                continue;
            }

            usage = usage.saturating_sub(candidate.size_bytes);
            report.deleted += 1;
            report.bytes_freed = report.bytes_freed.saturating_add(candidate.size_bytes);
            if age_eligible {
                report.age_deleted += 1;
            }
            if quota_eligible {
                report.quota_deleted += 1;
            }

            if let Some((tombstone, evidence)) = tombstone {
                if !recovery_tombstone_matches(&tombstone, &evidence) {
                    report.failed.push(RetentionFailure {
                        relative_path: candidate.relative_path.clone(),
                        kind: RetentionFailureKind::TombstoneRevalidation,
                        detail: "recovery tombstone changed after retention planning".to_owned(),
                    });
                    // Media is already gone. Preserve ambiguous transaction evidence.
                    continue;
                }
                if let Err(error) = std::fs::remove_file(&tombstone) {
                    report.failed.push(RetentionFailure {
                        relative_path: candidate.relative_path.clone(),
                        kind: RetentionFailureKind::TombstoneCleanup,
                        detail: error.to_string(),
                    });
                    // Media is already gone. Reconciliation will remove the stale DB row.
                    continue;
                }
            }

            if let Err(error) = self.index.remove_relative_path(&candidate.relative_path) {
                report.failed.push(RetentionFailure {
                    relative_path: candidate.relative_path.clone(),
                    kind: RetentionFailureKind::IndexDelete,
                    detail: error.to_string(),
                });
            }
            drop(recovery_lease);
        }

        Ok(report)
    }

    /// Conservative non-footage artifact cleanup, separate from retention bytes.
    pub fn cleanup_stale_recovery_artifacts(
        &self,
    ) -> Result<ArtifactCleanupReport, StorageManagerError> {
        let inventory = inventory_recordings(&self.layout)?;
        let mut by_camera: BTreeMap<CameraId, Vec<_>> = BTreeMap::new();
        for artifact in inventory.artifacts {
            by_camera
                .entry(artifact.camera_id.clone())
                .or_default()
                .push(artifact);
        }

        let mut report = ArtifactCleanupReport::default();
        for (camera, artifacts) in by_camera {
            let lease = match CameraLease::try_acquire(&self.layout, &camera) {
                Ok(lease) => lease,
                Err(StorageError::CameraAlreadyActive { .. }) => {
                    report.skipped_owned_elsewhere += artifacts.len();
                    continue;
                }
                Err(_) => {
                    report.failed += artifacts.len();
                    continue;
                }
            };

            for artifact in artifacts {
                debug_assert!(lease.authorizes(&self.layout, &camera));
                match artifact.kind {
                    RecordingFileKind::RecoveryScratch => {
                        if classify_recording_file(&artifact.path)
                            != RecordingFileKind::RecoveryScratch
                        {
                            report.preserved_ambiguous += 1;
                            continue;
                        }
                        match std::fs::symlink_metadata(&artifact.path) {
                            Ok(metadata) if metadata.is_file() => {
                                if std::fs::remove_file(&artifact.path).is_ok() {
                                    report.removed_scratch += 1;
                                } else {
                                    report.failed += 1;
                                }
                            }
                            _ => report.preserved_ambiguous += 1,
                        }
                    }
                    RecordingFileKind::RecoveryTombstone => {
                        if cleanup_stale_tombstone(&artifact.path) {
                            report.removed_tombstones += 1;
                        } else {
                            report.preserved_ambiguous += 1;
                        }
                    }
                    _ => report.preserved_ambiguous += 1,
                }
            }
            drop(lease);
        }
        Ok(report)
    }
}

fn validate_retention(
    policy: RetentionPolicy,
    quota: Option<StorageQuota>,
) -> Result<(), StorageManagerError> {
    policy
        .validate()
        .map_err(|error| StorageManagerError::Policy(error.to_string()))?;
    if let Some(quota) = quota {
        quota
            .validate()
            .map_err(|error| StorageManagerError::Policy(error.to_string()))?;
    }
    if let Some(max_storage_bytes) = policy.max_storage_bytes {
        let Some(quota) = quota else {
            return Err(StorageManagerError::Policy(
                "max_storage_bytes requires an explicit StorageQuota cleanup target".to_owned(),
            ));
        };
        if quota.max_bytes != max_storage_bytes {
            return Err(StorageManagerError::Policy(format!(
                "RetentionPolicy.max_storage_bytes ({max_storage_bytes}) must equal StorageQuota.max_bytes ({})",
                quota.max_bytes
            )));
        }
    }
    Ok(())
}

fn recording_to_upsert(
    recording: &InventoryRecording,
    media_duration_ms: Option<u64>,
) -> RecordingUpsert {
    RecordingUpsert {
        camera_id: recording.camera_id.clone(),
        relative_path: recording.relative_path.clone(),
        kind: match recording.kind {
            RecordingFileKind::NormalRecording => RecordingKind::Normal,
            RecordingFileKind::RecoveredRecording => RecordingKind::Recovered,
            _ => unreachable!("inventory recordings contain only finalized recording kinds"),
        },
        state: nian_domain::RecordingState::Complete,
        started_at: recording.started_at,
        sequence: recording.sequence,
        size_bytes: recording.size_bytes,
        media_duration_ms,
    }
}

fn same_indexed_recording(row: &IndexedRecording, expected: &RecordingUpsert) -> bool {
    row.camera_id == expected.camera_id
        && row.relative_path == expected.relative_path
        && row.kind == expected.kind
        && row.state == expected.state
        && row.started_at == expected.started_at
        && row.sequence == expected.sequence
        && row.size_bytes == expected.size_bytes
}

fn classify_partials_by_lease(
    layout: &RecordingsLayout,
    inventory: &FilesystemInventory,
    report: &mut ReconciliationReport,
) {
    let mut counts: BTreeMap<CameraId, usize> = BTreeMap::new();
    for partial in &inventory.partials {
        *counts.entry(partial.camera_id.clone()).or_default() += 1;
    }
    for (camera, count) in counts {
        match CameraLease::try_acquire(layout, &camera) {
            Ok(lease) => {
                report.recovery_pending_partials += count;
                drop(lease);
            }
            Err(StorageError::CameraAlreadyActive { .. }) => {
                report.active_partials += count;
            }
            Err(error) => report.errors.push(ReconciliationFailure {
                camera_id: camera,
                kind: ReconciliationFailureKind::LeaseInspection,
                detail: error.to_string(),
            }),
        }
    }
}

enum Revalidation {
    Missing,
    Changed(String),
}

fn revalidate_candidate(
    layout: &RecordingsLayout,
    candidate: &InventoryRecording,
) -> Result<(), Revalidation> {
    let expected_path = layout.root().join(Path::new(&candidate.relative_path));
    if expected_path != candidate.path {
        return Err(Revalidation::Changed(
            "candidate no longer maps to its canonical relative path".to_owned(),
        ));
    }
    let metadata = match std::fs::symlink_metadata(&candidate.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Revalidation::Missing);
        }
        Err(error) => return Err(Revalidation::Changed(error.to_string())),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Revalidation::Changed(
            "candidate is no longer a regular file".to_owned(),
        ));
    }
    if metadata.len() != candidate.size_bytes {
        return Err(Revalidation::Changed(
            "candidate size changed after planning".to_owned(),
        ));
    }
    if classify_recording_file(&candidate.path) != candidate.kind {
        return Err(Revalidation::Changed(
            "candidate recording kind changed after planning".to_owned(),
        ));
    }
    let Some(name) = candidate.path.file_name().and_then(|name| name.to_str()) else {
        return Err(Revalidation::Changed(
            "candidate filename is no longer canonical UTF-8".to_owned(),
        ));
    };
    let Some(identity) = owned_recording_name(name) else {
        return Err(Revalidation::Changed(
            "candidate identity no longer parses".to_owned(),
        ));
    };
    if identity.sequence != candidate.sequence || identity.started_at != candidate.started_at.time()
    {
        return Err(Revalidation::Changed(
            "candidate identity changed after planning".to_owned(),
        ));
    }
    Ok(())
}

fn relative_path(
    layout: &RecordingsLayout,
    absolute: &Path,
) -> Result<String, StorageManagerError> {
    let relative = absolute.strip_prefix(layout.root()).map_err(|_| {
        StorageManagerError::InvalidFinalizedRecording(
            "final path is outside the storage root".to_owned(),
        )
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "final path has a non-canonical component".to_owned(),
            ));
        };
        let Some(part) = part.to_str() else {
            return Err(StorageManagerError::InvalidFinalizedRecording(
                "final path component is not UTF-8".to_owned(),
            ));
        };
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn quarantine_corrupt_index(index_path: &Path) -> Result<(), StorageManagerError> {
    let wal = sqlite_sidecar_path(index_path, "-wal")?;
    let shm = sqlite_sidecar_path(index_path, "-shm")?;
    let sources = [index_path.to_path_buf(), wal, shm];

    let serial = (1_u32..)
        .find(|serial| {
            sources.iter().all(|source| {
                let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                !source
                    .with_file_name(format!("{name}.corrupt-{serial}"))
                    .exists()
            })
        })
        .ok_or_else(|| {
            StorageManagerError::Policy("cannot allocate corruption backup name".to_owned())
        })?;

    for source in sources {
        match std::fs::symlink_metadata(&source) {
            Ok(_) => {
                let name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        StorageManagerError::Policy(
                            "SQLite control filename is not UTF-8".to_owned(),
                        )
                    })?;
                let target = source.with_file_name(format!("{name}.corrupt-{serial}"));
                std::fs::rename(&source, &target).map_err(|io| {
                    StorageManagerError::Storage(StorageError::Io {
                        path: source.clone(),
                        source: io,
                    })
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source_error) => {
                return Err(StorageManagerError::Storage(StorageError::Io {
                    path: source,
                    source: source_error,
                }));
            }
        }
    }
    Ok(())
}

fn sqlite_sidecar_path(index_path: &Path, suffix: &str) -> Result<PathBuf, StorageManagerError> {
    let Some(file_name) = index_path.file_name() else {
        return Err(StorageManagerError::Policy(
            "SQLite index path has no filename".to_owned(),
        ));
    };
    let mut sidecar_name = file_name.to_os_string();
    sidecar_name.push(suffix);
    Ok(index_path.with_file_name(sidecar_name))
}

fn cleanup_stale_tombstone(path: &Path) -> bool {
    let Some(final_name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".done"))
    else {
        return false;
    };
    let final_path = path.with_file_name(final_name);
    let Some(paths) = recovery_transaction_paths(&final_path) else {
        return false;
    };
    if std::fs::symlink_metadata(&paths.recovered_final).is_ok()
        || std::fs::symlink_metadata(&paths.original_partial).is_ok()
    {
        return false;
    }
    let Some(transaction) = std::fs::read(path)
        .ok()
        .and_then(|bytes| parse_recovery_tombstone(&bytes))
    else {
        return false;
    };
    let Some(original_name) = paths
        .original_partial
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let Some(recovered_name) = paths
        .recovered_final
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if transaction.original != original_name || transaction.final_name != recovered_name {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[cfg(unix)]
    #[test]
    fn revalidation_rejects_a_recording_replaced_by_symlink_after_planning() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let day = layout.day_dir(&camera, NaiveDate::from_ymd_opt(2026, 8, 20).unwrap());
        std::fs::create_dir_all(&day).unwrap();
        let media = day.join("08-30-00.mkv");
        std::fs::write(&media, b"planned footage").unwrap();
        let candidate = inventory_recordings(&layout)
            .unwrap()
            .recordings
            .into_iter()
            .next()
            .unwrap();

        std::fs::remove_file(&media).unwrap();
        let outside = temp.path().join("outside.mkv");
        std::fs::write(&outside, b"must survive").unwrap();
        symlink(&outside, &media).unwrap();

        assert!(matches!(
            revalidate_candidate(&layout, &candidate),
            Err(Revalidation::Changed(_))
        ));
        assert_eq!(std::fs::read(outside).unwrap(), b"must survive");
    }
}
