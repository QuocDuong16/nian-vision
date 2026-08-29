//! Deterministic, symlink-safe inventory of canonical recording trees.

use std::fs::DirEntry;
use std::path::PathBuf;

use chrono::{NaiveDate, NaiveDateTime};
use nian_domain::CameraId;

use crate::classification::owned_recording_name;
use crate::{RecordingFileKind, RecordingsLayout, StorageError, classify_recording_file_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRecording {
    pub camera_id: CameraId,
    pub relative_path: String,
    pub path: PathBuf,
    pub kind: RecordingFileKind,
    /// Local naive wall-clock identity from directory date + canonical filename.
    pub started_at: NaiveDateTime,
    pub sequence: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryPartial {
    pub camera_id: CameraId,
    pub relative_path: String,
    pub path: PathBuf,
    pub started_at: NaiveDateTime,
    pub sequence: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryArtifact {
    pub camera_id: CameraId,
    pub path: PathBuf,
    pub kind: RecordingFileKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilesystemInventory {
    pub recordings: Vec<InventoryRecording>,
    pub partials: Vec<InventoryPartial>,
    pub artifacts: Vec<InventoryArtifact>,
    pub ignored_foreign: usize,
}

/// Scans exactly `<root>/<camera>/<YYYY>/<MM>/<DD>/<file>`.
///
/// Directory symlinks are never followed, `.nian` cannot parse as CameraId,
/// and only regular canonical recording files enter `recordings`.
pub fn inventory_recordings(
    layout: &RecordingsLayout,
) -> Result<FilesystemInventory, StorageError> {
    let mut inventory = FilesystemInventory::default();
    let root = layout.root();
    let root_metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(inventory),
        Err(source) => {
            return Err(StorageError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    if !root_metadata.is_dir() {
        return Err(StorageError::InvalidRoot {
            reason: "storage root exists but is not a directory".to_owned(),
        });
    }

    for camera_entry in read_dir_sorted(root)? {
        if !entry_is_real_directory(&camera_entry)? {
            inventory.ignored_foreign += 1;
            continue;
        }
        let Some(camera_name) = camera_entry.file_name().to_str().map(ToOwned::to_owned) else {
            inventory.ignored_foreign += 1;
            continue;
        };
        let Ok(camera_id) = CameraId::parse(&camera_name) else {
            inventory.ignored_foreign += 1;
            continue;
        };

        for year_entry in read_dir_sorted(&camera_entry.path())? {
            let Some(year) = parse_component(&year_entry, 4, 1, 9999)? else {
                inventory.ignored_foreign += 1;
                continue;
            };
            for month_entry in read_dir_sorted(&year_entry.path())? {
                let Some(month) = parse_component(&month_entry, 2, 1, 12)? else {
                    inventory.ignored_foreign += 1;
                    continue;
                };
                for day_entry in read_dir_sorted(&month_entry.path())? {
                    let Some(day) = parse_component(&day_entry, 2, 1, 31)? else {
                        inventory.ignored_foreign += 1;
                        continue;
                    };
                    let Some(date) = NaiveDate::from_ymd_opt(year as i32, month, day) else {
                        inventory.ignored_foreign += 1;
                        continue;
                    };
                    scan_day(&camera_id, date, &day_entry.path(), &mut inventory)?;
                }
            }
        }
    }

    inventory
        .recordings
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    inventory
        .partials
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    inventory.artifacts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inventory)
}

fn scan_day(
    camera_id: &CameraId,
    date: NaiveDate,
    day_dir: &std::path::Path,
    inventory: &mut FilesystemInventory,
) -> Result<(), StorageError> {
    for entry in read_dir_sorted(day_dir)? {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            inventory.ignored_foreign += 1;
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            inventory.ignored_foreign += 1;
            continue;
        };
        let kind = classify_recording_file_name(&name);
        let Some(identity) = owned_recording_name(&name) else {
            inventory.ignored_foreign += 1;
            continue;
        };
        let started_at = date.and_time(identity.started_at);
        let relative_path = format!(
            "{}/{:04}/{:02}/{:02}/{}",
            camera_id.as_str(),
            date.format("%Y"),
            date.format("%m"),
            date.format("%d"),
            name
        );

        match kind {
            RecordingFileKind::NormalRecording | RecordingFileKind::RecoveredRecording => {
                inventory.recordings.push(InventoryRecording {
                    camera_id: camera_id.clone(),
                    relative_path,
                    path,
                    kind,
                    started_at,
                    sequence: identity.sequence,
                    size_bytes: metadata.len(),
                });
            }
            RecordingFileKind::ActiveOrCrashPartial => {
                inventory.partials.push(InventoryPartial {
                    camera_id: camera_id.clone(),
                    relative_path,
                    path,
                    started_at,
                    sequence: identity.sequence,
                });
            }
            RecordingFileKind::RecoveryScratch | RecordingFileKind::RecoveryTombstone => {
                inventory.artifacts.push(InventoryArtifact {
                    camera_id: camera_id.clone(),
                    path,
                    kind,
                });
            }
            RecordingFileKind::Unknown => inventory.ignored_foreign += 1,
        }
    }
    Ok(())
}

fn parse_component(
    entry: &DirEntry,
    width: usize,
    min: u32,
    max: u32,
) -> Result<Option<u32>, StorageError> {
    if !entry_is_real_directory(entry)? {
        return Ok(None);
    }
    let name = entry.file_name();
    let Some(value) = name.to_str() else {
        return Ok(None);
    };
    if value.len() != width || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let Ok(number) = value.parse::<u32>() else {
        return Ok(None);
    };
    Ok((min..=max).contains(&number).then_some(number))
}

fn entry_is_real_directory(entry: &DirEntry) -> Result<bool, StorageError> {
    let path = entry.path();
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|source| StorageError::Io { path, source })?;
    Ok(metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn read_dir_sorted(path: &std::path::Path) -> Result<Vec<DirEntry>, StorageError> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(DirEntry::file_name);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventories_normal_recovered_partial_and_ignores_control_foreign() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let day = layout.day_dir(&camera, NaiveDate::from_ymd_opt(2026, 8, 29).unwrap());
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("08-30-00.mkv"), b"normal").unwrap();
        std::fs::write(day.join("08-30-01.recovered.mkv"), b"recovered").unwrap();
        std::fs::write(day.join("08-30-02.partial.mkv"), b"partial").unwrap();
        std::fs::write(day.join("08-30-01.recovered.mkv.done"), b"marker").unwrap();
        std::fs::write(day.join(".nian-camera.lock"), b"").unwrap();
        std::fs::write(day.join("notes.txt"), b"foreign").unwrap();

        let inventory = inventory_recordings(&layout).unwrap();
        assert_eq!(inventory.recordings.len(), 2);
        assert_eq!(inventory.partials.len(), 1);
        assert_eq!(inventory.artifacts.len(), 1);
        assert!(inventory.ignored_foreign >= 2);
        assert_eq!(
            inventory.recordings[0].relative_path,
            "cam-a/2026/08/29/08-30-00.mkv"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recording_looking_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-a").unwrap();
        let day = layout.day_dir(&camera, NaiveDate::from_ymd_opt(2026, 8, 29).unwrap());
        std::fs::create_dir_all(&day).unwrap();
        let outside = temp.path().join("outside.mkv");
        std::fs::write(&outside, b"do not follow").unwrap();
        symlink(&outside, day.join("08-30-00.mkv")).unwrap();

        let inventory = inventory_recordings(&layout).unwrap();
        assert!(inventory.recordings.is_empty());
        assert_eq!(std::fs::read(outside).unwrap(), b"do not follow");
    }
}
