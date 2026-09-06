//! Filesystem/SQLite reconciliation and retention orchestration for M4.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Duration as ChronoDuration, NaiveDate, NaiveDateTime};
use nian_domain::{CameraId, RetentionPolicy, StorageQuota};
use nian_index::{IndexError, IndexedRecording, RecordingIndex, RecordingKind, RecordingUpsert};
use nian_storage::classification::owned_recording_name;
use nian_storage::{
    CameraLease, FilesystemInventory, InventoryRecording, PathPresence, RecordingFileKind,
    RecordingsLayout, RecoveredRetentionState, RecoveryTombstone, StorageError,
    classify_recording_file, filesystem_identity_datetime, inspect_path_presence,
    inspect_recovered_retention, inventory_recordings, parse_recovery_tombstone,
    recovery_tombstone_matches, recovery_transaction_paths,
};

#[cfg(test)]
struct RetentionTestGate {
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static RETENTION_PRE_DELETE_GATE: Mutex<Option<RetentionTestGate>> = Mutex::new(None);

const MAX_RECORDING_INDEX_CORRUPT_BACKUPS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum StorageManagerError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    SqliteFamily(#[from] nian_storage::SqliteFamilyError),
    #[error("invalid retention configuration: {0}")]
    Policy(String),
    #[error("retention requires a successful reconciliation or rebuild first")]
    NotReconciled,
    #[error("invalid finalized recording metadata: {0}")]
    InvalidFinalizedRecording(String),
    #[error("recording index is temporarily unavailable during repair")]
    IndexUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingLookupError {
    NotFound,
    Missing,
    Stale(String),
    NotFinalized,
    Index(String),
}

#[derive(Debug, Clone)]
pub struct ValidatedRecording {
    pub indexed: IndexedRecording,
    pub path: PathBuf,
}

impl From<IndexError> for RecordingLookupError {
    fn from(error: IndexError) -> Self {
        Self::Index(error.to_string())
    }
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
    RecoveryInspection,
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
    pub skipped_playback: usize,
    pub blocked_recovery_transactions: usize,
    pub missing: usize,
    pub quota_triggered: bool,
    pub usage_before: u64,
    pub usage_after: u64,
    pub quota_target_reached: bool,
    pub failed: Vec<RetentionFailure>,
}

/// Application-level pins for finalized recordings currently used by playback.
/// This is deliberately separate from `CameraLease`: playback never owns or
/// mutates the live recording namespace.
#[derive(Debug, Clone, Default)]
pub struct PlaybackPins {
    inner: Arc<Mutex<HashMap<String, usize>>>,
}

impl PlaybackPins {
    pub fn pin(&self, relative_path: impl Into<String>) -> PlaybackPin {
        let relative_path = relative_path.into();
        let mut pins = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pins.entry(relative_path.clone()).or_default() += 1;
        drop(pins);
        PlaybackPin {
            pins: self.clone(),
            relative_path,
        }
    }

    pub fn is_pinned(&self, relative_path: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(relative_path)
            .is_some_and(|count| *count > 0)
    }
}

#[derive(Debug)]
pub struct PlaybackPin {
    pins: PlaybackPins,
    relative_path: String,
}

impl Drop for PlaybackPin {
    fn drop(&mut self) {
        let mut pins = self
            .pins
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = pins.get_mut(&self.relative_path) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                pins.remove(&self.relative_path);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactCleanupReport {
    pub removed_scratch: usize,
    pub removed_tombstones: usize,
    pub skipped_owned_elsewhere: usize,
    pub preserved_ambiguous: usize,
    pub inspection_failures: usize,
    pub failed: usize,
}

/// Coordinates the rebuildable index with authoritative filesystem state.
///
/// `reconciled` is a safety gate: retention is unavailable until filesystem
/// truth has been successfully applied to the SQLite cache.
#[derive(Debug)]
pub struct StorageManager {
    layout: RecordingsLayout,
    index: Option<RecordingIndex>,
    retention_policy: RetentionPolicy,
    storage_quota: Option<StorageQuota>,
    reconciled: bool,
    rebuilt_after_corruption: bool,
    playback_pins: PlaybackPins,
}

impl StorageManager {
    pub fn open(
        layout: RecordingsLayout,
        retention_policy: RetentionPolicy,
        storage_quota: Option<StorageQuota>,
    ) -> Result<Self, StorageManagerError> {
        Self::open_with_playback_pins(
            layout,
            retention_policy,
            storage_quota,
            PlaybackPins::default(),
        )
    }

    pub fn open_with_playback_pins(
        layout: RecordingsLayout,
        retention_policy: RetentionPolicy,
        storage_quota: Option<StorageQuota>,
        playback_pins: PlaybackPins,
    ) -> Result<Self, StorageManagerError> {
        validate_retention(retention_policy, storage_quota)?;
        layout.ensure_control_dir()?;
        let index_path = layout.recording_index_path();
        prepare_index_family(&index_path)?;

        match RecordingIndex::open(&index_path) {
            Ok(index) => Ok(Self {
                layout,
                index: Some(index),
                retention_policy,
                storage_quota,
                reconciled: false,
                rebuilt_after_corruption: false,
                playback_pins,
            }),
            Err(error) if error.is_corruption() => {
                quarantine_corrupt_index(&index_path)?;
                let index = RecordingIndex::open(&index_path)?;
                let mut manager = Self {
                    layout,
                    index: Some(index),
                    retention_policy,
                    storage_quota,
                    reconciled: false,
                    rebuilt_after_corruption: true,
                    playback_pins,
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

    fn index(&self) -> Result<&RecordingIndex, StorageManagerError> {
        self.index
            .as_ref()
            .ok_or(StorageManagerError::IndexUnavailable)
    }

    fn index_mut(&mut self) -> Result<&mut RecordingIndex, StorageManagerError> {
        self.index
            .as_mut()
            .ok_or(StorageManagerError::IndexUnavailable)
    }

    pub fn reconcile(&mut self) -> Result<ReconciliationReport, StorageManagerError> {
        self.reconciled = false;
        let inventory = inventory_recordings(&self.layout)?;
        let indexed = match self.index()?.list_all() {
            Ok(indexed) => indexed,
            Err(error) if error.is_corruption() => {
                return self.repair_reconciliation(inventory);
            }
            Err(error) => return Err(error.into()),
        };
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
            let trusted_duration = existing.and_then(|row| {
                same_filesystem_recording(row, recording)
                    .then_some(row.media_duration_ms)
                    .flatten()
            });
            let upsert = recording_to_upsert(recording, trusted_duration);
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
        match self.index_mut()?.apply_reconciliation(&upserts, &removals) {
            Ok(()) => {}
            Err(error) if error.is_corruption() => {
                return self.repair_reconciliation(inventory);
            }
            Err(error) => return Err(error.into()),
        }
        self.reconciled = true;
        Ok(report)
    }

    /// Reconstructs the complete recording index from filesystem facts alone.
    ///
    /// Media duration is deliberately left NULL because filenames and file
    /// sizes do not prove duration.
    pub fn rebuild(&mut self) -> Result<usize, StorageManagerError> {
        self.reconciled = false;
        let inventory = inventory_recordings(&self.layout)?;
        let rows: Vec<_> = inventory
            .recordings
            .iter()
            .map(|recording| recording_to_upsert(recording, None))
            .collect();
        match self.index_mut()?.replace_from_snapshot(&rows) {
            Ok(()) => {}
            Err(error) if error.is_corruption() => {
                return self.repair_index_from_inventory(&inventory);
            }
            Err(error) => return Err(error.into()),
        }
        self.reconciled = true;
        Ok(rows.len())
    }

    /// Explicit application-level repair path for a disposable/corrupt index.
    pub fn repair_index(&mut self) -> Result<usize, StorageManagerError> {
        self.reconciled = false;
        let inventory = inventory_recordings(&self.layout)?;
        self.repair_index_from_inventory(&inventory)
    }

    fn repair_reconciliation(
        &mut self,
        inventory: FilesystemInventory,
    ) -> Result<ReconciliationReport, StorageManagerError> {
        let inserted = self.repair_index_from_inventory(&inventory)?;
        let mut report = ReconciliationReport {
            inserted,
            ignored_foreign: inventory.ignored_foreign,
            ..ReconciliationReport::default()
        };
        classify_partials_by_lease(&self.layout, &inventory, &mut report);
        Ok(report)
    }

    fn repair_index_from_inventory(
        &mut self,
        inventory: &FilesystemInventory,
    ) -> Result<usize, StorageManagerError> {
        self.reconciled = false;
        let rows: Vec<_> = inventory
            .recordings
            .iter()
            .map(|recording| recording_to_upsert(recording, None))
            .collect();
        let index_path = self.layout.recording_index_path();

        drop(self.index.take());
        quarantine_corrupt_index(&index_path)?;
        let mut index = RecordingIndex::open(&index_path)?;
        if let Err(error) = index.replace_from_snapshot(&rows) {
            self.index = Some(index);
            return Err(error.into());
        }
        self.index = Some(index);
        self.reconciled = true;
        self.rebuilt_after_corruption = true;
        Ok(rows.len())
    }

    pub fn list_camera(
        &self,
        camera_id: &CameraId,
    ) -> Result<Vec<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.list_camera(camera_id)?)
    }

    pub fn query_time_range(
        &self,
        camera_id: &CameraId,
        start: NaiveDateTime,
        end: NaiveDateTime,
    ) -> Result<Vec<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.query_time_range(camera_id, start, end)?)
    }

    pub fn find_recording_at(
        &self,
        camera_id: &CameraId,
        timestamp: NaiveDateTime,
    ) -> Result<Option<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.find_recording_at(camera_id, timestamp)?)
    }

    pub fn available_recording_days(
        &self,
        camera_id: &CameraId,
    ) -> Result<Vec<NaiveDate>, StorageManagerError> {
        Ok(self.index()?.available_days(camera_id)?)
    }

    pub fn recording_by_id(
        &self,
        recording_id: &str,
    ) -> Result<Option<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.get_by_relative_path(recording_id)?)
    }

    pub fn previous_recording(
        &self,
        recording: &IndexedRecording,
    ) -> Result<Option<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.previous_recording(recording)?)
    }

    pub fn next_recording(
        &self,
        recording: &IndexedRecording,
    ) -> Result<Option<IndexedRecording>, StorageManagerError> {
        Ok(self.index()?.next_recording(recording)?)
    }

    pub fn write_media_duration(
        &mut self,
        recording: &IndexedRecording,
        media_duration_ms: u64,
    ) -> Result<bool, StorageManagerError> {
        Ok(self
            .index_mut()?
            .update_duration_if_identity_matches(recording, media_duration_ms)?)
    }

    pub fn validate_recording_for_playback(
        &self,
        recording_id: &str,
    ) -> Result<ValidatedRecording, RecordingLookupError> {
        let index = self.index.as_ref().ok_or_else(|| {
            RecordingLookupError::Index("recording index is unavailable".to_owned())
        })?;
        let indexed = index
            .get_by_relative_path(recording_id)?
            .ok_or(RecordingLookupError::NotFound)?;
        if indexed.state != nian_domain::RecordingState::Complete {
            return Err(RecordingLookupError::NotFinalized);
        }
        let path = validate_indexed_playback_path(&self.layout, &indexed)?;
        Ok(ValidatedRecording { indexed, path })
    }

    pub fn total_indexed_recording_bytes(&self) -> Result<u64, StorageManagerError> {
        Ok(self.index()?.total_recording_bytes()?)
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
        let filesystem_started_at = filesystem_identity_datetime(started_at);
        let kind = match classify_recording_file(final_path) {
            RecordingFileKind::NormalRecording => RecordingKind::Normal,
            RecordingFileKind::RecoveredRecording => RecordingKind::Recovered,
            other => {
                return Err(StorageManagerError::InvalidFinalizedRecording(format!(
                    "path {final_path:?} has non-final kind {other:?}"
                )));
            }
        };
        let expected_parent = self.layout.day_dir(camera_id, filesystem_started_at.date());
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
        if identity.started_at != filesystem_started_at.time() {
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
            started_at: filesystem_started_at,
            sequence: identity.sequence,
            size_bytes,
            media_duration_ms,
        };
        match self.index_mut()?.upsert(&upsert) {
            Ok(changed) => Ok(changed),
            Err(error) => {
                if error.is_corruption() {
                    self.reconciled = false;
                }
                Err(error.into())
            }
        }
    }

    /// Runs age/quota retention using an injected local naive wall-clock time.
    ///
    /// Age is interpreted in the same local wall-clock domain encoded by
    /// recording filenames. No UTC claim is made.
    pub fn run_retention(
        &mut self,
        now: NaiveDateTime,
    ) -> Result<RetentionReport, StorageManagerError> {
        self.run_retention_with_delete(now, |path| std::fs::remove_file(path))
    }

    fn run_retention_with_delete<F>(
        &mut self,
        now: NaiveDateTime,
        remove_file: F,
    ) -> Result<RetentionReport, StorageManagerError>
    where
        F: Fn(&Path) -> std::io::Result<()>,
    {
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

        let usage_before: u64 = recordings
            .iter()
            .map(|recording| recording.size_bytes)
            .sum();
        let mut usage = usage_before;
        let quota_triggered = self
            .storage_quota
            .is_some_and(|quota| usage > quota.max_bytes);
        let mut report = RetentionReport {
            examined: recordings.len(),
            quota_triggered,
            usage_before,
            usage_after: usage_before,
            quota_target_reached: !quota_triggered,
            ..RetentionReport::default()
        };
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

            if self.playback_pins.is_pinned(&candidate.relative_path) {
                report.skipped_playback += 1;
                continue;
            }

            match revalidate_candidate(&self.layout, &candidate) {
                Ok(()) => {}
                Err(Revalidation::Missing) => {
                    report.missing += 1;
                    usage = usage.saturating_sub(candidate.size_bytes);
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

            let recovery_transaction = if candidate.kind == RecordingFileKind::RecoveredRecording {
                match inspect_recovered_retention(&candidate.path) {
                    RecoveredRetentionState::Settled {
                        tombstone,
                        evidence,
                    } => Some((tombstone, evidence)),
                    RecoveredRetentionState::Blocked { .. } => {
                        report.blocked_recovery_transactions += 1;
                        continue;
                    }
                    RecoveredRetentionState::InspectionError { path, kind } => {
                        report.failed.push(RetentionFailure {
                            relative_path: candidate.relative_path.clone(),
                            kind: RetentionFailureKind::RecoveryInspection,
                            detail: format!("cannot inspect recovery path {path:?}: {kind:?}"),
                        });
                        continue;
                    }
                }
            } else {
                None
            };

            if let Some((ref tombstone, ref evidence)) = recovery_transaction
                && let Err(failure) =
                    revalidate_recovered_commit(&self.layout, &candidate, tombstone, evidence)
            {
                report.failed.push(failure);
                continue;
            }

            #[cfg(test)]
            if let Ok(gate) = RETENTION_PRE_DELETE_GATE.lock()
                && let Some(gate) = gate.as_ref()
            {
                let _ = gate.reached.send(());
                let _ = gate.resume.recv();
            }

            // Close the plan-to-delete race: playback may have validated and
            // pinned this immutable final after retention selected it.
            if self.playback_pins.is_pinned(&candidate.relative_path) {
                report.skipped_playback += 1;
                continue;
            }

            if let Err(error) = remove_file(&candidate.path) {
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

            if let Some((tombstone, evidence)) = recovery_transaction {
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

            let index_delete = self
                .index_mut()?
                .remove_relative_path(&candidate.relative_path);
            if let Err(error) = index_delete {
                if error.is_corruption() {
                    self.reconciled = false;
                }
                report.failed.push(RetentionFailure {
                    relative_path: candidate.relative_path.clone(),
                    kind: RetentionFailureKind::IndexDelete,
                    detail: error.to_string(),
                });
            }
        }

        report.usage_after = usage;
        report.quota_target_reached = match self.storage_quota {
            Some(quota) if quota_triggered => usage <= quota.cleanup_target_bytes,
            _ => true,
        };
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
                        match cleanup_stale_tombstone(&artifact.path) {
                            TombstoneCleanupOutcome::Removed => report.removed_tombstones += 1,
                            TombstoneCleanupOutcome::PreservedAmbiguous => {
                                report.preserved_ambiguous += 1
                            }
                            TombstoneCleanupOutcome::InspectionFailure => {
                                report.inspection_failures += 1
                            }
                            TombstoneCleanupOutcome::RemoveFailed => report.failed += 1,
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

pub(crate) fn validate_indexed_playback_path(
    layout: &RecordingsLayout,
    indexed: &IndexedRecording,
) -> Result<PathBuf, RecordingLookupError> {
    let relative = Path::new(&indexed.relative_path);
    if relative.is_absolute() {
        return Err(RecordingLookupError::Stale(
            "indexed recording identity is absolute".to_owned(),
        ));
    }

    let parts: Vec<_> = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    if parts.len() != 5 || parts.iter().any(Option::is_none) {
        return Err(RecordingLookupError::Stale(
            "indexed recording identity has invalid path grammar".to_owned(),
        ));
    }
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    let camera = CameraId::parse(&parts[0]).map_err(|_| {
        RecordingLookupError::Stale("indexed recording camera component is invalid".to_owned())
    })?;
    if camera != indexed.camera_id {
        return Err(RecordingLookupError::Stale(
            "indexed recording camera identity changed".to_owned(),
        ));
    }
    let year = parts[1].parse::<i32>().ok();
    let month = parts[2].parse::<u32>().ok();
    let day = parts[3].parse::<u32>().ok();
    let date = match (year, month, day) {
        (Some(year), Some(month), Some(day)) => NaiveDate::from_ymd_opt(year, month, day),
        _ => None,
    }
    .ok_or_else(|| {
        RecordingLookupError::Stale("indexed recording date path is invalid".to_owned())
    })?;
    if date != indexed.started_at.date() {
        return Err(RecordingLookupError::Stale(
            "indexed recording date does not match timeline identity".to_owned(),
        ));
    }

    let filename = &parts[4];
    let Some(identity) = owned_recording_name(filename) else {
        return Err(RecordingLookupError::Stale(
            "indexed recording filename is not canonical".to_owned(),
        ));
    };
    if identity.started_at != indexed.started_at.time() || identity.sequence != indexed.sequence {
        return Err(RecordingLookupError::Stale(
            "indexed recording filename identity changed".to_owned(),
        ));
    }
    let expected_kind = match indexed.kind {
        RecordingKind::Normal => RecordingFileKind::NormalRecording,
        RecordingKind::Recovered => RecordingFileKind::RecoveredRecording,
    };
    if nian_storage::classify_recording_file_name(filename) != expected_kind {
        return Err(RecordingLookupError::Stale(
            "indexed recording kind does not match filename".to_owned(),
        ));
    }

    let root_metadata = match std::fs::symlink_metadata(layout.root()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RecordingLookupError::Missing);
        }
        Err(_) => {
            return Err(RecordingLookupError::Stale(
                "recording root cannot be inspected".to_owned(),
            ));
        }
    };
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(RecordingLookupError::Stale(
            "recording root is not a real directory".to_owned(),
        ));
    }

    let mut path = layout.root().to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(RecordingLookupError::Missing);
            }
            Err(_) => {
                return Err(RecordingLookupError::Stale(
                    "recording path cannot be inspected".to_owned(),
                ));
            }
        };
        let is_last = index + 1 == parts.len();
        if metadata.file_type().is_symlink()
            || (is_last && !metadata.is_file())
            || (!is_last && !metadata.is_dir())
        {
            return Err(RecordingLookupError::Stale(
                "recording path contains a symlink or wrong object type".to_owned(),
            ));
        }
        if is_last && metadata.len() != indexed.size_bytes {
            return Err(RecordingLookupError::Stale(
                "recording size changed since indexing".to_owned(),
            ));
        }
    }
    Ok(path)
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

fn same_filesystem_recording(row: &IndexedRecording, recording: &InventoryRecording) -> bool {
    let kind = match recording.kind {
        RecordingFileKind::NormalRecording => RecordingKind::Normal,
        RecordingFileKind::RecoveredRecording => RecordingKind::Recovered,
        _ => return false,
    };

    row.camera_id == recording.camera_id
        && row.relative_path == recording.relative_path
        && row.kind == kind
        && row.state == nian_domain::RecordingState::Complete
        && row.started_at == recording.started_at
        && row.sequence == recording.sequence
        && row.size_bytes == recording.size_bytes
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

fn revalidate_recovered_commit(
    layout: &RecordingsLayout,
    candidate: &InventoryRecording,
    expected_tombstone: &Path,
    expected_evidence: &RecoveryTombstone,
) -> Result<(), RetentionFailure> {
    if let Err(error) = revalidate_candidate(layout, candidate) {
        let detail = match error {
            Revalidation::Missing => "recovered final disappeared before commit".to_owned(),
            Revalidation::Changed(detail) => detail,
        };
        return Err(RetentionFailure {
            relative_path: candidate.relative_path.clone(),
            kind: RetentionFailureKind::Revalidation,
            detail,
        });
    }

    match inspect_recovered_retention(&candidate.path) {
        RecoveredRetentionState::Settled {
            tombstone,
            evidence,
        } if tombstone == expected_tombstone
            && evidence == *expected_evidence
            && evidence.size_bytes == candidate.size_bytes =>
        {
            Ok(())
        }
        RecoveredRetentionState::Settled { .. } => Err(RetentionFailure {
            relative_path: candidate.relative_path.clone(),
            kind: RetentionFailureKind::TombstoneRevalidation,
            detail: "recovery transaction evidence changed before final deletion".to_owned(),
        }),
        RecoveredRetentionState::Blocked { reason } => Err(RetentionFailure {
            relative_path: candidate.relative_path.clone(),
            kind: RetentionFailureKind::TombstoneRevalidation,
            detail: format!("recovery transaction became blocked before commit: {reason}"),
        }),
        RecoveredRetentionState::InspectionError { path, kind } => Err(RetentionFailure {
            relative_path: candidate.relative_path.clone(),
            kind: RetentionFailureKind::RecoveryInspection,
            detail: format!("cannot re-inspect recovery path {path:?}: {kind:?}"),
        }),
    }
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

fn prepare_index_family(index_path: &Path) -> Result<(), StorageManagerError> {
    nian_storage::prepare_sqlite_family(index_path, MAX_RECORDING_INDEX_CORRUPT_BACKUPS)?;
    Ok(())
}

fn quarantine_corrupt_index(index_path: &Path) -> Result<(), StorageManagerError> {
    nian_storage::quarantine_sqlite_family(index_path, MAX_RECORDING_INDEX_CORRUPT_BACKUPS)?;
    Ok(())
}

#[cfg(test)]
fn quarantine_pending_marker(index_path: &Path) -> Result<PathBuf, StorageManagerError> {
    Ok(nian_storage::quarantine_marker_path(index_path)?)
}

#[cfg(test)]
fn corrupt_target_path(source: &Path, serial: u32) -> Result<PathBuf, StorageManagerError> {
    Ok(nian_storage::quarantine_target_path(source, serial)?)
}

#[cfg(test)]
fn sqlite_sidecar_path(index_path: &Path, suffix: &str) -> Result<PathBuf, StorageManagerError> {
    Ok(nian_storage::sqlite_sidecar_path(index_path, suffix)?)
}

#[cfg(test)]
fn corrupt_serial_from_name(name: &str, sources: &[PathBuf]) -> Option<u32> {
    sources.iter().find_map(|source| {
        let file_name = source.file_name()?.to_string_lossy();
        let suffix = name.strip_prefix(&format!("{file_name}.corrupt-"))?;
        let serial = suffix.parse::<u32>().ok()?;
        (serial != 0).then_some(serial)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TombstoneCleanupOutcome {
    Removed,
    PreservedAmbiguous,
    InspectionFailure,
    RemoveFailed,
}

fn cleanup_stale_tombstone(path: &Path) -> TombstoneCleanupOutcome {
    cleanup_stale_tombstone_with(path, &|candidate| inspect_path_presence(candidate))
}

fn cleanup_stale_tombstone_with<F>(path: &Path, presence: &F) -> TombstoneCleanupOutcome
where
    F: Fn(&Path) -> PathPresence,
{
    let Some(final_name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".done"))
    else {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    };
    let final_path = path.with_file_name(final_name);
    let Some(paths) = recovery_transaction_paths(&final_path) else {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    };

    for candidate in [&paths.recovered_final, &paths.original_partial] {
        match presence(candidate) {
            PathPresence::Present(_) => return TombstoneCleanupOutcome::PreservedAmbiguous,
            PathPresence::Absent => {}
            PathPresence::Uninspectable(_) => {
                return TombstoneCleanupOutcome::InspectionFailure;
            }
        }
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TombstoneCleanupOutcome::PreservedAmbiguous;
        }
        Err(_) => return TombstoneCleanupOutcome::InspectionFailure,
    };
    let Some(transaction) = parse_recovery_tombstone(&bytes) else {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    };
    let Some(original_name) = paths
        .original_partial
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    };
    let Some(recovered_name) = paths
        .recovered_final
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    };
    if transaction.original != original_name || transaction.final_name != recovered_name {
        return TombstoneCleanupOutcome::PreservedAmbiguous;
    }
    match std::fs::remove_file(path) {
        Ok(()) => TombstoneCleanupOutcome::Removed,
        Err(_) => TombstoneCleanupOutcome::RemoveFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};

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

    #[test]
    fn filesystem_deletion_failure_keeps_index_row_and_media() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let started_at =
            NaiveDateTime::parse_from_str("2026-08-20T08:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let media = layout
            .day_dir(&camera, started_at.date())
            .join("08-30-00.mkv");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(&media, b"footage").unwrap();

        let mut manager = StorageManager::open(
            layout,
            RetentionPolicy {
                max_age_days: Some(1),
                max_storage_bytes: None,
            },
            None,
        )
        .unwrap();
        manager.reconcile().unwrap();

        let report = manager
            .run_retention_with_delete(
                NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
                |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            )
            .unwrap();

        assert_eq!(report.deleted, 0);
        assert!(
            report
                .failed
                .iter()
                .any(|failure| failure.kind == RetentionFailureKind::FilesystemDelete)
        );
        assert!(media.exists());
        assert_eq!(manager.list_camera(&camera).unwrap().len(), 1);
    }

    #[test]
    fn stale_tombstone_inspection_error_preserves_marker() {
        let temp = tempfile::tempdir().unwrap();
        let tombstone = temp.path().join("08-30-00.recovered.mkv.done");
        std::fs::write(
            &tombstone,
            nian_storage::recovery_tombstone_payload(
                "08-30-00.partial.mkv",
                "08-30-00.recovered.mkv",
                7,
            ),
        )
        .unwrap();
        let final_path = temp.path().join("08-30-00.recovered.mkv");

        let outcome = cleanup_stale_tombstone_with(&tombstone, &|candidate| {
            if candidate == final_path {
                PathPresence::Uninspectable(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))
            } else {
                inspect_path_presence(candidate)
            }
        });

        assert_eq!(outcome, TombstoneCleanupOutcome::InspectionFailure);
        assert!(tombstone.is_file());
    }

    #[test]
    fn half_quarantined_sqlite_family_converges_before_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let index_path = temp.path().join("recordings.sqlite3");
        let wal = sqlite_sidecar_path(&index_path, "-wal").unwrap();
        let shm = sqlite_sidecar_path(&index_path, "-shm").unwrap();
        let marker = quarantine_pending_marker(&index_path).unwrap();

        let main_corrupt_1 = corrupt_target_path(&index_path, 1).unwrap();
        std::fs::write(&main_corrupt_1, b"old main evidence").unwrap();
        std::fs::write(&wal, b"old wal").unwrap();
        std::fs::write(&shm, b"old shm").unwrap();
        std::fs::write(&marker, b"NIAN-SQLITE-QUARANTINE v1\nserial: 2\n").unwrap();

        prepare_index_family(&index_path).unwrap();

        assert!(!index_path.exists());
        assert!(!wal.exists());
        assert!(!shm.exists());
        assert!(!marker.exists());
        assert_eq!(std::fs::read(main_corrupt_1).unwrap(), b"old main evidence");
        assert_eq!(
            std::fs::read(corrupt_target_path(&wal, 2).unwrap()).unwrap(),
            b"old wal"
        );
        assert_eq!(
            std::fs::read(corrupt_target_path(&shm, 2).unwrap()).unwrap(),
            b"old shm"
        );
    }

    #[test]
    fn repeated_recording_index_quarantine_keeps_only_a_bounded_backup_set() {
        let temp = tempfile::tempdir().unwrap();
        let index_path = temp.path().join("recordings.sqlite3");
        let wal = sqlite_sidecar_path(&index_path, "-wal").unwrap();
        let shm = sqlite_sidecar_path(&index_path, "-shm").unwrap();
        let sources = [index_path.clone(), wal, shm];

        for generation in 0..(MAX_RECORDING_INDEX_CORRUPT_BACKUPS + 3) {
            std::fs::write(&index_path, format!("corrupt-{generation}")).unwrap();
            quarantine_corrupt_index(&index_path).unwrap();
        }

        let mut serials = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                corrupt_serial_from_name(&name.to_string_lossy(), &sources)
            })
            .collect::<Vec<_>>();
        serials.sort_unstable();
        serials.dedup();
        assert_eq!(serials.len(), MAX_RECORDING_INDEX_CORRUPT_BACKUPS);
        assert!(serials.iter().all(|serial| *serial >= 4));
    }

    #[test]
    fn playback_pin_skips_retention_until_the_session_releases_it() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let started_at =
            NaiveDateTime::parse_from_str("2026-08-20T08:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let media = layout
            .day_dir(&camera, started_at.date())
            .join("08-30-00.mkv");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(&media, b"pinned footage").unwrap();
        let relative = "cam-a/2026/08/20/08-30-00.mkv";
        let pins = PlaybackPins::default();
        let mut manager = StorageManager::open_with_playback_pins(
            layout,
            RetentionPolicy {
                max_age_days: Some(1),
                max_storage_bytes: None,
            },
            None,
            pins.clone(),
        )
        .unwrap();
        manager.reconcile().unwrap();

        let pin = pins.pin(relative);
        let report = manager
            .run_retention(
                NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
            )
            .unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_playback, 1);
        assert!(media.is_file());
        assert!(manager.recording_by_id(relative).unwrap().is_some());

        drop(pin);
        let report = manager
            .run_retention(
                NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
            )
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!media.exists());
        assert!(manager.recording_by_id(relative).unwrap().is_none());
    }

    #[test]
    fn concurrent_camera_leases_do_not_block_pinned_playback_or_global_retention() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera_a = CameraId::parse("cam-a").unwrap();
        let camera_b = CameraId::parse("cam-b").unwrap();
        let started_at =
            NaiveDateTime::parse_from_str("2026-08-20T08:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let final_a = layout
            .day_dir(&camera_a, started_at.date())
            .join("08-30-00.mkv");
        let final_b = layout
            .day_dir(&camera_b, started_at.date())
            .join("08-30-00.mkv");
        let partial_a = layout
            .day_dir(&camera_a, started_at.date())
            .join("09-00-00.partial.mkv");
        let partial_b = layout
            .day_dir(&camera_b, started_at.date())
            .join("09-00-00.partial.mkv");
        for path in [&final_a, &final_b, &partial_a, &partial_b] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"camera-scoped-media").unwrap();
        }

        let lease_a = CameraLease::try_acquire(&layout, &camera_a).unwrap();
        let lease_b = CameraLease::try_acquire(&layout, &camera_b).unwrap();
        let pins = PlaybackPins::default();
        let mut manager = StorageManager::open_with_playback_pins(
            layout,
            RetentionPolicy {
                max_age_days: Some(1),
                max_storage_bytes: None,
            },
            None,
            pins.clone(),
        )
        .unwrap();
        let reconciliation = manager.reconcile().unwrap();
        assert_eq!(reconciliation.active_partials, 2);

        let relative_a = "cam-a/2026/08/20/08-30-00.mkv";
        let pin = pins.pin(relative_a);
        let report = manager
            .run_retention(
                NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
            )
            .unwrap();

        assert_eq!(report.skipped_playback, 1);
        assert_eq!(report.deleted, 1);
        assert!(final_a.is_file());
        assert!(!final_b.exists());
        assert!(partial_a.is_file());
        assert!(partial_b.is_file());
        assert!(manager.recording_by_id(relative_a).unwrap().is_some());
        assert!(
            manager
                .recording_by_id("cam-b/2026/08/20/08-30-00.mkv")
                .unwrap()
                .is_none()
        );

        drop(pin);
        drop(lease_a);
        drop(lease_b);
    }

    #[test]
    fn playback_pin_created_after_retention_planning_wins_before_delete() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let started_at =
            NaiveDateTime::parse_from_str("2026-08-20T08:30:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let media = layout
            .day_dir(&camera, started_at.date())
            .join("08-30-00.mkv");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(&media, b"race footage").unwrap();
        let relative = "cam-a/2026/08/20/08-30-00.mkv";
        let pins = PlaybackPins::default();
        let mut manager = StorageManager::open_with_playback_pins(
            layout,
            RetentionPolicy {
                max_age_days: Some(1),
                max_storage_bytes: None,
            },
            None,
            pins.clone(),
        )
        .unwrap();
        manager.reconcile().unwrap();

        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        *RETENTION_PRE_DELETE_GATE.lock().unwrap() = Some(RetentionTestGate {
            reached: reached_tx,
            resume: resume_rx,
        });
        let worker = std::thread::spawn(move || {
            let report = manager
                .run_retention(
                    NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S")
                        .unwrap(),
                )
                .unwrap();
            (manager, report)
        });
        reached_rx.recv().unwrap();
        let pin = pins.pin(relative);
        resume_tx.send(()).unwrap();
        let (mut manager, report) = worker.join().unwrap();
        *RETENTION_PRE_DELETE_GATE.lock().unwrap() = None;

        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped_playback, 1);
        assert!(media.is_file());
        drop(pin);
        assert_eq!(
            manager
                .run_retention(
                    NaiveDateTime::parse_from_str("2026-08-29T12:00:00", "%Y-%m-%dT%H:%M:%S",)
                        .unwrap(),
                )
                .unwrap()
                .deleted,
            1
        );
    }
}
