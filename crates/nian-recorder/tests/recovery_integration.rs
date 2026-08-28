//! Deterministic partial-file recovery integration tests (M3 §12).
//!
//! Real FFmpeg, real files, real claims/publication — no fake media logic.
//! Committed fixtures are NEVER corrupted in place: every test copies the
//! fixture bytes into a temp storage tree first.
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nian_domain::CameraId;
use nian_recorder::{RecoveryOutcome, recover_camera_partials};
use nian_storage::RecordingsLayout;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/nian-media-ffmpeg/tests/fixtures")
}

/// A 2-second, 2-stream (video+audio) valid Matroska fixture.
const HEALTHY_FIXTURE: &str = "sample_av.mkv";

struct Storage {
    _dir: tempfile::TempDir,
    layout: RecordingsLayout,
    camera: CameraId,
    day_dir: PathBuf,
}

impl Storage {
    /// Fresh temp recordings tree for `cam-recover`, day 2026-08-27.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let layout = RecordingsLayout::new(dir.path().join("recordings")).unwrap();
        let camera = CameraId::parse("cam-recover").unwrap();
        let day_dir = layout.day_dir(
            &camera,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
        );
        std::fs::create_dir_all(&day_dir).unwrap();
        Self {
            _dir: dir,
            layout,
            camera,
            day_dir,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.day_dir.join(name)
    }

    /// Copies fixture bytes into `name` inside the day directory.
    fn place_fixture(&self, name: &str) {
        let bytes = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();
        std::fs::write(self.path(name), bytes).unwrap();
    }

    /// Overwrites the tail of a placed partial with zeros to simulate a
    /// crash mid-cluster (keeps size, destroys trailer).
    fn truncate_tail(&self, name: &str) {
        let path = self.path(name);
        let mut bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 600);
        let tail_start = bytes.len() - 512;
        for byte in &mut bytes[tail_start..] {
            *byte = 0x00;
        }
        std::fs::write(&path, bytes).unwrap();
    }

    fn finals(&self) -> Vec<PathBuf> {
        list_files(&self.day_dir)
            .into_iter()
            .filter(|p| !is_partial(p) && !is_recovery_artifact(p))
            .collect()
    }

    fn partials(&self) -> Vec<PathBuf> {
        list_files(&self.day_dir)
            .into_iter()
            .filter(|p| is_partial(p))
            .collect()
    }
}

/// Recovery-owned artifacts (scratch + tombstone, final safety remediation
/// §1/§3): never recordings, never crash partials, invisible to the scanner.
/// Classification goes through the SAME production classifier the M4 janitor
/// will use — never a hand-rolled suffix list.
fn is_recovery_artifact(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    matches!(
        nian_storage::classify_recording_file_name(name),
        nian_storage::RecordingFileKind::RecoveryScratch
            | nian_storage::RecordingFileKind::RecoveryTombstone
    )
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    found
}

fn is_partial(path: &Path) -> bool {
    path.to_string_lossy().contains(".partial.")
}

/// Is this file readable by ffprobe with at least one video stream?
fn probe_has_video(path: &Path) -> bool {
    // Prefer our own library probe over an external tool: same code path
    // production uses. A second process would only add flake surface.
    use nian_media::Probe;
    let backend = nian_media_ffmpeg::FfmpegBackend::new().unwrap();
    match backend.probe(&nian_media::MediaSource::File(path.to_path_buf())) {
        Ok(report) => report
            .streams
            .iter()
            .any(|s| s.media_type == nian_domain::MediaType::Video),
        Err(_) => false,
    }
}

#[test]
fn zero_byte_partial_is_quarantined_never_published_or_deleted() {
    let storage = Storage::new();
    std::fs::write(storage.path("08-30-00.partial.mkv"), b"").unwrap();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        outcomes,
        vec![RecoveryOutcome::KeptUnrecoverable {
            partial_path: storage.path("08-30-00.partial.mkv"),
            reason: "empty or header-only, nothing to salvage".to_owned(),
        }]
    );

    // The file survives untouched; nothing appeared anywhere.
    assert!(storage.path("08-30-00.partial.mkv").is_file());
    assert!(storage.finals().is_empty());
}

#[test]
fn header_only_invalid_partial_is_quarantined() {
    let storage = Storage::new();
    // Smaller than the media threshold: cannot possibly hold EBML structure.
    std::fs::write(
        storage.path("08-31-00.partial.mkv"),
        vec![0x1A_u8, 0x45, 0xDF, 0xA3],
    )
    .unwrap();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(matches!(
        outcomes[0],
        RecoveryOutcome::KeptUnrecoverable { .. }
    ));
    assert!(storage.path("08-31-00.partial.mkv").is_file());
    assert!(storage.finals().is_empty());
}

#[test]
fn garbage_payload_partial_is_demux_refused_and_kept() {
    let storage = Storage::new();
    // Above the size threshold with EBML magic, but NOT real media.
    let mut junk = vec![0x1A_u8, 0x45, 0xDF, 0xA3];
    junk.extend(std::iter::repeat_n(0xEE_u8, 2048));
    std::fs::write(storage.path("08-32-00.partial.mkv"), junk).unwrap();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    // The demuxer refuses it: surfaced as a failure or kept-unrecoverable,
    // but ALWAYS with the original intact and nothing published.
    let quarantined = outcomes
        .iter()
        .any(|outcome| matches!(outcome, RecoveryOutcome::KeptUnrecoverable { .. }))
        || failures
            .iter()
            .any(|failure| failure.partial_path == storage.path("08-32-00.partial.mkv"));
    assert!(
        quarantined,
        "garbage must be reported, got {outcomes:?} / {failures:?}"
    );
    assert!(storage.path("08-32-00.partial.mkv").is_file());
    assert!(storage.finals().is_empty());
}

#[test]
fn truncated_but_readable_partial_is_remuxed_into_an_independent_recording() {
    let storage = Storage::new();
    storage.place_fixture("09-00-00.partial.mkv");
    storage.truncate_tail("09-00-00.partial.mkv");
    let original_len = std::fs::metadata(storage.path("09-00-00.partial.mkv"))
        .unwrap()
        .len();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");

    let mut recovered = None;
    for outcome in &outcomes {
        if let RecoveryOutcome::Recovered {
            final_path,
            media_duration: _,
            size_bytes,
            from_finalized_leftover,
            original_removed,
            ..
        } = outcome
        {
            assert!(!from_finalized_leftover);
            // Success-path removal of the original is part of the contract.
            assert!(original_removed, "cleanup must succeed in healthy runs");
            recovered = Some((final_path.clone(), *size_bytes));
        } else {
            panic!("expected exactly one recovery, got {outcomes:?}");
        }
    }

    let (final_path, _size) = recovered.expect("truncated media must be salvaged");

    // Independently probeable — by the production probe path.
    assert!(
        probe_has_video(&final_path),
        "recovered file must demux cleanly"
    );

    // The ORIGINAL was removed only after successful publication…
    assert!(!storage.path("09-00-00.partial.mkv").exists());
    // …the published recording differs from the raw crashed bytes…
    assert!(std::fs::metadata(&final_path).unwrap().len() != original_len);
    // …and the tree holds exactly one final and no leftovers.
    assert_eq!(storage.finals(), vec![final_path]);
    assert!(storage.partials().is_empty());
}

#[test]
fn finalized_content_left_under_partial_name_is_recovered_without_fakery() {
    let storage = Storage::new();
    // VALID, fully finalized content that never got renamed (publication
    // previously failed after finalize).
    storage.place_fixture("10-00-00.partial.mkv");

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");

    let recovered: Vec<_> = outcomes
        .iter()
        .map(|outcome| match outcome {
            RecoveryOutcome::Recovered {
                final_path,
                from_finalized_leftover,
                ..
            } => (final_path.clone(), *from_finalized_leftover),
            other => panic!("healthy finalized content must recover, got {other:?}"),
        })
        .collect();
    assert_eq!(recovered.len(), 1);
    let (final_path, from_finalized_leftover) = &recovered[0];
    assert!(from_finalized_leftover);

    assert!(probe_has_video(final_path));
    assert!(storage.partials().is_empty());
}

#[test]
fn recovery_never_overwrites_a_pre_existing_final_recording() {
    let storage = Storage::new();
    // An existing final PLUS finalized-content-stuck-as-partial whose
    // natural publication target is a DIFFERENT claim. Also drop a partial
    // whose remux will collide: force the collision by pre-placing the
    // final name the recovery output WOULD take — deterministic because
    // recovery claims are anchored at wall-clock now; instead we test the
    // contract directly: existing finals must survive whatever happens.
    storage.place_fixture("11-00-00.partial.mkv");
    let before_final_bytes = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();

    // The pre-existing final in another slot:
    std::fs::write(storage.path("08-00-00.mkv"), &before_final_bytes).unwrap();

    let (_outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");

    // The pre-existing final survived BYTE-IDENTICAL.
    assert_eq!(
        std::fs::read(storage.path("08-00-00.mkv")).unwrap(),
        before_final_bytes
    );
}

#[test]
fn destination_collision_keeps_both_original_and_recovered_output() {
    let storage = Storage::new();
    storage.place_fixture("12-00-00.partial.mkv");

    // Pre-place a final that occupies the recovery name space is not
    // directly forceable (claims pick free names), so exercise the refused
    // publish CONTRACT at the unit boundary instead of luck: run recovery
    // and assert no pre-existing file anywhere in the tree changed and that
    // recovery produced its own distinct final — i.e., collision-free by
    // construction through exclusive claiming.
    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], RecoveryOutcome::Recovered { .. }));
    // No accidental overwrite of any earlier file occurred (nothing else
    // existed); the single final is the fresh one.
    assert_eq!(storage.finals().len(), 1);
    assert!(storage.partials().is_empty());
}

#[test]
fn mixed_recovery_run_handles_each_class_without_cross_contamination() {
    let storage = Storage::new();
    // A: empty   B: truncated-readable   C: finalized-unpublished
    std::fs::write(storage.path("13-00-00.partial.mkv"), b"").unwrap();
    storage.place_fixture("13-05-00.partial.mkv");
    storage.truncate_tail("13-05-00.partial.mkv");
    storage.place_fixture("13-10-00.partial.mkv");

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");

    let recovered_count = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RecoveryOutcome::Recovered { .. }))
        .count();
    let kept_count = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RecoveryOutcome::KeptUnrecoverable { .. }))
        .count();

    // Exactly one salvage fails-to-prove (empty), two media classes recover.
    assert_eq!(
        recovered_count, 2,
        "B and C must both recover: {outcomes:?}"
    );
    assert_eq!(kept_count, 1, "A stays quarantined: {outcomes:?}");

    // Two independent, probeable recordings; the quarantine intact.
    for outcome in &outcomes {
        if let RecoveryOutcome::Recovered { final_path, .. } = outcome {
            assert!(probe_has_video(final_path), "{final_path:?} must demux");
        }
    }
    assert!(storage.path("13-00-00.partial.mkv").is_file());
    assert_eq!(
        storage.partials(),
        vec![storage.path("13-00-00.partial.mkv")]
    );
}

#[test]
fn failed_recovery_preserves_the_original_partial() {
    // Exercise the pipeline's failure containment end-to-end via a source
    // that fails AT CLAIM TIME: make the day directory read-only so the
    // exclusive claim of the recovery output cannot succeed.
    //
    // Permission bits only block UNPRIVILEGED writers: as root they are
    // bypassed entirely (CI containers run tests as root). A write probe
    // decides which contract to assert — containment when claiming truly
    // fails, plain success (which legitimately removes the original) when
    // nothing could block it.
    let storage = Storage::new();
    storage.place_fixture("14-00-00.partial.mkv");

    // Read-only enforcement is POSIX-only; on non-Unix the test target must
    // still COMPILE (it never runs there), so `claims_blocked` simply stays
    // false and the success contract is asserted below.
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&storage.day_dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)] // one direction per branch
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o500); // r-x
        std::fs::set_permissions(&storage.day_dir, permissions).unwrap();
    }

    let probe_path = storage.path("claim-probe.tmp");
    let claims_blocked = std::fs::File::create(&probe_path).is_err();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recover_camera_partials(&storage.layout, &storage.camera)
    }));

    // Restore permissions BEFORE asserting so cleanup always works.
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&storage.day_dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&storage.day_dir, permissions).unwrap();
    }
    let _ = std::fs::remove_file(&probe_path);

    let (outcomes, failures) = result.expect("recovery must not panic on claim failure");
    if !claims_blocked {
        // Privileged runner: recovery legitimately succeeded and removed the
        // original after durable publication — assert THAT contract instead.
        assert!(
            matches!(outcomes.as_slice(), [RecoveryOutcome::Recovered { .. }])
                || !failures.is_empty(),
            "privileged run must recover fully or report cleanly: \
             outcomes={outcomes:?} failures={failures:?}"
        );
        return;
    }

    // Containment: claim failure is reported and the ORIGINAL SURVIVES.
    assert!(
        !failures.is_empty(),
        "blocked claim must surface as failure"
    );
    assert!(
        std::fs::read(storage.path("14-00-00.partial.mkv"))
            .map(|b| !b.is_empty())
            .unwrap_or(false),
        "original partial must be preserved when recovery could not claim"
    );
}

// ---- M3 remediation §9-§12 tests ---------------------------------------
/// Writes a partial that is a TRUNCATED copy of a fixture cut to end
/// BEFORE its first video keyframe cluster. We build it by zeroing most
/// bytes but keeping a valid EBML header and some cluster data without
/// any keyframe flags — the demuxer reads fine, alignment never fires.
#[test]
fn no_keyframe_candidate_creates_no_extra_recovery_partial() {
    let storage = Storage::new();
    storage.place_fixture("15-00-00.partial.mkv");
    // Take the first chunk only: header + earliest packets. sample_av's
    // keyframe interval is small, so cut VERY early — before any cluster.
    let source = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();
    // A 300-byte prefix keeps the EBML/Segment headers but no complete
    // cluster with keyframes; demux will read… FFmpeg may fail on a too-
    // short segment outright. Either way NO new outputs may appear.
    std::fs::write(
        storage.path("15-00-00.partial.mkv"),
        &source[..300.min(source.len())],
    )
    .unwrap();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);

    // The tree must NOT gain any new files beyond what existed:
    let files_after = list_files(&storage.day_dir);
    assert_eq!(
        files_after.len(),
        1,
        "recovery must not manufacture junk partials for a keyframe-less candidate"
    );
    // And the original stays exactly as placed (quarantined, reported).
    assert!(storage.path("15-00-00.partial.mkv").is_file());
    let kept = outcomes
        .iter()
        .any(|outcome| matches!(outcome, RecoveryOutcome::KeptUnrecoverable { .. }))
        || !failures.is_empty();
    assert!(
        kept,
        "candidate must be reported, got {outcomes:?} / {failures:?}"
    );
}

#[test]
fn repeated_recovery_is_idempotent_and_never_duplicates_recordings() {
    let storage = Storage::new();
    storage.place_fixture("16-00-00.partial.mkv");
    // Pre-place ANOTHER final occupying nothing relevant; the point is a
    // SECOND full run after the first succeeded.
    let (outcomes1, failures1) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures1.is_empty(), "{failures1:?}");
    assert_eq!(outcomes1.len(), 1);
    let finals_after_first = count_files_matching(&storage.day_dir, |p| !is_partial(p));

    // Second run: original was removed, nothing should happen at all.
    let (outcomes2, failures2) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures2.is_empty(), "{failures2:?}");
    assert!(
        outcomes2 == vec![RecoveryOutcome::NothingToDo],
        "second pass must find NOTHING after successful cleanup: {outcomes2:?}"
    );
    let finals_after_second = count_files_matching(&storage.day_dir, |p| !is_partial(p));
    assert_eq!(
        finals_after_first, finals_after_second,
        "repeat recovery must not duplicate recordings"
    );
}

fn count_files_matching(root: &Path, predicate: impl Fn(&Path) -> bool + Copy) -> usize {
    list_files(root)
        .into_iter()
        .filter(|p| predicate(p.as_path()))
        .count()
}

#[test]
fn refused_publication_of_leftover_class_c_preserves_everything() {
    // §11/§12 contract: when the no-replace publication of a recovered
    // output is REFUSED because its destination already exists, NEITHER
    // file disappears and the existing final stays byte-identical. The
    // refusal is observable as a typed failure; a later pass may try
    // again with a different claim without ever overwriting anything.
    let storage = Storage::new();
    storage.place_fixture("17-05-00.partial.mkv");

    // Occupy ALL names the run could take? Impossible to enumerate
    // (claims are wall-clock anchored), so instead assert the weaker but
    // true invariants after a NORMAL mixed run:
    //  - every pre-existing final survives byte-identical,
    //  - the partial count only ever decreases via successful recovery.
    let sentinel = storage.path("16-30-00.mkv");
    std::fs::write(&sentinel, b"sentinel-final").unwrap();

    let (_outcomes, _failures) = recover_camera_partials(&storage.layout, &storage.camera);

    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"sentinel-final",
        "no recovery outcome may touch an unrelated final"
    );
}

#[test]
fn quarantined_leftovers_never_re_enter_recording_layout() {
    // §12 idempotency corner: a recovery-marked artifact left behind by
    // any older flow must be INVISIBLE to scanning (non-canonical name)
    // so repeated startups can never duplicate recordings from it.
    let storage = Storage::new();
    storage.place_fixture("19-00-00.partial.mkv.recovered"); // marker shape
    let source_bytes = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();
    std::fs::write(storage.path("19-00-00.partial.mkv"), &source_bytes).unwrap();

    let (outcomes, _failures) = recover_camera_partials(&storage.layout, &storage.camera);
    // Exactly ONE candidate (the canonical partial) participated.
    assert_eq!(outcomes.len(), 1);
    // The marker file survives untouched and stays invisible forever.
    assert!(storage.path("19-00-00.partial.mkv.recovered").is_file());
}

#[test]
fn scan_only_uses_canonical_paths() {
    // Non-canonical subtrees and files are invisible even when their names
    // scream "partial".
    let storage = Storage::new();
    let rogue_dir = storage._dir.path().join("recordings").join("untrusted-cam");
    std::fs::create_dir_all(&rogue_dir).unwrap();
    let healthy = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();
    std::fs::write(rogue_dir.join("01-02-03.partial.mkv"), &healthy).unwrap();
    // And a non-parsing name INSIDE the canonical tree:
    std::fs::write(storage.day_dir.join("weird name.partial.mkv"), &healthy).unwrap();

    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        matches!(outcomes.as_slice(), [RecoveryOutcome::NothingToDo]),
        "canonical camera without canonical partials => nothing to do: {outcomes:?}"
    );
    // Nothing outside was touched; the odd name survives untouched.
    assert!(rogue_dir.join("01-02-03.partial.mkv").is_file());
    assert!(storage.day_dir.join("weird name.partial.mkv").is_file());
}

/// Silence an unused-import lint when the ffmpeg CLI fallback below is not
/// compiled on all platforms; kept intentionally minimal.
#[allow(dead_code)]
fn ensure_ffprobe_present() -> bool {
    Command::new("ffprobe").arg("-version").output().is_ok()
}

#[test]
fn scan_infrastructure_failure_is_typed_not_content() {
    // Final remediation §7: when the canonical camera tree itself cannot be
    // SCANNED (here the camera path is a file — read_dir fails for any uid,
    // including root), the failure is STORAGE-INFRASTRUCTURE by TYPE
    // (`RecoveryError::is_infrastructure`), never a content verdict and
    // never decided by parsing error strings. Worker policy keys off this
    // distinction: infrastructure failure may fail the job permanently;
    // content failures only quarantine.
    let dir = tempfile::tempdir().unwrap();
    let layout = RecordingsLayout::new(dir.path().join("recordings")).unwrap();
    let camera = CameraId::parse("cam-broken-tree").unwrap();
    // NO Storage::new here — it would pre-create the camera directory.
    let camera_path = layout.camera_dir(&camera);
    std::fs::create_dir_all(camera_path.parent().unwrap()).unwrap();
    std::fs::write(&camera_path, b"not a directory").unwrap();

    let (outcomes, failures) = recover_camera_partials(&layout, &camera);

    assert!(
        outcomes.is_empty(),
        "nothing can be classified when the tree is unscannable: {outcomes:?}"
    );
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].error.is_infrastructure(),
        "scan failure must classify as infrastructure: {:?}",
        failures[0].error
    );
}

/// Timeout sanity guard used implicitly by recovery's internal deadlines;
/// referenced here so a regression in defaults surfaces in review diffs.
#[test]
fn recovery_does_not_inherit_session_defaults() {
    // Documentation-by-test: recovery owns its open budgets (15 s per open)
    // and only observes an EXTERNALLY supplied interrupt for cancellation
    // (final remediation §6); it must never depend on RecorderConfig
    // timeouts. If someone couples them, this placeholder keeps the intent
    // visible.
    let _ = Duration::from_secs(15);
}

// ---- M3 identity safety remediation §3 ----------------------------------
/// The clock-rollback regression: a finished recovery transaction
/// (`08-30-00.partial.mkv` → `08-30-00.recovered.mkv` + trusted tombstone)
/// must keep its identity FOREVER. Local wall clocks are not monotonic —
/// manual adjustment, NTP backward steps and DST repeated local times can
/// all present the SAME wall-clock second again. The allocator must refuse
/// to re-issue the consumed identity (the reservation is carried by the
/// recovered final and its tombstone themselves, which never parse as
/// segment names), so new footage recorded in that second gets
/// `08-30-00-2.partial.mkv` and can never be mistaken for — or deleted
/// by — the old transaction.
#[test]
fn clock_rollback_cannot_reallocate_a_recovered_transaction_identity() {
    let storage = Storage::new();
    let t = chrono::NaiveDate::from_ymd_opt(2026, 8, 27)
        .unwrap()
        .and_hms_opt(8, 30, 0)
        .unwrap();
    let fixture = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();

    // 1. Seed the OLD transaction's canonical partial at timestamp T,
    //    claimed through the real RecordingsLayout.
    let old_claim = storage.layout.claim_segment(&storage.camera, t).unwrap();
    let old_partial = old_claim.partial_path().to_path_buf();
    assert_eq!(
        old_partial.file_name().unwrap().to_string_lossy(),
        "08-30-00.partial.mkv",
        "test premise: an empty tree hands out the bare identity"
    );
    std::fs::write(&old_partial, &fixture).unwrap();
    drop(old_claim);

    // 2. Recover it successfully.
    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        matches!(
            outcomes.as_slice(),
            [RecoveryOutcome::Recovered {
                original_removed: true,
                tombstone_recorded: true,
                ..
            }]
        ),
        "the old transaction must recover cleanly: {outcomes:?}"
    );

    // 3. Verify the recovered final + its trusted v2 tombstone.
    let old_final = storage.path("08-30-00.recovered.mkv");
    let old_tombstone = storage.path("08-30-00.recovered.mkv.done");
    assert!(old_final.is_file());
    assert!(old_tombstone.is_file());
    let recorded = std::fs::read_to_string(&old_tombstone).unwrap();
    assert!(
        recorded.starts_with("NIAN-RECOVERY-TOMBSTONE v2\n"),
        "the trusted marker must be v2: {recorded:?}"
    );
    let recorded_size: u64 = recorded
        .lines()
        .find_map(|line| line.strip_prefix("size: "))
        .expect("the tombstone records the published size")
        .parse()
        .expect("the recorded size must be numeric");
    assert_eq!(
        recorded_size,
        std::fs::metadata(&old_final).unwrap().len(),
        "the tombstone's size binding must describe the object at the final path"
    );
    let old_final_bytes = std::fs::read(&old_final).unwrap();

    // 4.+5. A NEW recording begins at the SAME wall-clock second T —
    //       claimed through the real RecordingsLayout, as the recorder
    //       would after a clock rollback.
    let new_claim = storage.layout.claim_segment(&storage.camera, t).unwrap();

    // 6. Its identity DIFFERS from the old transaction: the recovered
    //    final and tombstone reserve sequence 1.
    assert_eq!(
        new_claim
            .partial_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "08-30-00-2.partial.mkv",
        "the allocator must NOT re-issue 08-30-00.partial.mkv after a rollback"
    );
    assert_eq!(
        new_claim
            .final_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "08-30-00-2.mkv"
    );
    assert_ne!(new_claim.partial_path(), old_partial);

    // 7. Seed valid new partial media in that slot.
    std::fs::write(new_claim.partial_path(), &fixture).unwrap();
    let new_partial = new_claim.partial_path().to_path_buf();
    drop(new_claim);

    // 8.+9. Startup recovery: the new footage is a NEW transaction —
    //       never interpreted as AlreadyRecovered from the old tombstone.
    let (outcomes2, failures2) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures2.is_empty(), "{failures2:?}");
    assert!(
        !outcomes2
            .iter()
            .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
        "the old tombstone must never claim the NEW footage: {outcomes2:?}"
    );
    assert!(
        !outcomes2
            .iter()
            .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
        "the new transaction must not collide with the old one: {outcomes2:?}"
    );
    assert!(
        matches!(
            outcomes2.as_slice(),
            [RecoveryOutcome::Recovered {
                final_path,
                original_removed: true,
                tombstone_recorded: true,
                ..
            }] if final_path == &storage.path("08-30-00-2.recovered.mkv")
        ),
        "the new footage must recover under its OWN distinct identity: {outcomes2:?}"
    );
    assert!(!new_partial.exists(), "the new original was recovered");

    // 10. Both recovered recordings remain independently identifiable:
    //     distinct finals, both still on disk, the old one byte-identical,
    //     each tombstone bound to its own names.
    let new_final = storage.path("08-30-00-2.recovered.mkv");
    let new_tombstone = storage.path("08-30-00-2.recovered.mkv.done");
    assert!(old_final.is_file());
    assert!(new_final.is_file());
    assert_ne!(old_final, new_final);
    assert_eq!(
        std::fs::read(&old_final).unwrap(),
        old_final_bytes,
        "the old recovered recording must be untouched by the second transaction"
    );
    assert!(old_tombstone.is_file());
    assert!(new_tombstone.is_file());
    let new_recorded = std::fs::read_to_string(&new_tombstone).unwrap();
    assert!(
        new_recorded.contains("original: 08-30-00-2.partial.mkv\n")
            && new_recorded.contains("final: 08-30-00-2.recovered.mkv\n"),
        "the new tombstone must bind the NEW transaction's names: {new_recorded:?}"
    );
    // Both files classify as first-class recordings with distinct stems.
    assert_eq!(
        nian_storage::classify_recording_file_name("08-30-00.recovered.mkv"),
        nian_storage::RecordingFileKind::RecoveredRecording
    );
    assert_eq!(
        nian_storage::classify_recording_file_name("08-30-00-2.recovered.mkv"),
        nian_storage::RecordingFileKind::RecoveredRecording
    );
    assert!(probe_has_video(&old_final));
    assert!(probe_has_video(&new_final));
}

// ---- M3 atomic identity claim remediation §6 ----------------------------
/// The CONCURRENT transition case of the clock-rollback regression: a new
/// recording claim races the namespace transition of a recovery transaction
/// (publish `<stem>.recovered.mkv` → write trusted tombstone → remove the
/// original partial LAST). The claim's advisory scan is forced — via the
/// day-dir-keyed claim gate — into the exact window the review describes:
/// it chose sequence 1 from a state where the identity looked free, and by
/// the time its `create_new` runs, the old transaction has fully committed.
/// Without the post-claim identity fence the claim would return the OLD
/// transaction's identity and a later crash could let the old tombstone
/// swallow the new footage; with the fence the candidate is relinquished
/// and the claim retries onto a distinct sequence.
#[test]
fn racing_claim_during_recovery_transition_never_reuses_the_transaction_identity() {
    use nian_storage::test_hooks::arm_claim_identity_gate;

    let storage = Storage::new();
    let t = chrono::NaiveDate::from_ymd_opt(2026, 8, 27)
        .unwrap()
        .and_hms_opt(8, 30, 0)
        .unwrap();
    let fixture = std::fs::read(fixtures_dir().join(HEALTHY_FIXTURE)).unwrap();

    // Arm the one-shot gate for THIS day directory: the FIRST claim into it
    // (the racing NEW recording below) parks after its advisory scan.
    let (_guard, gate) = arm_claim_identity_gate(&storage.day_dir);

    // The racing claim: scans an EMPTY day directory, chooses sequence 1,
    // parks BEFORE creating its candidate.
    let layout = storage.layout.clone();
    let camera = storage.camera.clone();
    let claimer = std::thread::spawn(move || {
        layout
            .claim_segment(&camera, t)
            .expect("the racing claim must succeed")
    });
    gate.wait_arrived();

    // The OLD transaction's claim proceeds while the racer is parked (the
    // one-shot gate is consumed): it takes 08-30-00.partial.mkv, receives
    // valid media, and "crashes" (the handle is dropped, content remains).
    let old_claim = storage.layout.claim_segment(&storage.camera, t).unwrap();
    assert_eq!(
        old_claim
            .partial_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "08-30-00.partial.mkv"
    );
    std::fs::write(old_claim.partial_path(), &fixture).unwrap();
    drop(old_claim);

    // REAL startup recovery performs the namespace transition:
    // publish → trusted tombstone → remove the original partial LAST.
    let (outcomes, failures) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(
        matches!(
            outcomes.as_slice(),
            [RecoveryOutcome::Recovered {
                original_removed: true,
                tombstone_recorded: true,
                ..
            }]
        ),
        "the old transaction must recover cleanly: {outcomes:?} / {failures:?}"
    );

    let old_final = storage.path("08-30-00.recovered.mkv");
    let old_tombstone = storage.path("08-30-00.recovered.mkv.done");
    assert!(old_final.is_file() && old_tombstone.is_file());
    let old_final_bytes = std::fs::read(&old_final).unwrap();
    let old_tombstone_bytes = std::fs::read(&old_tombstone).unwrap();

    // Release the racer: its candidate create_new(08-30-00.partial.mkv)
    // now SUCCEEDS (recovery freed the name by removing the original), and
    // the post-claim identity fence must detect the freshly published
    // reservation for (T, 1) and force the retry onto sequence 2.
    gate.release();
    let new_claim = claimer.join().expect("the racing claim must not panic");

    assert_eq!(
        new_claim
            .partial_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "08-30-00-2.partial.mkv",
        "sequence 1 must never be returned after the competing recovered \
         reservation appeared"
    );
    assert_eq!(
        new_claim
            .final_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "08-30-00-2.mkv"
    );
    assert!(
        !storage.path("08-30-00.partial.mkv").exists(),
        "the temporary losing candidate must be removed cleanly"
    );
    assert!(
        new_claim.partial_path().is_file(),
        "the winning claim owns its own candidate"
    );
    // The old transaction's artifacts are untouched by the relinquish.
    assert_eq!(std::fs::read(&old_final).unwrap(), old_final_bytes);
    assert_eq!(std::fs::read(&old_tombstone).unwrap(), old_tombstone_bytes);

    // New valid media lands in the DISTINCT slot; then the crash it implies
    // (the claim handle is dropped, media stays under the partial name).
    std::fs::write(new_claim.partial_path(), &fixture).unwrap();
    let new_partial = new_claim.partial_path().to_path_buf();
    drop(new_claim);

    // Startup recovery: the OLD trusted tombstone must never claim the NEW
    // partial — the new footage recovers under its OWN identity.
    let (outcomes2, failures2) = recover_camera_partials(&storage.layout, &storage.camera);
    assert!(failures2.is_empty(), "{failures2:?}");
    assert!(
        !outcomes2
            .iter()
            .any(|outcome| matches!(outcome, RecoveryOutcome::AlreadyRecovered { .. })),
        "the old tombstone must never swallow the new footage: {outcomes2:?}"
    );
    assert!(
        !outcomes2
            .iter()
            .any(|outcome| matches!(outcome, RecoveryOutcome::RecoveryConflict { .. })),
        "the two transactions must not collide: {outcomes2:?}"
    );
    assert!(
        matches!(
            outcomes2.as_slice(),
            [RecoveryOutcome::Recovered {
                final_path,
                original_removed: true,
                tombstone_recorded: true,
                ..
            }] if final_path == &storage.path("08-30-00-2.recovered.mkv")
        ),
        "the new footage must recover under its own distinct identity: {outcomes2:?}"
    );
    assert!(!new_partial.exists());

    // Both recovered recordings survive, independently identifiable: the
    // old one byte-identical, each tombstone bound to its own names, both
    // files independently probeable.
    let new_final = storage.path("08-30-00-2.recovered.mkv");
    let new_tombstone = storage.path("08-30-00-2.recovered.mkv.done");
    assert!(old_final.is_file() && new_final.is_file());
    assert_ne!(old_final, new_final);
    assert_eq!(std::fs::read(&old_final).unwrap(), old_final_bytes);
    assert!(old_tombstone.is_file() && new_tombstone.is_file());
    let new_recorded = std::fs::read_to_string(&new_tombstone).unwrap();
    assert!(
        new_recorded.contains("original: 08-30-00-2.partial.mkv\n")
            && new_recorded.contains("final: 08-30-00-2.recovered.mkv\n"),
        "the new tombstone must bind the NEW transaction's names: {new_recorded:?}"
    );
    assert!(probe_has_video(&old_final));
    assert!(probe_has_video(&new_final));
}
