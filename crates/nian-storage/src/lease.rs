//! Kernel-backed per-camera recording ownership.
//!
//! The lock file is deliberately permanent. Ownership is the operating
//! system's exclusive file lock on the open handle, never pathname existence,
//! a PID, or a heartbeat timestamp.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nian_domain::CameraId;

use crate::{RecordingsLayout, StorageError};

/// Stable control-artifact name stored at the root of each camera tree.
pub const CAMERA_LEASE_FILE_NAME: &str = ".nian-camera.lock";

/// Exclusive ownership of one camera job in one recordings layout.
///
/// The open [`File`] owns the kernel lock. Dropping this value, including by
/// process death, closes the handle and releases ownership automatically.
#[derive(Debug)]
pub struct CameraLease {
    camera_id: CameraId,
    lock_path: PathBuf,
    _file: File,
}

impl CameraLease {
    /// Creates/reuses the stable lock file and tries to take its exclusive OS
    /// lock without blocking.
    ///
    /// `WouldBlock` is mapped to [`StorageError::CameraAlreadyActive`] rather
    /// than inferred from an error string. A stale lock FILE is harmless: if
    /// no process owns its OS lock, this succeeds.
    pub fn try_acquire(layout: &RecordingsLayout, camera: &CameraId) -> Result<Self, StorageError> {
        let camera_dir = layout.camera_dir(camera);
        std::fs::create_dir_all(&camera_dir).map_err(|source| StorageError::Io {
            path: camera_dir.clone(),
            source,
        })?;

        let lock_path = camera_dir.join(CAMERA_LEASE_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| StorageError::Io {
                path: lock_path.clone(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => Ok(Self {
                camera_id: camera.clone(),
                lock_path,
                _file: file,
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(StorageError::CameraAlreadyActive {
                camera_id: camera.clone(),
                lock_path,
            }),
            Err(std::fs::TryLockError::Error(source)) => Err(StorageError::Io {
                path: lock_path,
                source,
            }),
        }
    }

    /// Camera identity bound to this lease.
    pub fn camera_id(&self) -> &CameraId {
        &self.camera_id
    }

    /// Stable lock path whose open file handle owns the OS lock.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Proves this lease belongs to the requested camera in this exact layout.
    pub fn authorizes(&self, layout: &RecordingsLayout, camera: &CameraId) -> bool {
        self.camera_id == *camera
            && self.lock_path == layout.camera_dir(camera).join(CAMERA_LEASE_FILE_NAME)
    }

    /// Verifies camera/layout identity and returns a typed storage error on
    /// mismatch. Callers that gate filesystem operations should prefer this
    /// over reimplementing the identity comparison themselves.
    pub fn verify(&self, layout: &RecordingsLayout, camera: &CameraId) -> Result<(), StorageError> {
        if self.authorizes(layout, camera) {
            return Ok(());
        }
        Err(StorageError::CameraLeaseMismatch {
            camera_id: camera.clone(),
            lock_path: layout.camera_dir(camera).join(CAMERA_LEASE_FILE_NAME),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, RecordingsLayout) {
        let temp = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(temp.path().join("recordings")).unwrap();
        (temp, layout)
    }

    #[test]
    fn same_camera_is_exclusive() {
        let (_temp, layout) = layout();
        let camera = CameraId::parse("cam-a").unwrap();
        let first = CameraLease::try_acquire(&layout, &camera).unwrap();

        let second = CameraLease::try_acquire(&layout, &camera).unwrap_err();
        assert!(matches!(second, StorageError::CameraAlreadyActive { .. }));

        drop(first);
        CameraLease::try_acquire(&layout, &camera).unwrap();
    }

    #[test]
    fn different_cameras_can_be_leased_concurrently() {
        let (_temp, layout) = layout();
        let camera_a = CameraId::parse("cam-a").unwrap();
        let camera_b = CameraId::parse("cam-b").unwrap();

        let _a = CameraLease::try_acquire(&layout, &camera_a).unwrap();
        let _b = CameraLease::try_acquire(&layout, &camera_b).unwrap();
    }

    #[test]
    fn stale_lock_file_without_os_lock_is_acquirable() {
        let (_temp, layout) = layout();
        let camera = CameraId::parse("cam-a").unwrap();
        let camera_dir = layout.camera_dir(&camera);
        std::fs::create_dir_all(&camera_dir).unwrap();
        std::fs::write(camera_dir.join(CAMERA_LEASE_FILE_NAME), b"stale marker").unwrap();

        CameraLease::try_acquire(&layout, &camera).unwrap();
    }

    #[test]
    fn lease_is_bound_to_camera_and_layout() {
        let (temp, layout_a) = layout();
        let layout_b = RecordingsLayout::new(temp.path().join("other-recordings")).unwrap();
        let camera_a = CameraId::parse("cam-a").unwrap();
        let camera_b = CameraId::parse("cam-b").unwrap();
        let lease = CameraLease::try_acquire(&layout_a, &camera_a).unwrap();

        assert!(lease.authorizes(&layout_a, &camera_a));
        assert!(lease.verify(&layout_a, &camera_a).is_ok());
        assert!(!lease.authorizes(&layout_a, &camera_b));
        assert!(!lease.authorizes(&layout_b, &camera_a));
        assert!(matches!(
            lease.verify(&layout_a, &camera_b),
            Err(StorageError::CameraLeaseMismatch { .. })
        ));
        assert!(matches!(
            lease.verify(&layout_b, &camera_a),
            Err(StorageError::CameraLeaseMismatch { .. })
        ));
    }
}
