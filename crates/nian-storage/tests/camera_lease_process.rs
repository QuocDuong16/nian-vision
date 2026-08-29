#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::io::{BufRead as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use nian_domain::CameraId;
use nian_storage::{CameraLease, RecordingsLayout, StorageError};

const CHILD_ENV: &str = "NIAN_CAMERA_LEASE_TEST_CHILD";
const ROOT_ENV: &str = "NIAN_CAMERA_LEASE_TEST_ROOT";
const CAMERA: &str = "lease-process-cam";

#[test]
#[ignore = "spawned explicitly by holder_process_death_releases_camera_lease"]
fn child_process_holds_camera_lease() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    let root = std::path::PathBuf::from(std::env::var_os(ROOT_ENV).expect("child root"));
    let layout = RecordingsLayout::new(root).expect("child layout");
    let camera = CameraId::parse(CAMERA).expect("child camera");
    let _lease = CameraLease::try_acquire(&layout, &camera).expect("child lease");

    println!("LEASE_READY");
    std::io::stdout().flush().expect("flush ready");

    // Keep the open File (and therefore the OS lock) alive until the parent
    // terminates this process. No heartbeat, PID ownership, or lock-file
    // deletion participates in correctness.
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

#[test]
fn holder_process_death_releases_camera_lease() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("recordings");
    let layout = RecordingsLayout::new(root.clone()).unwrap();
    let camera = CameraId::parse(CAMERA).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("child_process_holds_camera_lease")
        .arg("--ignored")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(ROOT_ENV, &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let ready = lines.any(|line| line.is_ok_and(|line| line.contains("LEASE_READY")));
        let _ = ready_tx.send(ready);
    });

    assert!(
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "child never signaled that the camera lease was held"
    );

    let blocked = CameraLease::try_acquire(&layout, &camera).unwrap_err();
    assert!(matches!(blocked, StorageError::CameraAlreadyActive { .. }));

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "the holder should have been terminated");
    reader.join().unwrap();

    // Waiting for process death is the synchronization point: after the OS
    // closes the holder's file descriptor, no stale-file cleanup is needed.
    CameraLease::try_acquire(&layout, &camera).unwrap();
}
