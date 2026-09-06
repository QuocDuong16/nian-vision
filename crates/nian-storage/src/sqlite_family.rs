//! Restart-convergent quarantine for rebuildable SQLite index families.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::paths::publish_no_replace;
use crate::{PathPresence, StorageError, inspect_path_presence};

const MARKER_MAGIC: &str = "NIAN-SQLITE-QUARANTINE v1";
const LEGACY_MARKER: &str = "pending";
const MAX_MARKER_BYTES: u64 = 128;
const MAX_SERIAL_ALLOCATION_ATTEMPTS: u32 = 100;
const MARKER_UPGRADE_SUFFIX: &str = ".upgrade";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingMarker {
    Current(u32),
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteFamilyQuarantine {
    pub serial: u32,
    pub main_backup: PathBuf,
    pub wal_backup: PathBuf,
    pub shm_backup: PathBuf,
}

impl SqliteFamilyQuarantine {
    pub fn evidence_path(&self) -> PathBuf {
        if self.main_backup.is_file() {
            self.main_backup.clone()
        } else if self.wal_backup.is_file() {
            self.wal_backup.clone()
        } else {
            self.shm_backup.clone()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SqliteFamilyError {
    #[error("SQLite family path has no filename or parent: {0:?}")]
    InvalidPath(PathBuf),
    #[error("SQLite quarantine marker is malformed: {0:?}")]
    InvalidMarker(PathBuf),
    #[error("SQLite quarantine target already exists while canonical source remains: {0:?}")]
    Collision(PathBuf),
    #[error("canonical SQLite family member remained after quarantine: {0:?}")]
    CanonicalMemberRemained(PathBuf),
    #[error("cannot allocate SQLite quarantine generation")]
    AllocationExhausted,
    #[error("SQLite quarantine I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[cfg(any(test, feature = "test-hooks"))]
    #[error("injected SQLite quarantine interruption")]
    InjectedInterruption,
}

pub fn prepare_sqlite_family(
    main: &Path,
    max_backups: usize,
) -> Result<Option<SqliteFamilyQuarantine>, SqliteFamilyError> {
    let family = SqliteFamilyPaths::new(main)?;
    if let Some(marker) = read_pending_marker(&family.marker)? {
        let serial = resolve_pending_serial(&family, marker)?;
        prune_backups(
            &family,
            max_backups.saturating_sub(1),
            Some(BackupGeneration::Numeric(serial)),
        )?;
        return settle_generation(&family, serial).map(Some);
    }

    prune_backups(&family, max_backups, None)?;
    let main_present = path_present(&family.main)?;
    let wal_present = path_present(&family.wal)?;
    let shm_present = path_present(&family.shm)?;
    if !main_present && (wal_present || shm_present) {
        return quarantine_sqlite_family(main, max_backups).map(Some);
    }
    Ok(None)
}

pub fn quarantine_sqlite_family(
    main: &Path,
    max_backups: usize,
) -> Result<SqliteFamilyQuarantine, SqliteFamilyError> {
    let family = SqliteFamilyPaths::new(main)?;
    let serial = match read_pending_marker(&family.marker)? {
        Some(marker) => resolve_pending_serial(&family, marker)?,
        None => {
            prune_backups(&family, max_backups.saturating_sub(1), None)?;
            let serial = allocate_serial(&family)?;
            create_pending_marker(&family.marker, serial)?;
            serial
        }
    };
    prune_backups(
        &family,
        max_backups.saturating_sub(1),
        Some(BackupGeneration::Numeric(serial)),
    )?;
    settle_generation(&family, serial)
}

fn settle_generation(
    family: &SqliteFamilyPaths,
    serial: u32,
) -> Result<SqliteFamilyQuarantine, SqliteFamilyError> {
    let report = family.report(serial)?;

    for (source, target) in [
        (&family.wal, &report.wal_backup),
        (&family.shm, &report.shm_backup),
        (&family.main, &report.main_backup),
    ] {
        match inspect_path_presence(source) {
            PathPresence::Present(_) => {
                match inspect_path_presence(target) {
                    PathPresence::Absent => {}
                    PathPresence::Present(_) => {
                        if same_file_identity(source, target)? {
                            remove_canonical_link(source)?;
                            #[cfg(any(test, feature = "test-hooks"))]
                            if crate::test_hooks::sqlite_quarantine_interrupt_after_move(
                                &family.main,
                            ) {
                                return Err(SqliteFamilyError::InjectedInterruption);
                            }
                            continue;
                        }
                        return Err(SqliteFamilyError::Collision(target.clone()));
                    }
                    PathPresence::Uninspectable(source) => {
                        return Err(io_error(target, source));
                    }
                }

                #[cfg(any(test, feature = "test-hooks"))]
                if crate::test_hooks::sqlite_quarantine_take_partial_publication(source) {
                    std::fs::hard_link(source, target)
                        .map_err(|source| io_error(target, source))?;
                    return Err(SqliteFamilyError::InjectedInterruption);
                }

                if let Err(error) = publish_no_replace(source, target) {
                    if path_present(source)? && path_present(target)? {
                        if same_file_identity(source, target)? {
                            remove_canonical_link(source)?;
                        } else {
                            return Err(SqliteFamilyError::Collision(target.clone()));
                        }
                    } else {
                        return Err(error.into());
                    }
                }
                #[cfg(any(test, feature = "test-hooks"))]
                if crate::test_hooks::sqlite_quarantine_interrupt_after_move(&family.main) {
                    return Err(SqliteFamilyError::InjectedInterruption);
                }
            }
            PathPresence::Absent => {}
            PathPresence::Uninspectable(source_error) => {
                return Err(io_error(source, source_error));
            }
        }
    }

    for source in [&family.main, &family.wal, &family.shm] {
        match inspect_path_presence(source) {
            PathPresence::Absent => {}
            PathPresence::Present(_) => {
                return Err(SqliteFamilyError::CanonicalMemberRemained(source.clone()));
            }
            PathPresence::Uninspectable(source_error) => {
                return Err(io_error(source, source_error));
            }
        }
    }

    match std::fs::remove_file(&family.marker) {
        Ok(()) => Ok(report),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(report),
        Err(source) => Err(io_error(&family.marker, source)),
    }
}

fn remove_canonical_link(source: &Path) -> Result<(), SqliteFamilyError> {
    std::fs::remove_file(source).map_err(|error| io_error(source, error))
}

pub fn quarantine_marker_path(main: &Path) -> Result<PathBuf, SqliteFamilyError> {
    SqliteFamilyPaths::new(main).map(|family| family.marker)
}

pub fn quarantine_target_path(
    main_or_sidecar: &Path,
    serial: u32,
) -> Result<PathBuf, SqliteFamilyError> {
    if serial == 0 {
        return Err(SqliteFamilyError::AllocationExhausted);
    }
    let Some(file_name) = main_or_sidecar.file_name() else {
        return Err(SqliteFamilyError::InvalidPath(
            main_or_sidecar.to_path_buf(),
        ));
    };
    let mut name = file_name.to_os_string();
    name.push(format!(".corrupt-{serial}"));
    Ok(main_or_sidecar.with_file_name(name))
}

pub fn sqlite_sidecar_path(main: &Path, suffix: &str) -> Result<PathBuf, SqliteFamilyError> {
    let Some(file_name) = main.file_name() else {
        return Err(SqliteFamilyError::InvalidPath(main.to_path_buf()));
    };
    let mut name = file_name.to_os_string();
    name.push(suffix);
    Ok(main.with_file_name(name))
}

struct SqliteFamilyPaths {
    main: PathBuf,
    wal: PathBuf,
    shm: PathBuf,
    marker: PathBuf,
    parent: PathBuf,
}

impl SqliteFamilyPaths {
    fn new(main: &Path) -> Result<Self, SqliteFamilyError> {
        let parent = main
            .parent()
            .ok_or_else(|| SqliteFamilyError::InvalidPath(main.to_path_buf()))?;
        let wal = sqlite_sidecar_path(main, "-wal")?;
        let shm = sqlite_sidecar_path(main, "-shm")?;
        let marker = sqlite_sidecar_path(main, ".quarantine-pending")?;
        Ok(Self {
            main: main.to_path_buf(),
            wal,
            shm,
            marker,
            parent: parent.to_path_buf(),
        })
    }

    fn sources(&self) -> [&PathBuf; 3] {
        [&self.main, &self.wal, &self.shm]
    }

    fn report(&self, serial: u32) -> Result<SqliteFamilyQuarantine, SqliteFamilyError> {
        Ok(SqliteFamilyQuarantine {
            serial,
            main_backup: quarantine_target_path(&self.main, serial)?,
            wal_backup: quarantine_target_path(&self.wal, serial)?,
            shm_backup: quarantine_target_path(&self.shm, serial)?,
        })
    }
}

fn read_pending_marker(marker: &Path) -> Result<Option<PendingMarker>, SqliteFamilyError> {
    match inspect_path_presence(marker) {
        PathPresence::Absent => Ok(None),
        PathPresence::Present(metadata) if metadata.is_file() => {
            if metadata.len() > MAX_MARKER_BYTES {
                return Err(SqliteFamilyError::InvalidMarker(marker.to_path_buf()));
            }
            let text =
                std::fs::read_to_string(marker).map_err(|source| io_error(marker, source))?;
            if text == LEGACY_MARKER || text == format!("{LEGACY_MARKER}\n") {
                return Ok(Some(PendingMarker::Legacy));
            }
            parse_current_marker(&text)
                .map(PendingMarker::Current)
                .map(Some)
                .ok_or_else(|| SqliteFamilyError::InvalidMarker(marker.to_path_buf()))
        }
        PathPresence::Present(_) => Err(SqliteFamilyError::InvalidMarker(marker.to_path_buf())),
        PathPresence::Uninspectable(source) => Err(io_error(marker, source)),
    }
}

fn parse_current_marker(text: &str) -> Option<u32> {
    let mut lines = text.lines();
    if lines.next()? != MARKER_MAGIC {
        return None;
    }
    let serial = lines
        .next()?
        .strip_prefix("serial: ")?
        .parse::<u32>()
        .ok()?;
    if serial == 0 || lines.next().is_some() {
        return None;
    }
    Some(serial)
}

fn create_pending_marker(marker: &Path, serial: u32) -> Result<(), SqliteFamilyError> {
    match create_marker_file(marker, serial) {
        Ok(()) => Ok(()),
        Err(SqliteFamilyError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            match read_pending_marker(marker)? {
                Some(PendingMarker::Current(existing)) if existing == serial => Ok(()),
                _ => Err(SqliteFamilyError::Collision(marker.to_path_buf())),
            }
        }
        Err(error) => Err(error),
    }
}

fn create_marker_file(path: &Path, serial: u32) -> Result<(), SqliteFamilyError> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    write!(file, "{MARKER_MAGIC}\nserial: {serial}\n")
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(path, source))
}

fn resolve_pending_serial(
    family: &SqliteFamilyPaths,
    marker: PendingMarker,
) -> Result<u32, SqliteFamilyError> {
    match marker {
        PendingMarker::Current(serial) => Ok(serial),
        PendingMarker::Legacy => upgrade_legacy_marker(family),
    }
}

fn upgrade_legacy_marker(family: &SqliteFamilyPaths) -> Result<u32, SqliteFamilyError> {
    let upgrade = sqlite_sidecar_path(&family.marker, MARKER_UPGRADE_SUFFIX)?;
    let serial = match read_pending_marker(&upgrade)? {
        Some(PendingMarker::Current(serial)) if generation_available(family, serial)? => serial,
        Some(PendingMarker::Current(_)) => {
            std::fs::remove_file(&upgrade).map_err(|source| io_error(&upgrade, source))?;
            let serial = allocate_serial(family)?;
            create_marker_file(&upgrade, serial)?;
            serial
        }
        Some(PendingMarker::Legacy) => {
            return Err(SqliteFamilyError::InvalidMarker(upgrade));
        }
        None => {
            let serial = allocate_serial(family)?;
            create_marker_file(&upgrade, serial)?;
            serial
        }
    };

    if !generation_available(family, serial)? {
        return Err(SqliteFamilyError::Collision(
            family.report(serial)?.main_backup,
        ));
    }
    replace_marker_file(&upgrade, &family.marker)?;
    Ok(serial)
}

fn generation_available(
    family: &SqliteFamilyPaths,
    serial: u32,
) -> Result<bool, SqliteFamilyError> {
    let report = family.report(serial)?;
    for target in [&report.main_backup, &report.wal_backup, &report.shm_backup] {
        match inspect_path_presence(target) {
            PathPresence::Absent => {}
            PathPresence::Present(_) => return Ok(false),
            PathPresence::Uninspectable(source) => return Err(io_error(target, source)),
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn replace_marker_file(staged: &Path, marker: &Path) -> Result<(), SqliteFamilyError> {
    std::fs::rename(staged, marker).map_err(|source| io_error(marker, source))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_marker_file(staged: &Path, marker: &Path) -> Result<(), SqliteFamilyError> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        let mut buffer: Vec<u16> = path.as_os_str().encode_wide().collect();
        buffer.push(0);
        buffer
    }

    let staged_wide = wide(staged);
    let marker_wide = wide(marker);
    let succeeded = unsafe {
        MoveFileExW(
            staged_wide.as_ptr(),
            marker_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded != 0 {
        Ok(())
    } else {
        Err(io_error(marker, std::io::Error::last_os_error()))
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_marker_file(staged: &Path, marker: &Path) -> Result<(), SqliteFamilyError> {
    std::fs::rename(staged, marker).map_err(|source| io_error(marker, source))
}

fn allocate_serial(family: &SqliteFamilyPaths) -> Result<u32, SqliteFamilyError> {
    let highest = highest_serial(family)?;
    let first = highest
        .checked_add(1)
        .ok_or(SqliteFamilyError::AllocationExhausted)?;
    for offset in 0..MAX_SERIAL_ALLOCATION_ATTEMPTS {
        let serial = first
            .checked_add(offset)
            .ok_or(SqliteFamilyError::AllocationExhausted)?;
        let report = family.report(serial)?;
        let mut available = true;
        for target in [&report.main_backup, &report.wal_backup, &report.shm_backup] {
            match inspect_path_presence(target) {
                PathPresence::Absent => {}
                PathPresence::Present(_) => {
                    available = false;
                    break;
                }
                PathPresence::Uninspectable(source) => return Err(io_error(target, source)),
            }
        }
        if available {
            return Ok(serial);
        }
    }
    Err(SqliteFamilyError::AllocationExhausted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BackupGeneration {
    // The pre-M15 EventIndex format was <main>.corrupt-<stamp>-<attempt>[-wal|-shm].
    // Legacy sorts before numeric so new restart-convergent generations displace it first.
    Legacy { stamp: u64, attempt: u16 },
    Numeric(u32),
}

fn highest_serial(family: &SqliteFamilyPaths) -> Result<u32, SqliteFamilyError> {
    let mut highest = 0_u32;
    for entry in
        std::fs::read_dir(&family.parent).map_err(|source| io_error(&family.parent, source))?
    {
        let entry = entry.map_err(|source| io_error(&family.parent, source))?;
        if let Some(BackupGeneration::Numeric(serial)) =
            generation_from_name(&entry.file_name().to_string_lossy(), family)
        {
            highest = highest.max(serial);
        }
    }
    Ok(highest)
}

fn prune_backups(
    family: &SqliteFamilyPaths,
    keep: usize,
    active: Option<BackupGeneration>,
) -> Result<(), SqliteFamilyError> {
    let mut retained: Vec<BackupGeneration> = Vec::with_capacity(keep);
    for entry in
        std::fs::read_dir(&family.parent).map_err(|source| io_error(&family.parent, source))?
    {
        let entry = entry.map_err(|source| io_error(&family.parent, source))?;
        let Some(generation) = generation_from_name(&entry.file_name().to_string_lossy(), family)
        else {
            continue;
        };
        if Some(generation) == active || retained.contains(&generation) {
            continue;
        }
        if keep == 0 {
            remove_generation(family, generation)?;
            continue;
        }
        if retained.len() < keep {
            retained.push(generation);
            retained.sort_unstable();
            continue;
        }
        if generation < retained[0] {
            remove_generation(family, generation)?;
            continue;
        }
        let expired = retained.remove(0);
        remove_generation(family, expired)?;
        retained.push(generation);
        retained.sort_unstable();
    }
    Ok(())
}

fn remove_generation(
    family: &SqliteFamilyPaths,
    generation: BackupGeneration,
) -> Result<(), SqliteFamilyError> {
    let targets = match generation {
        BackupGeneration::Numeric(serial) => [
            quarantine_target_path(&family.main, serial)?,
            quarantine_target_path(&family.wal, serial)?,
            quarantine_target_path(&family.shm, serial)?,
        ],
        BackupGeneration::Legacy { stamp, attempt } => {
            legacy_generation_paths(family, stamp, attempt)?
        }
    };
    for target in targets {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&target, source)),
        }
    }
    Ok(())
}

fn generation_from_name(name: &str, family: &SqliteFamilyPaths) -> Option<BackupGeneration> {
    for source in family.sources() {
        let file_name = source.file_name()?.to_string_lossy();
        if let Some(suffix) = name.strip_prefix(&format!("{file_name}.corrupt-"))
            && let Ok(serial) = suffix.parse::<u32>()
            && serial != 0
        {
            return Some(BackupGeneration::Numeric(serial));
        }
    }

    let main_name = family.main.file_name()?.to_string_lossy();
    let suffix = name.strip_prefix(&format!("{main_name}.corrupt-"))?;
    let core = suffix
        .strip_suffix("-wal")
        .or_else(|| suffix.strip_suffix("-shm"))
        .unwrap_or(suffix);
    let mut parts = core.split('-');
    let stamp = parts.next()?.parse::<u64>().ok()?;
    let attempt = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(BackupGeneration::Legacy { stamp, attempt })
}

fn legacy_generation_paths(
    family: &SqliteFamilyPaths,
    stamp: u64,
    attempt: u16,
) -> Result<[PathBuf; 3], SqliteFamilyError> {
    let Some(file_name) = family.main.file_name() else {
        return Err(SqliteFamilyError::InvalidPath(family.main.clone()));
    };
    let mut base = file_name.to_os_string();
    base.push(format!(".corrupt-{stamp}-{attempt}"));
    let main = family.parent.join(&base);
    let mut wal_name = base.clone();
    wal_name.push("-wal");
    let mut shm_name = base;
    shm_name.push("-shm");
    Ok([
        main,
        family.parent.join(wal_name),
        family.parent.join(shm_name),
    ])
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> Result<bool, SqliteFamilyError> {
    use std::os::unix::fs::MetadataExt as _;

    let left_metadata = std::fs::symlink_metadata(left).map_err(|source| io_error(left, source))?;
    let right_metadata =
        std::fs::symlink_metadata(right).map_err(|source| io_error(right, source))?;
    if !left_metadata.is_file()
        || left_metadata.file_type().is_symlink()
        || !right_metadata.is_file()
        || right_metadata.file_type().is_symlink()
    {
        return Ok(false);
    }
    Ok(left_metadata.dev() == right_metadata.dev() && left_metadata.ino() == right_metadata.ino())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn same_file_identity(left: &Path, right: &Path) -> Result<bool, SqliteFamilyError> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    fn identity(path: &Path) -> Result<(u32, u64), SqliteFamilyError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(SqliteFamilyError::Collision(path.to_path_buf()));
        }
        let file = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
        let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        let succeeded = unsafe {
            GetFileInformationByHandle(file.as_raw_handle() as HANDLE, information.as_mut_ptr())
        };
        if succeeded == 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        let information = unsafe { information.assume_init() };
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        Ok((information.dwVolumeSerialNumber, file_index))
    }

    Ok(identity(left)? == identity(right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &Path, right: &Path) -> Result<bool, SqliteFamilyError> {
    let _ = (left, right);
    Ok(false)
}

fn path_present(path: &Path) -> Result<bool, SqliteFamilyError> {
    match inspect_path_presence(path) {
        PathPresence::Present(_) => Ok(true),
        PathPresence::Absent => Ok(false),
        PathPresence::Uninspectable(source) => Err(io_error(path, source)),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> SqliteFamilyError {
    SqliteFamilyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::PoisonError;

    use tempfile::tempdir;

    use super::*;

    const MAX_BACKUPS: usize = 4;

    fn family_at(directory: &Path, name: &str) -> SqliteFamilyPaths {
        SqliteFamilyPaths::new(&directory.join(name)).unwrap()
    }

    fn write_canonical_family(family: &SqliteFamilyPaths) {
        std::fs::write(&family.main, b"main").unwrap();
        std::fs::write(&family.wal, b"wal").unwrap();
        std::fs::write(&family.shm, b"shm").unwrap();
    }

    fn generations(family: &SqliteFamilyPaths) -> BTreeSet<BackupGeneration> {
        std::fs::read_dir(&family.parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| generation_from_name(&entry.file_name().to_string_lossy(), family))
            .collect()
    }

    #[test]
    fn legacy_pending_marker_with_full_family_upgrades_and_converges() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "recordings.sqlite3");
        write_canonical_family(&family);
        std::fs::write(&family.marker, b"pending\n").unwrap();

        let report = prepare_sqlite_family(&family.main, MAX_BACKUPS)
            .unwrap()
            .expect("legacy marker owns quarantine");

        assert_eq!(report.serial, 1);
        assert_eq!(std::fs::read(&report.main_backup).unwrap(), b"main");
        assert_eq!(std::fs::read(&report.wal_backup).unwrap(), b"wal");
        assert_eq!(std::fs::read(&report.shm_backup).unwrap(), b"shm");
        assert!(!family.main.exists());
        assert!(!family.wal.exists());
        assert!(!family.shm.exists());
        assert!(!family.marker.exists());
        assert_eq!(generations(&family).len(), 1);
    }

    #[test]
    fn legacy_pending_marker_preserves_already_moved_numeric_wal_evidence() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "recordings.sqlite3");
        std::fs::write(&family.main, b"main").unwrap();
        std::fs::write(&family.shm, b"shm").unwrap();
        let old_wal = quarantine_target_path(&family.wal, 1).unwrap();
        std::fs::write(&old_wal, b"legacy-wal").unwrap();
        std::fs::write(&family.marker, b"pending\n").unwrap();

        let report = prepare_sqlite_family(&family.main, MAX_BACKUPS)
            .unwrap()
            .expect("legacy marker owns quarantine");

        assert_eq!(report.serial, 2);
        assert_eq!(std::fs::read(&old_wal).unwrap(), b"legacy-wal");
        assert_eq!(std::fs::read(&report.main_backup).unwrap(), b"main");
        assert_eq!(std::fs::read(&report.shm_backup).unwrap(), b"shm");
        assert!(!family.main.exists());
        assert!(!family.wal.exists());
        assert!(!family.shm.exists());
        assert!(!family.marker.exists());
        assert_eq!(generations(&family).len(), 2);
    }

    #[test]
    fn staged_legacy_marker_upgrade_is_reused_after_restart() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "recordings.sqlite3");
        write_canonical_family(&family);
        std::fs::write(&family.marker, b"pending\n").unwrap();
        let upgrade = sqlite_sidecar_path(&family.marker, MARKER_UPGRADE_SUFFIX).unwrap();
        create_marker_file(&upgrade, 1).unwrap();

        let report = prepare_sqlite_family(&family.main, MAX_BACKUPS)
            .unwrap()
            .expect("staged legacy upgrade must resume");

        assert_eq!(report.serial, 1);
        assert!(!upgrade.exists());
        assert!(!family.marker.exists());
        assert_eq!(generations(&family).len(), 1);
    }

    #[test]
    fn legacy_marker_upgrade_survives_multiple_restarts_without_new_serials() {
        let _fault = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "recordings.sqlite3");
        write_canonical_family(&family);
        std::fs::write(&family.marker, b"pending\n").unwrap();

        let first = crate::test_hooks::arm_sqlite_quarantine_interruption(&family.main, 1);
        assert!(matches!(
            prepare_sqlite_family(&family.main, MAX_BACKUPS),
            Err(SqliteFamilyError::InjectedInterruption)
        ));
        drop(first);
        assert_eq!(
            read_pending_marker(&family.marker).unwrap(),
            Some(PendingMarker::Current(1))
        );

        let second = crate::test_hooks::arm_sqlite_quarantine_interruption(&family.main, 1);
        assert!(matches!(
            prepare_sqlite_family(&family.main, MAX_BACKUPS),
            Err(SqliteFamilyError::InjectedInterruption)
        ));
        drop(second);
        assert_eq!(
            read_pending_marker(&family.marker).unwrap(),
            Some(PendingMarker::Current(1))
        );

        let report = prepare_sqlite_family(&family.main, MAX_BACKUPS)
            .unwrap()
            .expect("third restart settles same generation");
        assert_eq!(report.serial, 1);
        assert_eq!(generations(&family).len(), 1);
        assert!(!family.marker.exists());
    }

    #[test]
    fn same_file_wal_partial_publication_is_recovered_on_retry() {
        let _fault = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        write_canonical_family(&family);

        let hook = crate::test_hooks::arm_sqlite_quarantine_partial_publication(&family.wal);
        assert!(matches!(
            quarantine_sqlite_family(&family.main, MAX_BACKUPS),
            Err(SqliteFamilyError::InjectedInterruption)
        ));
        drop(hook);
        let wal_backup = quarantine_target_path(&family.wal, 1).unwrap();
        assert!(family.wal.exists());
        assert!(wal_backup.exists());
        assert!(same_file_identity(&family.wal, &wal_backup).unwrap());

        let report = quarantine_sqlite_family(&family.main, MAX_BACKUPS).unwrap();
        assert_eq!(report.serial, 1);
        assert!(!family.wal.exists());
        assert!(!family.marker.exists());
        assert_eq!(std::fs::read(&report.wal_backup).unwrap(), b"wal");
    }

    #[test]
    fn unrelated_source_and_owned_target_collision_remains_fail_closed() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        write_canonical_family(&family);
        create_pending_marker(&family.marker, 1).unwrap();
        let wal_backup = quarantine_target_path(&family.wal, 1).unwrap();
        std::fs::write(&wal_backup, b"unrelated").unwrap();

        let error = quarantine_sqlite_family(&family.main, MAX_BACKUPS).unwrap_err();
        assert!(matches!(error, SqliteFamilyError::Collision(path) if path == wal_backup));
        assert_eq!(std::fs::read(&family.wal).unwrap(), b"wal");
        assert_eq!(std::fs::read(&wal_backup).unwrap(), b"unrelated");
        assert!(family.marker.exists());
    }

    #[test]
    fn same_file_main_partial_publication_is_recovered_on_retry() {
        let _fault = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        write_canonical_family(&family);

        let hook = crate::test_hooks::arm_sqlite_quarantine_partial_publication(&family.main);
        assert!(matches!(
            quarantine_sqlite_family(&family.main, MAX_BACKUPS),
            Err(SqliteFamilyError::InjectedInterruption)
        ));
        drop(hook);
        let main_backup = quarantine_target_path(&family.main, 1).unwrap();
        assert!(family.main.exists());
        assert!(main_backup.exists());
        assert!(same_file_identity(&family.main, &main_backup).unwrap());

        let report = quarantine_sqlite_family(&family.main, MAX_BACKUPS).unwrap();
        assert_eq!(report.serial, 1);
        assert!(!family.main.exists());
        assert_eq!(std::fs::read(&report.main_backup).unwrap(), b"main");
    }

    #[test]
    fn same_file_retry_then_later_interruption_still_converges_same_generation() {
        let _fault = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        write_canonical_family(&family);

        let partial = crate::test_hooks::arm_sqlite_quarantine_partial_publication(&family.wal);
        assert!(quarantine_sqlite_family(&family.main, MAX_BACKUPS).is_err());
        drop(partial);

        let later = crate::test_hooks::arm_sqlite_quarantine_interruption(&family.main, 2);
        assert!(matches!(
            quarantine_sqlite_family(&family.main, MAX_BACKUPS),
            Err(SqliteFamilyError::InjectedInterruption)
        ));
        drop(later);
        assert_eq!(
            read_pending_marker(&family.marker).unwrap(),
            Some(PendingMarker::Current(1))
        );

        let report = quarantine_sqlite_family(&family.main, MAX_BACKUPS).unwrap();
        assert_eq!(report.serial, 1);
        assert_eq!(generations(&family).len(), 1);
        assert!(!family.marker.exists());
    }

    #[test]
    fn healthy_prepare_prunes_excess_numeric_sidecar_only_generations() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "recordings.sqlite3");
        std::fs::write(&family.main, b"healthy-main").unwrap();
        for serial in 1..=6 {
            let source = if serial % 2 == 0 {
                &family.wal
            } else {
                &family.shm
            };
            std::fs::write(quarantine_target_path(source, serial).unwrap(), b"old").unwrap();
        }

        assert!(
            prepare_sqlite_family(&family.main, MAX_BACKUPS)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&family.main).unwrap(), b"healthy-main");
        assert_eq!(generations(&family).len(), MAX_BACKUPS);
    }

    #[test]
    fn healthy_prepare_prunes_excess_legacy_event_sidecar_generations() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        std::fs::write(&family.main, b"healthy-main").unwrap();
        for stamp in 100..=105 {
            let paths = legacy_generation_paths(&family, stamp, 0).unwrap();
            let sidecar = if stamp % 2 == 0 { &paths[1] } else { &paths[2] };
            std::fs::write(sidecar, b"legacy-sidecar").unwrap();
        }

        assert!(
            prepare_sqlite_family(&family.main, MAX_BACKUPS)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&family.main).unwrap(), b"healthy-main");
        assert_eq!(generations(&family).len(), MAX_BACKUPS);
    }

    #[test]
    fn active_generation_is_never_pruned_during_prepare() {
        let temp = tempdir().unwrap();
        let family = family_at(temp.path(), "events.sqlite3");
        std::fs::write(&family.main, b"main").unwrap();
        std::fs::write(&family.shm, b"shm").unwrap();
        create_pending_marker(&family.marker, 10).unwrap();
        let active_wal = quarantine_target_path(&family.wal, 10).unwrap();
        std::fs::write(&active_wal, b"active-wal").unwrap();
        for serial in 1..=6 {
            std::fs::write(
                quarantine_target_path(&family.main, serial).unwrap(),
                b"old-main",
            )
            .unwrap();
        }

        let report = prepare_sqlite_family(&family.main, MAX_BACKUPS)
            .unwrap()
            .expect("active marker must settle");
        assert_eq!(report.serial, 10);
        assert_eq!(std::fs::read(&active_wal).unwrap(), b"active-wal");
        assert_eq!(generations(&family).len(), MAX_BACKUPS);
        assert!(!family.marker.exists());
    }
}
