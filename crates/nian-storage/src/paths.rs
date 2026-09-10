//! Deterministic recordings directory layout with traversal-safe names.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use nian_domain::CameraId;

use crate::classification::owned_recording_name;
use crate::error::StorageError;

#[cfg(test)]
/// Test-only fault seam: the FIRST probe create reports a collision so the
/// fresh-identity retry path is deterministically exercised.
static PROBE_FIRST_CREATE_COLLIDES: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
/// Test-only fault seam: marks the injected collision as consumed.
static PROBE_FIRST_CREATE_DONE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
/// Test-only fault seam: the probe removal fails deterministically.
static PROBE_REMOVE_FAILS: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
/// Serializes fault-armed pre-flight tests against the process-global seams
/// (parallel unit tests share these statics).
static PREFLIGHT_FAULT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reserved storage-control directory; cannot parse as a CameraId.
pub const CONTROL_DIRECTORY_NAME: &str = ".nian";
/// Rebuildable recording index filename inside the control directory.
pub const RECORDING_INDEX_FILE_NAME: &str = "recordings.sqlite3";
/// Normalized ONVIF event-history index beside the recording index.
pub const EVENT_INDEX_FILE_NAME: &str = "events.sqlite3";
/// File extension used for finalized recording segments.
pub const SEGMENT_EXTENSION: &str = "mkv";

/// Suffix inserted before the extension while a segment is still open
/// (`08-30-00.partial.mkv`); finalized files never carry it.
pub const SEGMENT_PARTIAL_SUFFIX: &str = ".partial";

/// Lowest disambiguation sequence rendered explicitly. Sequence 1 is the
/// bare name (`08-30-00.mkv`); from 2 on, the sequence appears as a suffix
/// (`08-30-00-2.mkv`). The suffix keeps recording collision-free when the
/// worker reconnects or restarts more than once within the same second.
pub const MIN_EXPLICIT_SEQUENCE: u32 = 2;

/// Builds paths inside the recordings storage root.
///
/// Every component is either derived from a validated [`CameraId`] or from a
/// formatted timestamp, and is re-checked by [`checked_component`] so no
/// user-controlled string can escape the storage tree.
#[derive(Debug, Clone)]
pub struct RecordingsLayout {
    root: PathBuf,
}

impl RecordingsLayout {
    /// Creates a layout rooted at an absolute directory.
    ///
    /// The filesystem root itself is rejected: cleanup must never be able to
    /// mistake `/` (or a drive root) for a recordings directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(StorageError::InvalidRoot {
                reason: "storage root must be absolute".to_owned(),
            });
        }
        if root.parent().is_none() {
            return Err(StorageError::InvalidRoot {
                reason: "storage root must not be the filesystem root".to_owned(),
            });
        }
        Ok(Self { root })
    }

    /// The validated storage root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/.nian`, reserved for rebuildable/control state rather than footage.
    pub fn control_dir(&self) -> PathBuf {
        self.root.join(CONTROL_DIRECTORY_NAME)
    }

    /// Centralized path of the rebuildable SQLite recording index.
    pub fn recording_index_path(&self) -> PathBuf {
        self.control_dir().join(RECORDING_INDEX_FILE_NAME)
    }

    /// Centralized path of the bounded normalized event-history SQLite index.
    pub fn event_index_path(&self) -> PathBuf {
        self.control_dir().join(EVENT_INDEX_FILE_NAME)
    }

    /// Creates the reserved control directory without accepting a symlink in
    /// its place. Recording inventory and retention never descend into it.
    pub fn ensure_control_dir(&self) -> Result<PathBuf, StorageError> {
        let control = self.control_dir();
        match std::fs::symlink_metadata(&control) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(StorageError::InvalidRoot {
                        reason: format!("control path {control:?} is not a real directory"),
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&control).map_err(|source| StorageError::Io {
                    path: control.clone(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(StorageError::Io {
                    path: control.clone(),
                    source,
                });
            }
        }
        Ok(control)
    }

    /// `<root>/<camera-id>`
    // The expects below are invariants, not guesses: `checked_component` only
    // rejects separators, control characters, and dot components, none of
    // which a validated `CameraId` or a formatted date can contain.
    #[allow(clippy::expect_used)]
    pub fn camera_dir(&self, camera: &CameraId) -> PathBuf {
        self.join_checked(&[camera.as_str()])
            .expect("validated CameraId is always path-safe")
    }

    /// Creates the camera directory if missing AND proves it is healthy
    /// enough for Nian Vision's recording/retention lifecycle (final
    /// correctness remediation §8, final safety remediation §4). The
    /// pre-flight proves, in order:
    ///
    /// * the camera directory can be created/accessed;
    /// * a FRESH file can be EXCLUSIVELY created (`create_new`);
    /// * bytes can actually be written and flushed (`sync_all`);
    /// * the probe can be closed;
    /// * the probe can be REMOVED — a directory that accepts files but
    ///   cannot delete them cannot host the retention lifecycle, so
    ///   cleanup failure is surfaced, never silently ignored.
    ///
    /// Probe naming is collision-safe (pid + monotonic serial + clock
    /// nanos): an `AlreadyExists` from a survived old probe retries with a
    /// FRESH probe identity instead of condemning the storage root. The
    /// reserved probe name never parses as a segment/recording/recovery
    /// name (always classifies `Unknown`). Failure surfaces a genuine
    /// storage error — the worker job maps it to terminal `storage_failed`
    /// while still holding the camera lease.
    pub fn ensure_camera_dir(&self, camera: &CameraId) -> Result<PathBuf, StorageError> {
        let dir = self.camera_dir(camera);
        std::fs::create_dir_all(&dir).map_err(|source| StorageError::Io {
            path: dir.clone(),
            source,
        })?;

        static PROBE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut last_collision: Option<StorageError> = None;
        for _ in 0..8 {
            let serial = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let probe = dir.join(format!(
                ".nian-write-probe-{}-{serial}-{nanos}.tmp",
                std::process::id()
            ));
            // Test-only seam: a deterministic first-attempt collision.
            #[cfg(test)]
            if PROBE_FIRST_CREATE_COLLIDES.load(Ordering::SeqCst)
                && !PROBE_FIRST_CREATE_DONE.swap(true, Ordering::SeqCst)
            {
                last_collision = Some(StorageError::Io {
                    path: probe,
                    source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                });
                continue;
            }
            let mut probe_file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
            {
                Ok(file) => file,
                // A survived old probe (or an exotic collision): retry with
                // another unique probe identity — the storage root is NOT
                // classified unavailable for this.
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_collision = Some(StorageError::Io {
                        path: probe,
                        source,
                    });
                    continue;
                }
                Err(source) => {
                    return Err(StorageError::Io {
                        path: probe,
                        source,
                    });
                }
            };
            // Exclusive creation succeeded; prove bytes actually land and
            // the handle closes cleanly.
            let write_result = probe_file
                .write_all(b"nian-vision write probe")
                .and_then(|()| probe_file.sync_all());
            drop(probe_file);
            if let Err(source) = write_result {
                let _ = std::fs::remove_file(&probe);
                return Err(StorageError::Io {
                    path: probe,
                    source,
                });
            }
            // The writeability proof is complete only when the probe can
            // ALSO be removed (final safety remediation §4): cleanup
            // failure is a genuine health finding, never ignored.
            #[cfg(test)]
            if PROBE_REMOVE_FAILS.load(Ordering::SeqCst) {
                return Err(StorageError::Io {
                    path: probe,
                    source: std::io::Error::other("injected probe removal failure"),
                });
            }
            std::fs::remove_file(&probe).map_err(|source| StorageError::Io {
                path: probe,
                source,
            })?;
            return Ok(dir);
        }
        Err(last_collision.unwrap_or_else(|| StorageError::Io {
            path: dir,
            source: std::io::Error::other("writeability probe exhausted collision retries"),
        }))
    }

    /// `<root>/<camera-id>/<year>/<month>/<day>`
    #[allow(clippy::expect_used)]
    pub fn day_dir(&self, camera: &CameraId, date: NaiveDate) -> PathBuf {
        self.join_checked(&[
            camera.as_str(),
            &format!("{:04}", date.year()),
            &format!("{:02}", date.month()),
            &format!("{:02}", date.day()),
        ])
        .expect("formatted date components are always path-safe")
    }

    /// Computes the paths for a new segment starting at `started_at` as if
    /// nothing else existed yet (sequence 1 when the day directory is
    /// missing).
    ///
    /// Collision-aware but **scan-only**: this is a dry-run naming helper
    /// for diagnostics and tests, not an acquisition primitive — between
    /// scanning and file creation another writer could take the name. The
    /// recorder must use [`RecordingsLayout::claim_segment`], which closes
    /// that TOCTOU window with exclusive creation.
    ///
    /// The names stay deterministic and parseable
    /// ([`parse_segment_file_name`]) so reconciliation can classify disk
    /// contents without trusting the database index.
    pub fn allocate_segment(
        &self,
        camera: &CameraId,
        started_at: NaiveDateTime,
    ) -> Result<AllocatedSegmentPaths, StorageError> {
        let day_dir = self.day_dir(camera, started_at.date());
        let sequence = allocate_segment_sequence(&day_dir, started_at.time())?;
        Ok(AllocatedSegmentPaths {
            partial_path: day_dir
                .join(partial_file_name_with_sequence(started_at.time(), sequence)),
            final_path: day_dir.join(segment_file_name_with_sequence(started_at.time(), sequence)),
        })
    }

    fn join_checked(&self, components: &[&str]) -> Result<PathBuf, StorageError> {
        let mut path = self.root.clone();
        for component in components {
            path.push(checked_component(component)?);
        }
        Ok(path)
    }

    /// Authoritatively and race-safely claims the next free segment slot
    /// for `started_at` (atomic identity claim remediation §1/§2).
    ///
    /// [`allocate_segment_sequence`] is only ADVISORY selection: directory
    /// enumeration is not an atomic snapshot, so between its scan and an
    /// exclusive creation another process can publish a reservation for the
    /// SAME `(started_at, sequence)` identity. This method is the
    /// authoritative acquisition primitive and closes the window in two
    /// layers:
    ///
    /// 1. the partial file is created with exclusive semantics
    ///    (`create_new`, O_EXCL); a lost name race rescans and retries;
    /// 2. a POST-CLAIM IDENTITY FENCE then re-checks the freshly created
    ///    candidate against a NEW Nian-owned namespace enumeration for the
    ///    same identity — a recovered final, its tombstone, a valid
    ///    recovery scratch, or a normal finalized recording that appeared
    ///    mid-flight forces this candidate to be relinquished (handle
    ///    closed, ONLY the candidate partial removed) and a higher sequence
    ///    retried. The fence is sound because startup recovery publishes
    ///    the recovered final and writes the tombstone BEFORE removing the
    ///    original partial: a freed original pathname implies the
    ///    transaction reservation is already observable.
    ///
    /// The day directory is created if needed. Two workers racing in the
    /// same second therefore always end up with distinct identities and an
    /// existing recording is never truncated or replaced.
    ///
    /// Filesystem identity invariant (sub-second identity remediation §1):
    /// a recording identity is (WHOLE-second local time, sequence) because
    /// recording names encode no sub-second component. The caller-supplied
    /// timestamp is normalized EXACTLY ONCE, here — the advisory
    /// allocation, the candidate naming and the post-claim fence all share
    /// this truncated identity time, so a live clock's nanoseconds can
    /// never make the fence compare `08:30:00.877` against a reservation
    /// parsed from `08-30-00.…` and miss the conflict. The full timestamp
    /// stays available for human/event metadata only.
    ///
    /// The returned [`ClaimedSegment`] documents the finalize contract
    /// (write into `partial_path`, publish to `final_path` with
    /// [`publish_no_replace`]).
    pub fn claim_segment(
        &self,
        camera: &CameraId,
        started_at: NaiveDateTime,
    ) -> Result<ClaimedSegment, StorageError> {
        let filesystem_started_at = filesystem_identity_datetime(started_at);
        let day_dir = self.day_dir(camera, filesystem_started_at.date());
        std::fs::create_dir_all(&day_dir).map_err(|source| StorageError::Io {
            path: day_dir.clone(),
            source,
        })?;

        // The single normalization point for the filesystem identity
        // (sub-second identity remediation §1).
        let identity_time = filesystem_started_at.time();

        loop {
            let sequence = allocate_segment_sequence(&day_dir, identity_time)?;
            // Deterministic test seam (atomic identity claim remediation
            // §5): parks AFTER the advisory scan chose a sequence but
            // BEFORE the candidate is created — the window where a
            // competing namespace transition lands in production.
            #[cfg(any(test, feature = "test-hooks"))]
            crate::test_hooks::claim_identity_gate_wait(&day_dir);
            let partial_path =
                day_dir.join(partial_file_name_with_sequence(identity_time, sequence));
            let final_path = day_dir.join(segment_file_name_with_sequence(identity_time, sequence));

            let claim = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)
            {
                Ok(partial_file) => partial_file,
                // Lost the race for this name (another writer created it
                // first): rescan and try the next sequence.
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(StorageError::Io {
                        path: partial_path,
                        source,
                    });
                }
            };

            // Deterministic test seam (sub-second identity remediation §4):
            // one-shot day-directory loss AFTER the candidate exists but
            // BEFORE the fence validates it — a real "storage vanished
            // mid-claim" fault whose failures are genuine OS errors.
            #[cfg(any(test, feature = "test-hooks"))]
            crate::test_hooks::claim_day_dir_loss_fire(&day_dir);

            // Post-claim identity fence (atomic identity claim remediation
            // §2): the candidate exists but was NEVER returned to the
            // recorder, so relinquishing it is unambiguous.
            let conflicted = match identity_conflict_after_claim(
                &day_dir,
                identity_time,
                sequence,
                &partial_path,
            ) {
                Ok(conflicted) => conflicted,
                Err(fence_error) => {
                    // The identity cannot be VALIDATED: never return this
                    // claim. Relinquish it — close the handle first
                    // (Windows-first: an open handle blocks the unlink),
                    // then remove ONLY this attempt's still-empty candidate.
                    // A cleanup failure is itself ambiguous leftover
                    // ownership: surfaced TYPED together with the fence
                    // failure, never silently discarded (sub-second
                    // identity remediation §4).
                    drop(claim);
                    if let Err(cleanup) = std::fs::remove_file(&partial_path) {
                        return Err(StorageError::ClaimFenceCleanup {
                            candidate: partial_path,
                            fence_error: Box::new(fence_error),
                            cleanup,
                        });
                    }
                    return Err(fence_error);
                }
            };
            if conflicted {
                // Safe rollback of a LOSING candidate (§3): close the
                // handle FIRST (Windows-first: an open handle blocks the
                // unlink), then remove ONLY this attempt's candidate. Any
                // removal failure — including an unexpected NotFound,
                // which would mean an external party deleted our claim —
                // is ambiguous ownership: surfaced TYPED, never ignored.
                drop(claim);
                if let Err(source) = std::fs::remove_file(&partial_path) {
                    return Err(StorageError::Io {
                        path: partial_path,
                        source,
                    });
                }
                continue;
            }

            return Ok(ClaimedSegment {
                partial_path,
                final_path,
                _partial_file: claim,
            });
        }
    }
}

/// An **exclusively claimed** segment slot — the race-safe acquisition M2
/// must build on.
///
/// The partial file was created with `O_EXCL` semantics (`create_new`), so
/// at acquisition time no other writer on the system held this name. The
/// open file handle is retained as the claim token: while it is alive,
/// another process's `create_new` on the same name fails and its
/// [`RecordingsLayout::claim_segment`] rescan moves to the next sequence.
/// Duplicate workers therefore never share (or truncate) a segment.
///
/// Dropping the claim closes the handle but deliberately keeps the (empty or
/// partial) file: an abandoned claim is indistinguishable from a crash and
/// stays eligible for startup reconciliation.
///
/// Finalize flow for the owner:
/// 1. write the segment through `MatroskaMuxer::create(partial_path)` —
///    safe because the claimed file exists and is owned by this process;
/// 2. publish with [`publish_no_replace`] from `partial_path` to
///    `final_path`: atomic on every supported platform and never replaces an
///    existing recording ([`StorageError::DestinationExists`] on collision).
#[derive(Debug)]
pub struct ClaimedSegment {
    partial_path: PathBuf,
    final_path: PathBuf,
    /// Claim token; kept private so callers cannot accidentally close it
    /// while still treating the slot as theirs. Rust opens files with
    /// FILE_SHARE_READ|WRITE|DELETE on Windows, so holding this handle does
    /// not block the no-replace publication step.
    _partial_file: std::fs::File,
}

impl ClaimedSegment {
    /// File the recorder writes into (already created, empty).
    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }

    /// No-replace publication target after successful finalization.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

/// Publishes finalized content at its final name **atomically and without
/// ever replacing an existing file**.
///
/// Platform strategy (ADR-0005):
///
/// * **Unix** — `renameat2(…, RENAME_NOREPLACE)` through `rustix`'s safe
///   API: a single syscall whose kernel-side existence check makes the
///   collision refusal race-free (macOS maps the flag onto
///   `renamex_np(RENAME_EXCL)`). Supported by the usual local filesystems;
///   when the kernel or filesystem cannot provide it (`ENOSYS`, `EINVAL`,
///   `EOPNOTSUPP`), the hard-link fallback below takes over.
/// * **Windows** — `MoveFileExW` *without* `MOVEFILE_REPLACE_EXISTING`:
///   the native same-volume no-replace move. Unlike hard links it works on
///   every filesystem a user may select for recordings (NTFS, FAT/exFAT),
///   and the existence check happens inside the move operation, so there is
///   no scan-then-move window either. The call must stay within one volume,
///   which the recordings layout guarantees (partial and final share the
///   day directory); `MOVEFILE_WRITE_THROUGH` keeps the rename durable.
/// * **Fallback** — hard-link + unlink for platforms/filesystems without an
///   atomic no-replace rename. `hard_link` never replaces either, so this
///   stays collision-safe; it needs link support (absent e.g. on FAT) and
///   surfaces a plain error there instead of silently degrading to an
///   overwrite-capable rename.
///
/// In every case the instant the final name appears it already references
/// the complete content, and [`StorageError::DestinationExists`] is returned
/// on collision. If only the source unlink fails after a successful link,
/// the segment is already durably published under its final name; the
/// returned error tells reconciliation to sweep the stale `.partial` link.
pub fn publish_no_replace(from: &Path, to: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        unix_publish_no_replace(from, to)
    }
    #[cfg(windows)]
    {
        windows_publish_no_replace(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (from, to);
        unimplemented!("publication is implemented for Unix and Windows targets")
    }
}

/// Unix path of [`publish_no_replace`]: atomic no-replace rename with a
/// collision-safe fallback where the platform lacks it.
#[cfg(unix)]
fn unix_publish_no_replace(from: &Path, to: &Path) -> Result<(), StorageError> {
    use rustix::fs::{CWD, RenameFlags};
    use rustix::io::Errno;

    match rustix::fs::renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(StorageError::DestinationExists {
            destination: to.to_path_buf(),
        }),
        // ENOSYS: kernel without renameat2 (<3.15). EINVAL/EOPNOTSUPP:
        // filesystem that rejects RENAME_NOREPLACE (some network/stacked
        // filesystems). All three mean "atomic no-replace unavailable" —
        // fall back while keeping the no-replace guarantee; anything else
        // is a real error.
        Err(Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP) => hard_link_fallback(from, to),
        Err(error) => Err(StorageError::Io {
            path: to.to_path_buf(),
            source: std::io::Error::from(error),
        }),
    }
}

/// Windows path of [`publish_no_replace`]: `MoveFileExW` with no replace
/// flag. This is the crate's single sanctioned unsafe block.
#[cfg(windows)]
#[allow(unsafe_code)] // sanctioned exception; see the crate-level policy
fn windows_publish_no_replace(from: &Path, to: &Path) -> Result<(), StorageError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn wide(path: &Path) -> Vec<u16> {
        let mut buffer: Vec<u16> = path.as_os_str().encode_wide().collect();
        buffer.push(0); // NUL terminator
        buffer
    }

    let from_wide = wide(from);
    let to_wide = wide(to);

    // SAFETY: both arguments are valid NUL-terminated UTF-16 strings that
    // outlive the call; MoveFileExW only reads them and returns 0 on
    // failure with GetLastError set. The flags deliberately omit
    // MOVEFILE_REPLACE_EXISTING — the kernel then refuses an existing
    // destination (ERROR_FILE_EXISTS/ERROR_ALREADY_EXISTS) inside the move
    // operation itself, which is the no-replace guarantee.
    let succeeded =
        unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if succeeded != 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == ERROR_FILE_EXISTS as i32 || code == ERROR_ALREADY_EXISTS as i32 => {
            Err(StorageError::DestinationExists {
                destination: to.to_path_buf(),
            })
        }
        _ => Err(StorageError::Io {
            path: to.to_path_buf(),
            source: error,
        }),
    }
}

/// Collision-safe publication without atomic no-replace rename support:
/// link the content under its final name (never replaces) and drop the
/// partial link afterwards.
#[allow(dead_code)] // reached only via the cfg-dispatched fallback paths
fn hard_link_fallback(from: &Path, to: &Path) -> Result<(), StorageError> {
    if let Err(source) = std::fs::hard_link(from, to) {
        return Err(if source.kind() == std::io::ErrorKind::AlreadyExists {
            StorageError::DestinationExists {
                destination: to.to_path_buf(),
            }
        } else {
            StorageError::Io {
                path: to.to_path_buf(),
                source,
            }
        });
    }
    std::fs::remove_file(from).map_err(|source| StorageError::Io {
        path: from.to_path_buf(),
        source,
    })
}

/// Partial and final paths for one freshly allocated segment, sharing the
/// same sequence so `.partial` → final rename recovery stays well-defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatedSegmentPaths {
    /// File the recorder writes into (`…/08-30-00.partial.mkv`).
    pub partial_path: PathBuf,
    /// Rename target after successful finalization (`…/08-30-00.mkv`).
    pub final_path: PathBuf,
}

/// ADVISORY selection of the smallest segment sequence for `started_at`
/// that collides with no existing file in `day_dir` — a dry-run naming
/// helper, never an acquisition primitive (atomic identity claim
/// remediation §7: only [`RecordingsLayout::claim_segment`] authoritatively
/// claims an identity, with the post-claim fence). See
/// [`RecordingsLayout::allocate_segment`].
///
/// Comparison happens at **whole-second granularity**: segment names encode
/// hours/minutes/seconds only, while a live clock carries nanoseconds —
/// comparing raw `NaiveTime`s would make `17:12:00.877` miss the existing
/// `17-12-00.mkv` and hand out sequence 1 twice.
///
/// Identity reservation (identity safety remediation §2): the scan covers
/// the WHOLE Nian-owned namespace — not only names `parse_segment_file_name`
/// accepts. A recovered final (`08-30-00.recovered.mkv`), its tombstone
/// (`08-30-00.recovered.mkv.done`), or a valid recovery scratch reserves the
/// `(started_at, sequence)` of its canonical stem, so a wall-clock rollback
/// (manual clock change, NTP backward adjustment, DST repeated local time)
/// can never re-allocate an identity a finished transaction already used —
/// the allocator hands out `08-30-00-2.partial.mkv` instead. Foreign/Unknown
/// files reserve nothing.
pub fn allocate_segment_sequence(
    day_dir: &Path,
    started_at: NaiveTime,
) -> Result<u32, StorageError> {
    let started_at = whole_seconds(started_at);
    let entries = match std::fs::read_dir(day_dir) {
        Ok(entries) => entries,
        // No directory yet: nothing can collide, use the bare name.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(source) => {
            return Err(StorageError::Io {
                path: day_dir.to_path_buf(),
                source,
            });
        }
    };

    let mut occupied: HashSet<u32> = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            path: day_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue; // non-UTF-8 names cannot be our segments
        };
        if let Some(owned) = owned_recording_name(name)
            && owned.started_at == started_at
        {
            occupied.insert(owned.sequence);
        }
    }

    let mut sequence = 1_u32;
    while occupied.contains(&sequence) {
        sequence = sequence.checked_add(1).ok_or_else(|| StorageError::Io {
            path: day_dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "segment sequence exhausted for this start time",
            ),
        })?;
    }
    Ok(sequence)
}

/// Canonical local wall-clock timestamp encoded by recording paths.
///
/// Filesystem identities have whole-second precision; sub-second event time
/// must never be persisted as if the filename could reproduce it.
pub fn filesystem_identity_datetime(started_at: NaiveDateTime) -> NaiveDateTime {
    NaiveDateTime::new(started_at.date(), whole_seconds(started_at.time()))
}

/// Drops sub-second components so live-clock timestamps compare equal to
/// the second-granular names the allocator emits.
fn whole_seconds(time: NaiveTime) -> NaiveTime {
    NaiveTime::from_hms_opt(time.hour(), time.minute(), time.second()).unwrap_or(time)
}

/// Post-claim identity fence (atomic identity claim remediation §2).
///
/// Directory enumeration is NOT an atomic snapshot: between the advisory
/// scan ([`allocate_segment_sequence`]) and this claim's exclusive
/// candidate creation, a competing process can publish a reservation for
/// the SAME `(started_at, sequence)` identity — a recovered recording, its
/// tombstone, a valid recovery scratch, or a normal finalized recording.
/// This fence performs a FRESH Nian-owned namespace check for the identity,
/// EXCLUDING the candidate partial this claim just created (`except`; the
/// only same-identity partial pathname possible). Returns `Ok(true)` when
/// the identity is no longer free: the caller must relinquish the candidate
/// and retry a higher sequence. Enumeration errors propagate — an
/// unvalidatable claim is never returned.
///
/// `started_at` must already be the WHOLE-second identity time
/// ([`whole_seconds`]; `claim_segment` normalizes once for all identity
/// uses), because names parsed from disk carry no sub-second component.
fn identity_conflict_after_claim(
    day_dir: &Path,
    started_at: NaiveTime,
    sequence: u32,
    except: &Path,
) -> Result<bool, StorageError> {
    let entries = std::fs::read_dir(day_dir).map_err(|source| StorageError::Io {
        path: day_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| StorageError::Io {
            path: day_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue; // non-UTF-8 names cannot be our segments
        };
        if Some(name) == except.file_name().and_then(|n| n.to_str()) {
            continue; // the candidate THIS claim just created
        }
        if let Some(owned) = owned_recording_name(name)
            && owned.started_at == started_at
            && owned.sequence == sequence
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Formats the final segment file name for a start time (`08-30-00.mkv`).
pub fn segment_file_name(started_at: NaiveTime) -> String {
    segment_file_name_with_sequence(started_at, 1)
}

/// Like [`segment_file_name`] but with an explicit disambiguation sequence;
/// sequence 1 renders as the bare name, larger ones as `08-30-00-N.mkv`.
pub fn segment_file_name_with_sequence(started_at: NaiveTime, sequence: u32) -> String {
    format!("{}.{}", time_stem(started_at, sequence), SEGMENT_EXTENSION)
}

/// Formats the in-progress segment file name (`08-30-00.partial.mkv`).
pub fn partial_file_name(started_at: NaiveTime) -> String {
    partial_file_name_with_sequence(started_at, 1)
}

/// Like [`partial_file_name`] but with an explicit disambiguation sequence.
pub fn partial_file_name_with_sequence(started_at: NaiveTime, sequence: u32) -> String {
    format!(
        "{}{}.{}",
        time_stem(started_at, sequence),
        SEGMENT_PARTIAL_SUFFIX,
        SEGMENT_EXTENSION
    )
}

/// `HH-MM-SS` for sequence 1, `HH-MM-SS-N` from [`MIN_EXPLICIT_SEQUENCE`] on.
fn time_stem(started_at: NaiveTime, sequence: u32) -> String {
    let time = format!(
        "{:02}-{:02}-{:02}",
        started_at.hour(),
        started_at.minute(),
        started_at.second()
    );
    if sequence == 1 {
        time
    } else {
        format!("{time}-{sequence}")
    }
}

/// A parsed segment file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSegmentName {
    /// Segment start time-of-day recovered from the name.
    pub started_at: NaiveTime,
    /// Disambiguation sequence; 1 for the bare form (`08-30-00.mkv`),
    /// `>= [`MIN_EXPLICIT_SEQUENCE`]` for `08-30-00-N.mkv`.
    pub sequence: u32,
    /// Whether the name refers to a not-yet-finalized partial file.
    pub is_partial: bool,
}

/// Parses `HH-MM-SS[-N][.partial].mkv` back into its parts.
///
/// Used by startup reconciliation to classify files found on disk without
/// trusting the database index. The optional `-N` suffix must be a decimal
/// number `>= [`MIN_EXPLICIT_SEQUENCE`]` without leading zeros, mirroring
/// exactly what the allocator emits.
pub fn parse_segment_file_name(name: &str) -> Result<ParsedSegmentName, StorageError> {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return Err(unrecognized(name));
    };
    if extension != SEGMENT_EXTENSION {
        return Err(unrecognized(name));
    }

    let (stem, is_partial) = match stem.strip_suffix(SEGMENT_PARTIAL_SUFFIX) {
        Some(stripped) => (stripped, true),
        None => (stem, false),
    };

    let mut parts = stem.split('-');
    let (h, m, s, sequence_part) = match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(h), Some(m), Some(s), None, None) => (h, m, s, None),
        (Some(h), Some(m), Some(s), Some(sequence), None) => (h, m, s, Some(sequence)),
        _ => return Err(unrecognized(name)),
    };

    let invalid = || unrecognized(name);

    let hour: u32 = h.parse().map_err(|_| invalid())?;
    let minute: u32 = m.parse().map_err(|_| invalid())?;
    let second: u32 = s.parse().map_err(|_| invalid())?;
    if h.len() != 2 || m.len() != 2 || s.len() != 2 {
        return Err(invalid());
    }

    let sequence = match sequence_part {
        None => 1,
        Some(text) => {
            // Canonical rendering only: no leading zeros, and below the
            // minimum explicit sequence the bare form is authoritative.
            if text.starts_with('0') {
                return Err(invalid());
            }
            let value: u32 = text.parse().map_err(|_| invalid())?;
            if value < MIN_EXPLICIT_SEQUENCE {
                return Err(invalid());
            }
            value
        }
    };

    let started_at = NaiveTime::from_hms_opt(hour, minute, second).ok_or_else(invalid)?;

    Ok(ParsedSegmentName {
        started_at,
        sequence,
        is_partial,
    })
}

fn unrecognized(name: &str) -> StorageError {
    StorageError::UnrecognizedSegmentName {
        name: name.to_owned(),
    }
}

/// Returns the input unchanged when it is safe to embed in a path.
fn checked_component(component: &str) -> Result<&str, StorageError> {
    let unsafe_chars = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    if component.is_empty()
        || component == "."
        || component == ".."
        || component
            .chars()
            .any(|c| c.is_control() || unsafe_chars.contains(&c))
    {
        return Err(StorageError::UnsafeComponent {
            component: component.to_owned(),
        });
    }
    Ok(component)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_in(root: &Path) -> RecordingsLayout {
        RecordingsLayout::new(root).unwrap()
    }

    fn sample_start() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(8, 30, 0)
            .unwrap()
    }

    fn spec_root() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(r"C:\srv\nian-vision\recordings")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/srv/nian-vision/recordings")
        }
    }

    fn touch(path: &Path) {
        std::fs::File::create(path).unwrap();
    }

    #[test]
    fn layout_matches_spec_example() {
        let root = spec_root();
        let layout = RecordingsLayout::new(&root).unwrap();
        let camera = CameraId::parse("cam-1").unwrap();
        let path = layout
            .allocate_segment(&camera, sample_start())
            .unwrap()
            .final_path;
        // Nothing exists yet, so the bare spec name is allocated.
        assert_eq!(path, root.join("cam-1/2026/08/26/08-30-00.mkv"));
    }

    #[test]
    fn rejects_relative_and_root_storage_roots() {
        assert!(RecordingsLayout::new("relative/path").is_err());
        #[cfg(windows)]
        assert!(RecordingsLayout::new(r"C:\").is_err());
        #[cfg(not(windows))]
        assert!(RecordingsLayout::new("/").is_err());
    }

    #[test]
    fn naming_forms_render_and_roundtrip() {
        let time = sample_start().time();

        assert_eq!(segment_file_name(time), "08-30-00.mkv");
        assert_eq!(partial_file_name(time), "08-30-00.partial.mkv");
        assert_eq!(segment_file_name_with_sequence(time, 2), "08-30-00-2.mkv");
        assert_eq!(
            partial_file_name_with_sequence(time, 12),
            "08-30-00-12.partial.mkv"
        );

        let bare = parse_segment_file_name("08-30-00.mkv").unwrap();
        assert_eq!(bare.sequence, 1);
        assert!(!bare.is_partial);
        assert_eq!(bare.started_at, time);

        let sequenced_partial = parse_segment_file_name("08-30-00-7.partial.mkv").unwrap();
        assert_eq!(sequenced_partial.sequence, 7);
        assert!(sequenced_partial.is_partial);
        assert_eq!(sequenced_partial.started_at, time);

        // Recovery rename target of a sequenced partial is the matching
        // sequenced final name.
        let partial_name = partial_file_name_with_sequence(time, 3);
        let parsed = parse_segment_file_name(&partial_name).unwrap();
        assert_eq!(
            segment_file_name_with_sequence(parsed.started_at, parsed.sequence),
            segment_file_name_with_sequence(time, 3)
        );
    }

    #[test]
    fn parse_rejects_foreign_and_noncanonical_names() {
        for bad in [
            "notes.txt",
            "video.mp4",
            "8-30-00.mkv",
            "123456.mkv",
            "08-30-00",
            "",
            "08-30-00.final.mkv",
            // sequence suffixes are canonical-only
            "08-30-00-1.mkv",   // 1 is spelled bare
            "08-30-00-01.mkv",  // leading zero
            "08-30-00-0.mkv",   // zero is never valid
            "08-30-00-x.mkv",   // not numeric
            "08-30-00-2-3.mkv", // one suffix only
        ] {
            assert!(parse_segment_file_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn allocations_in_same_second_never_collide() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        // The allocator never creates directories; emulate the recorder.
        std::fs::create_dir_all(layout.day_dir(&camera, sample_start().date())).unwrap();

        let first = layout.allocate_segment(&camera, sample_start()).unwrap();
        touch(&first.partial_path); // recorder creates the partial…

        let second = layout.allocate_segment(&camera, sample_start()).unwrap();
        touch(&second.partial_path);

        let third = layout.allocate_segment(&camera, sample_start()).unwrap();

        let names: Vec<String> = [first, second, third]
            .iter()
            .map(|p| {
                p.final_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["08-30-00.mkv", "08-30-00-2.mkv", "08-30-00-3.mkv"]
        );

        // Every allocated name still parses back to the same wall-clock start.
        for name in &names {
            let parsed = parse_segment_file_name(name).unwrap();
            assert_eq!(parsed.started_at, sample_start().time());
        }
    }

    #[test]
    fn existing_finalized_segment_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();
        // A completed recording exists — no partial anywhere.
        touch(&day_dir.join("08-30-00.mkv"));

        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert!(
            allocated
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-2.mkv")
        );
    }

    #[test]
    fn allocator_reuses_gaps_and_skips_both_name_forms() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.mkv")); // seq 1 taken (final)
        touch(&day_dir.join("08-30-00-2.partial.mkv")); // seq 2 taken (partial)

        // Gap reuse: -2 is occupied too, but an unrelated leftover such as a
        // stray -5 final would be skipped just the same.
        touch(&day_dir.join("08-30-00-5.mkv"));
        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert!(
            allocated
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-3.mkv")
        );

        // Other times of day are irrelevant to the collision check.
        let other_time = NaiveDate::from_ymd_opt(2026, 8, 26)
            .unwrap()
            .and_hms_opt(8, 30, 1)
            .unwrap();
        let other = layout.allocate_segment(&camera, other_time).unwrap();
        assert!(
            other
                .final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("08-30-01.mkv")
        );
    }

    #[test]
    fn missing_day_dir_allocates_bare_name() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            allocated.final_path.file_name().unwrap().to_string_lossy(),
            "08-30-00.mkv"
        );
        assert_eq!(
            allocated
                .partial_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "08-30-00.partial.mkv"
        );
    }

    #[test]
    fn subsecond_claim_timestamps_do_not_reuse_taken_sequences() {
        // Regression: segment names encode whole seconds only, while a live
        // clock carries nanoseconds. Occupancy matching must therefore be
        // second-granular — an existing 08-30-00.mkv must push a claim made
        // at 08:30:00.877 on to sequence 2.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("08-30-00.mkv"), b"published").unwrap();

        let subsecond_start = sample_start()
            .with_nanosecond(877_000_000)
            .expect("valid subsecond time");
        let claim = layout.claim_segment(&camera, subsecond_start).unwrap();
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.mkv",
            "sequence 1 is occupied despite the subsecond mismatch"
        );
    }

    #[test]
    fn recovered_final_reserves_its_sequence_against_clock_rollback() {
        // Identity safety remediation §2 (review row: "recovered final
        // reserves sequence"): `08-30-00.recovered.mkv` does not parse as a
        // segment name, yet it OCCUPIES the (08:30:00, sequence 1) identity
        // of its canonical stem. A wall-clock rollback (manual change, NTP
        // backward adjustment, DST repeated local time) must never hand
        // that identity to new footage — the allocator must pick
        // 08-30-00-2.partial.mkv.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.recovered.mkv"));

        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            allocated
                .partial_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "08-30-00-2.partial.mkv",
            "the recovered transaction identity must never be re-allocated"
        );
        assert_eq!(
            allocated.final_path.file_name().unwrap().to_string_lossy(),
            "08-30-00-2.mkv"
        );
    }

    #[test]
    fn tombstone_alone_reserves_its_sequence() {
        // Identity safety remediation §2 (review row: "tombstone reserves
        // sequence"): even when only `08-30-00.recovered.mkv.done` remains
        // (the recovered final since moved away by an M4 policy, say),
        // sequence 1 stays reserved.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.recovered.mkv.done"));

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
    }

    #[test]
    fn recovered_and_normal_files_produce_the_next_correct_sequence() {
        // Identity safety remediation §2 (review row: "recovered + normal
        // files produce the next correct sequence"): reservation counts
        // across BOTH name grammars — normal seq 2 plus the recovered
        // transaction at seq 1 → the next free sequence is 3.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.recovered.mkv")); // seq 1 (transaction)
        touch(&day_dir.join("08-30-00.recovered.mkv.done")); // seq 1 (tombstone)
        touch(&day_dir.join("08-30-00-2.mkv")); // seq 2 (normal recording)

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-3.partial.mkv"
        );
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-3.mkv"
        );
    }

    #[test]
    fn recovery_scratch_reserves_its_sequence() {
        // Identity safety remediation §2 (preferred reservation): a valid
        // exact-grammar scratch occupies its stem's identity too — wasting
        // a sequence is harmless, identity ambiguity is not.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.recovery-4194305-0-123456789.tmp"));

        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            allocated
                .partial_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
    }

    #[test]
    fn unknown_foreign_files_reserve_nothing() {
        // Identity safety remediation §2 (review row: "Unknown foreign
        // files do not reserve sequence"): arbitrary operator files must
        // never consume recording identities.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("holiday.mkv"));
        touch(&day_dir.join("notes.txt"));
        touch(&day_dir.join("08-30-00.recovered.mkv.doneX"));
        touch(&day_dir.join("08-30-00.recovery-not-ours.txt"));
        touch(&day_dir.join("12-00-00.recovered.mkv")); // different second

        let allocated = layout.allocate_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            allocated
                .partial_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "08-30-00.partial.mkv",
            "foreign names must not reserve anything"
        );
    }

    #[test]
    fn identity_reservation_respects_whole_second_granularity() {
        // Identity safety remediation §2 (review row: "whole-second
        // comparison remains correct"): a recovered final at 08-30-00 also
        // forces a subsecond 08:30:00.877 claim to sequence 2, while the
        // NEXT whole second still receives the bare name.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        touch(&day_dir.join("08-30-00.recovered.mkv"));

        let subsecond_start = sample_start()
            .with_nanosecond(877_000_000)
            .expect("valid subsecond time");
        let claim = layout.claim_segment(&camera, subsecond_start).unwrap();
        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv",
            "reservation must be second-granular, not name-exact"
        );

        let next_second = sample_start().with_second(1).expect("valid time");
        let later = layout.allocate_segment(&camera, next_second).unwrap();
        assert_eq!(
            later.partial_path.file_name().unwrap().to_string_lossy(),
            "08-30-01.partial.mkv",
            "the neighboring second stays untouched"
        );
    }

    // ---- Post-claim identity fence (atomic identity claim remediation §5)
    //
    // The gate parks a claim AFTER its advisory scan chose sequence 1 but
    // BEFORE the candidate partial is created — the deterministic stand-in
    // for a stale non-atomic directory snapshot. A competing reservation
    // planted inside that window must be caught by the post-claim fence:
    // the losing candidate is relinquished and a higher sequence retried.

    /// Runs `claim_segment` on a thread against the layout's day directory,
    /// parks it at the claim gate, lets the test plant a competing object,
    /// releases, and returns the final claim.
    fn claim_racing_planted_object(
        layout: &RecordingsLayout,
        camera: &CameraId,
        plant: &dyn Fn(&Path),
    ) -> ClaimedSegment {
        claim_racing_planted_object_at(layout, camera, sample_start(), plant)
    }

    /// Like [`claim_racing_planted_object`] but with an explicit (possibly
    /// sub-second) claim timestamp.
    fn claim_racing_planted_object_at(
        layout: &RecordingsLayout,
        camera: &CameraId,
        started_at: NaiveDateTime,
        plant: &dyn Fn(&Path),
    ) -> ClaimedSegment {
        use crate::test_hooks::arm_claim_identity_gate;

        let _fault_serialization_guard = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let day_dir = layout.day_dir(camera, started_at.date());
        let (_guard, gate) = arm_claim_identity_gate(&day_dir);

        let layout = layout.clone();
        let camera = camera.clone();
        let claimer = std::thread::spawn(move || {
            layout
                .claim_segment(&camera, started_at)
                .expect("claim must succeed")
        });

        // The claim scanned an (apparently) free namespace and parked
        // BEFORE creating its candidate.
        gate.wait_arrived();
        plant(&day_dir);
        gate.release();

        claimer.join().expect("claim thread must not panic")
    }

    #[test]
    fn claim_fence_retries_when_a_recovered_reservation_appears_midflight() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        let claim = claim_racing_planted_object(&layout, &camera, &|day_dir| {
            std::fs::write(
                day_dir.join("08-30-00.recovered.mkv"),
                b"published-recovery",
            )
            .unwrap();
        });

        // Sequence 1 was NEVER returned: the fence detected the recovered
        // reservation, relinquished the candidate and retried at 2.
        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.mkv"
        );
        // The temporary losing partial was removed cleanly...
        assert!(
            !day_dir.join("08-30-00.partial.mkv").exists(),
            "the losing candidate must be relinquished"
        );
        assert!(claim.partial_path().is_file());
        // ...and the competing recovered final is untouched.
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.recovered.mkv")).unwrap(),
            b"published-recovery"
        );
    }

    #[test]
    fn claim_fence_retries_on_a_tombstone_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        let tombstone_bytes =
            b"NIAN-RECOVERY-TOMBSTONE v2\noriginal: 08-30-00.partial.mkv\nfinal: 08-30-00.recovered.mkv\nsize: 3\n";
        let claim = claim_racing_planted_object(&layout, &camera, &|day_dir| {
            std::fs::write(day_dir.join("08-30-00.recovered.mkv.done"), tombstone_bytes).unwrap();
        });

        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv",
            "a lone tombstone still reserves the identity"
        );
        assert!(!day_dir.join("08-30-00.partial.mkv").exists());
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.recovered.mkv.done")).unwrap(),
            tombstone_bytes
        );
    }

    #[test]
    fn claim_fence_retries_on_a_normal_final_appearing() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        let claim = claim_racing_planted_object(&layout, &camera, &|day_dir| {
            std::fs::write(day_dir.join("08-30-00.mkv"), b"finalized").unwrap();
        });

        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
        assert!(!day_dir.join("08-30-00.partial.mkv").exists());
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.mkv")).unwrap(),
            b"finalized"
        );
    }

    #[test]
    fn claim_fence_retries_on_valid_scratch_appearing() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        // Exact production scratch grammar only (final safety remediation
        // §3): this shape is a Nian-owned reservation.
        let scratch_name = "08-30-00.recovery-4194305-0-123456789.tmp";
        let claim = claim_racing_planted_object(&layout, &camera, &|day_dir| {
            std::fs::write(day_dir.join(scratch_name), b"active scratch").unwrap();
        });

        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
        assert!(!day_dir.join("08-30-00.partial.mkv").exists());
        assert_eq!(
            std::fs::read(day_dir.join(scratch_name)).unwrap(),
            b"active scratch"
        );
    }

    #[test]
    fn claim_fence_ignores_foreign_files() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        // Foreign/Unknown names (wrong scratch grammar, operator files) are
        // NOT Nian-owned reservations: the fence must NOT fire.
        let claim = claim_racing_planted_object(&layout, &camera, &|day_dir| {
            std::fs::write(day_dir.join("holiday.mkv"), b"operator").unwrap();
            std::fs::write(day_dir.join("notes.txt"), b"operator").unwrap();
            std::fs::write(day_dir.join("08-30-00.recovery-not-ours.txt"), b"operator").unwrap();
        });

        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00.partial.mkv",
            "foreign files reserve nothing: sequence 1 stays valid"
        );
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00.mkv"
        );
        assert!(claim.partial_path().is_file());
    }

    #[test]
    fn claim_fence_treats_a_subsecond_claim_time_as_the_same_identity_second() {
        // Sub-second identity remediation §3: names parsed from disk carry
        // WHOLE seconds only, while a live claim timestamp carries
        // nanoseconds. The fence's identity comparison must therefore be
        // whole-second: a claim made at 08:30:00.123456789 must see the
        // freshly planted 08-30-00.recovered.mkv reservation as the SAME
        // identity second and retry — comparing raw `NaiveTime`s would miss
        // the conflict and hand the transaction's identity to new footage.
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();

        let subsecond = sample_start()
            .with_nanosecond(123_456_789)
            .expect("valid subsecond time");
        let claim = claim_racing_planted_object_at(&layout, &camera, subsecond, &|day_dir| {
            std::fs::write(
                day_dir.join("08-30-00.recovered.mkv"),
                b"published-recovery",
            )
            .unwrap();
        });

        // The reservation at 08-30-00 must fence the 08:30:00.123456789
        // claim off sequence 1 exactly as it fences a whole-second claim.
        assert_eq!(
            claim.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv",
            "the fence must fire across the subsecond-vs-whole-second comparison"
        );
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.mkv"
        );
        assert!(
            !day_dir.join("08-30-00.partial.mkv").exists(),
            "the losing candidate must be relinquished"
        );
        assert!(claim.partial_path().is_file());
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.recovered.mkv")).unwrap(),
            b"published-recovery",
            "the competing recovered final is untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fence_failure_with_uncleanable_candidate_surfaces_both_contexts() {
        // Sub-second identity remediation §4: when the fence cannot even
        // VALIDATE the identity, the candidate is never returned; its
        // relinquish is attempted, and if the removal ITSELF fails, that
        // failure is surfaced TYPED together with the fence failure — the
        // ambiguous leftover ownership is observable, never silently
        // discarded. The day-directory-loss hook produces a real-world
        // "storage vanished mid-claim" fault: both the fence enumeration
        // and the cleanup fail with genuine ENOENTs. (Unix-only: removing
        // a directory that still holds the open claim handle is a Windows
        // error, so the fault could not fire there.)
        let _fault_serialization = crate::test_hooks::FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-vanish").unwrap();
        let day_dir = layout.day_dir(&camera, sample_start().date());

        let _guard = crate::test_hooks::arm_claim_day_dir_loss(&day_dir);
        let outcome = layout.claim_segment(&camera, sample_start());

        let error = outcome.expect_err("a vanished day directory must fail the claim");
        match error {
            StorageError::ClaimFenceCleanup {
                candidate,
                fence_error,
                cleanup,
            } => {
                assert_eq!(
                    candidate,
                    day_dir.join("08-30-00.partial.mkv"),
                    "the typed error must name the exact unreturned candidate"
                );
                assert!(
                    matches!(&*fence_error, StorageError::Io { path, .. } if path == &day_dir),
                    "the fence failure must be the vanished-directory enumeration \
                     error: {fence_error:?}"
                );
                assert_eq!(
                    cleanup.kind(),
                    std::io::ErrorKind::NotFound,
                    "the candidate cleanup must have failed on the vanished directory"
                );
            }
            other => panic!("expected ClaimFenceCleanup, got {other:?}"),
        }
        // The fault really fired: the whole day directory (with the
        // candidate) is gone, which is exactly why cleanup could not
        // succeed and the dual-context error was required.
        assert!(!day_dir.exists());
    }

    #[test]
    fn claims_in_same_second_are_distinct_and_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let first = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            first.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00.partial.mkv"
        );

        // While the first claim is alive, a second claim for the same second
        // must land on the next sequence — never the same file.
        let second = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            second.partial_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-2.partial.mkv"
        );
        assert_ne!(first.partial_path(), second.partial_path());
        assert!(first.partial_path().is_file());
        assert!(second.partial_path().is_file());

        // Dropping a claim keeps the file (crash-recovery semantics).
        drop(second);
        let day_dir = first.partial_path().parent().unwrap().to_path_buf();
        assert!(day_dir.join("08-30-00-2.partial.mkv").is_file());
    }

    #[test]
    fn claim_skips_existing_files_without_overwriting_them() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        // Pre-existing content of both forms must survive untouched.
        let day_dir = layout.day_dir(&camera, sample_start().date());
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("08-30-00.mkv"), b"finalized").unwrap();
        std::fs::write(day_dir.join("08-30-00-2.partial.mkv"), b"partial").unwrap();

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_eq!(
            claim.final_path().file_name().unwrap().to_string_lossy(),
            "08-30-00-3.mkv"
        );
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00.mkv")).unwrap(),
            b"finalized"
        );
        assert_eq!(
            std::fs::read(day_dir.join("08-30-00-2.partial.mkv")).unwrap(),
            b"partial"
        );
    }

    #[test]
    fn publish_no_replace_moves_content_and_refuses_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let claim = layout.claim_segment(&camera, sample_start()).unwrap();
        std::fs::write(claim.partial_path(), b"segment-bytes").unwrap();

        publish_no_replace(claim.partial_path(), claim.final_path()).unwrap();
        assert!(!claim.partial_path().exists(), "partial link must be gone");
        assert_eq!(std::fs::read(claim.final_path()).unwrap(), b"segment-bytes");

        // Publishing another segment's content onto the now-existing final
        // name is refused and the existing recording stays byte-identical.
        let other = layout.claim_segment(&camera, sample_start()).unwrap();
        assert_ne!(other.final_path(), claim.final_path());
        std::fs::write(other.partial_path(), b"other").unwrap();
        match publish_no_replace(other.partial_path(), claim.final_path()) {
            Err(StorageError::DestinationExists { destination }) => {
                assert_eq!(destination, claim.final_path().to_path_buf());
            }
            other => panic!("expected DestinationExists, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(claim.final_path()).unwrap(),
            b"segment-bytes",
            "existing finalized segment must never be replaced"
        );
    }

    #[test]
    fn refused_publication_leaves_abandoned_partial_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-1").unwrap();

        let first = layout.claim_segment(&camera, sample_start()).unwrap();
        std::fs::write(first.partial_path(), b"first-content").unwrap();
        publish_no_replace(first.partial_path(), first.final_path()).unwrap();

        let second = layout.claim_segment(&camera, sample_start()).unwrap();
        std::fs::write(second.partial_path(), b"second-content").unwrap();

        // Force a collision: second's final name differs from first's, so
        // publish second's content onto FIRST's final path to emulate a
        // reconciliation race with an already-published segment.
        match publish_no_replace(second.partial_path(), first.final_path()) {
            Err(StorageError::DestinationExists { .. }) => {}
            other => panic!("expected DestinationExists, got {other:?}"),
        }

        // The abandoned partial is exactly where recovery expects it: same
        // path, original bytes, canonical `.partial` name.
        let path = second.partial_path();
        assert!(path.is_file(), "abandoned partial must remain on disk");
        assert_eq!(std::fs::read(path).unwrap(), b"second-content");
        let parsed = parse_segment_file_name(path.file_name().unwrap().to_str().unwrap())
            .expect("abandoned partial keeps its canonical name");
        assert!(parsed.is_partial);
        assert_eq!(parsed.started_at, sample_start().time());

        // And the colliding final was not modified by the failed attempt.
        assert_eq!(std::fs::read(first.final_path()).unwrap(), b"first-content");

        // Publishing to the correct (free) final name still works after the
        // refusal — the failure did not poison the slot.
        publish_no_replace(second.partial_path(), second.final_path()).unwrap();
        assert_eq!(
            std::fs::read(second.final_path()).unwrap(),
            b"second-content"
        );
        assert!(!second.partial_path().exists());
    }

    #[test]
    fn rejects_unsafe_components() {
        for evil in ["..", ".", "", "a/b", r"a\b", "a:b", "a\nb"] {
            assert!(
                checked_component(evil).is_err(),
                "accepted component {evil:?}"
            );
        }
    }

    #[test]
    fn camera_dir_preflight_proves_writeability_and_cleans_up() {
        // Final correctness remediation §8 + final safety remediation §4:
        // the pre-flight must succeed on a writable tree, must NOT
        // overwrite anything, and must leave no probe file behind. The
        // probe file's reserved name never classifies as a recording,
        // partial or recovery artifact.
        let _fault_serialization = PREFLIGHT_FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-preflight").unwrap();

        let dir = layout.ensure_camera_dir(&camera).unwrap();
        assert!(dir.is_dir());
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "the probe must clean up after itself: {leftovers:?}"
        );

        // Calling twice (directory already exists) still succeeds — this
        // is exactly the case where create_dir_all alone proves nothing.
        layout.ensure_camera_dir(&camera).unwrap();
        assert!(
            crate::classification::classify_recording_file_name(".nian-write-probe-1-0-42.tmp")
                == crate::classification::RecordingFileKind::Unknown
        );
    }

    #[test]
    fn preflight_retries_with_a_fresh_probe_identity_on_collision() {
        // Final safety remediation §4: an AlreadyExists from a survived old
        // probe retries with another unique probe identity instead of
        // classifying the storage root unavailable — and still leaves no
        // probe behind.
        let _fault_serialization = PREFLIGHT_FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PROBE_FIRST_CREATE_COLLIDES.store(true, Ordering::SeqCst);
        PROBE_FIRST_CREATE_DONE.store(false, Ordering::SeqCst);
        PROBE_REMOVE_FAILS.store(false, Ordering::SeqCst);

        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-collision").unwrap();

        let dir = layout.ensure_camera_dir(&camera);
        PROBE_FIRST_CREATE_COLLIDES.store(false, Ordering::SeqCst);
        let dir = dir.expect("a probe collision must retry, not fail the pre-flight");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "no probe may survive the pre-flight: {leftovers:?}"
        );
    }

    #[test]
    fn preflight_surfaces_probe_removal_failure() {
        // Final safety remediation §4: a directory where files can be
        // created but NOT removed is not healthy enough for the
        // recording/retention lifecycle — cleanup failure is surfaced as a
        // genuine storage error, never silently ignored.
        let _fault_serialization = PREFLIGHT_FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PROBE_FIRST_CREATE_COLLIDES.store(false, Ordering::SeqCst);
        PROBE_REMOVE_FAILS.store(true, Ordering::SeqCst);

        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-undeletable").unwrap();

        let outcome = layout.ensure_camera_dir(&camera);
        PROBE_REMOVE_FAILS.store(false, Ordering::SeqCst);
        assert!(
            outcome.is_err(),
            "a non-removable probe must fail the pre-flight"
        );
    }

    #[test]
    fn camera_dir_preflight_fails_when_the_target_is_not_a_directory() {
        // A genuine infrastructure failure: the camera path is occupied by
        // a file, so neither the directory nor any new recording file can
        // exist there — regardless of privileges.
        let _fault_serialization = PREFLIGHT_FAULT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let layout = layout_in(temp.path());
        let camera = CameraId::parse("cam-blocked").unwrap();
        let camera_path = layout.camera_dir(&camera);
        std::fs::create_dir_all(camera_path.parent().unwrap()).unwrap();
        std::fs::write(&camera_path, b"occupied").unwrap();

        assert!(
            layout.ensure_camera_dir(&camera).is_err(),
            "a file occupying the camera path must fail the writeability pre-flight"
        );
        assert_eq!(
            std::fs::read(&camera_path).unwrap(),
            b"occupied",
            "user data must never be overwritten by the pre-flight"
        );
    }
}
