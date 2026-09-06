//! Restart-convergent quarantine for rebuildable SQLite index families.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::paths::publish_no_replace;
use crate::{PathPresence, StorageError, inspect_path_presence};

const MARKER_MAGIC: &str = "NIAN-SQLITE-QUARANTINE v1";
const MAX_MARKER_BYTES: u64 = 128;
const MAX_SERIAL_ALLOCATION_ATTEMPTS: u32 = 100;

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
    let marker_present = path_present(&family.marker)?;
    let main_present = path_present(&family.main)?;
    let wal_present = path_present(&family.wal)?;
    let shm_present = path_present(&family.shm)?;
    if marker_present || (!main_present && (wal_present || shm_present)) {
        return quarantine_sqlite_family(main, max_backups).map(Some);
    }
    Ok(None)
}

pub fn quarantine_sqlite_family(
    main: &Path,
    max_backups: usize,
) -> Result<SqliteFamilyQuarantine, SqliteFamilyError> {
    let family = SqliteFamilyPaths::new(main)?;
    let serial = match read_pending_serial(&family.marker)? {
        Some(serial) => serial,
        None => {
            prune_backups(&family, max_backups.saturating_sub(1))?;
            let serial = allocate_serial(&family)?;
            create_pending_marker(&family.marker, serial)?;
            serial
        }
    };
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
                        return Err(SqliteFamilyError::Collision(target.clone()));
                    }
                    PathPresence::Uninspectable(source) => {
                        return Err(io_error(target, source));
                    }
                }
                publish_no_replace(source, target)?;
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

fn read_pending_serial(marker: &Path) -> Result<Option<u32>, SqliteFamilyError> {
    match inspect_path_presence(marker) {
        PathPresence::Absent => Ok(None),
        PathPresence::Present(metadata) if metadata.is_file() => {
            if metadata.len() > MAX_MARKER_BYTES {
                return Err(SqliteFamilyError::InvalidMarker(marker.to_path_buf()));
            }
            let text =
                std::fs::read_to_string(marker).map_err(|source| io_error(marker, source))?;
            parse_marker(&text)
                .map(Some)
                .ok_or_else(|| SqliteFamilyError::InvalidMarker(marker.to_path_buf()))
        }
        PathPresence::Present(_) => Err(SqliteFamilyError::InvalidMarker(marker.to_path_buf())),
        PathPresence::Uninspectable(source) => Err(io_error(marker, source)),
    }
}

fn parse_marker(text: &str) -> Option<u32> {
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
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_pending_serial(marker)?;
            return if existing == Some(serial) {
                Ok(())
            } else {
                Err(SqliteFamilyError::Collision(marker.to_path_buf()))
            };
        }
        Err(source) => return Err(io_error(marker, source)),
    };
    write!(file, "{MARKER_MAGIC}\nserial: {serial}\n")
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(marker, source))
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

fn prune_backups(family: &SqliteFamilyPaths, keep: usize) -> Result<(), SqliteFamilyError> {
    let mut retained: Vec<BackupGeneration> = Vec::with_capacity(keep);
    for entry in
        std::fs::read_dir(&family.parent).map_err(|source| io_error(&family.parent, source))?
    {
        let entry = entry.map_err(|source| io_error(&family.parent, source))?;
        let Some(generation) = generation_from_name(&entry.file_name().to_string_lossy(), family)
        else {
            continue;
        };
        if retained.contains(&generation) {
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
        if let Some(suffix) = name.strip_prefix(&format!("{file_name}.corrupt-")) {
            if let Ok(serial) = suffix.parse::<u32>() {
                if serial != 0 {
                    return Some(BackupGeneration::Numeric(serial));
                }
            }
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
