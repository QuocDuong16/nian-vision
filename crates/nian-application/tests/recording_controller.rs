// Integration tests use panicking assertions/setup helpers deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use nian_application::{
    DesiredRecording, RecordingController, RecordingControllerError, RecordingRunFailure,
    RecordingRunner, RecordingState, RecordingThreadSpawner, WorkerEnd,
};
use nian_domain::CameraId;

struct HappyRunner;
impl RecordingRunner for HappyRunner {
    fn run(
        &self,
        _desired: DesiredRecording,
        stop: Arc<AtomicBool>,
        observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure> {
        observer(
            &serde_json::json!({"state":"recovering","camera_id":"cam-a","retry_attempt":0,"finalized_segments":0,"failure_category":""}),
        );
        observer(
            &serde_json::json!({"state":"connecting","camera_id":"cam-a","retry_attempt":0,"finalized_segments":0,"failure_category":""}),
        );
        observer(
            &serde_json::json!({"state":"recording","camera_id":"cam-a","retry_attempt":0,"finalized_segments":1,"failure_category":""}),
        );
        while !stop.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        Ok(WorkerEnd::RequestedShutdown)
    }
}

struct FailingRunner;
impl RecordingRunner for FailingRunner {
    fn run(
        &self,
        _desired: DesiredRecording,
        _stop: Arc<AtomicBool>,
        _observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure> {
        Err(RecordingRunFailure::Permanent {
            failure_category: Some("storage_failed".into()),
        })
    }
}

struct TerminalBeforeReturnRunner {
    first_terminal: &'static str,
    calls: AtomicUsize,
    entered: Arc<Barrier>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl RecordingRunner for TerminalBeforeReturnRunner {
    fn run(
        &self,
        desired: DesiredRecording,
        stop: Arc<AtomicBool>,
        observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            observer(&serde_json::json!({
                "state": self.first_terminal,
                "camera_id": desired.camera,
                "failure_category": "storage_failed"
            }));
            self.entered.wait();
            let (lock, condvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
            if self.first_terminal == "failed" {
                Err(RecordingRunFailure::Permanent {
                    failure_category: Some("storage_failed".to_owned()),
                })
            } else {
                Ok(WorkerEnd::JobCompletedCleanly)
            }
        } else {
            observer(&serde_json::json!({
                "state": "recording",
                "camera_id": desired.camera,
                "retry_attempt": 0,
                "finalized_segments": 2
            }));
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(WorkerEnd::RequestedShutdown)
        }
    }
}

struct PanicThenHappyRunner {
    calls: AtomicUsize,
}

impl RecordingRunner for PanicThenHappyRunner {
    fn run(
        &self,
        desired: DesiredRecording,
        stop: Arc<AtomicBool>,
        observer: Arc<dyn Fn(&serde_json::Value) + Send + Sync>,
    ) -> Result<WorkerEnd, RecordingRunFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected runner panic");
        }
        observer(&serde_json::json!({
            "state": "recording",
            "camera_id": desired.camera,
            "retry_attempt": 0,
            "finalized_segments": 1
        }));
        while !stop.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        Ok(WorkerEnd::RequestedShutdown)
    }
}

struct FailFirstSpawner {
    calls: AtomicUsize,
}

impl RecordingThreadSpawner for FailFirstSpawner {
    fn spawn(
        &self,
        name: String,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(std::io::Error::other("injected thread spawn failure"))
        } else {
            std::thread::Builder::new().name(name).spawn(task)
        }
    }
}

fn desired(camera: &str) -> DesiredRecording {
    DesiredRecording {
        camera: camera.to_owned(),
        storage_root: "/tmp/nian-controller-test".to_owned(),
        source_json: serde_json::json!({"kind":"rtsp","url":"rtsp://admin:SENTINEL@cam.local/stream"}),
        segment_target_secs: 300,
        copy_audio: true,
    }
}

fn wait_for(controller: &mut RecordingController, camera: &str, state: RecordingState) {
    let camera_id = CameraId::parse(camera).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if controller.status(&camera_id).unwrap().state == state {
            return;
        }
        std::thread::yield_now();
    }
    panic!(
        "state did not reach {state:?}; current={:?}",
        controller.status(&camera_id).unwrap()
    );
}

#[test]
fn simultaneous_cameras_start_and_stopping_one_preserves_the_other() {
    let mut controller = RecordingController::new(Arc::new(HappyRunner));
    let a = CameraId::parse("cam-a").unwrap();
    let b = CameraId::parse("cam-b").unwrap();
    controller.start(a.clone(), desired("cam-a")).unwrap();
    controller.start(b.clone(), desired("cam-b")).unwrap();
    wait_for(&mut controller, "cam-a", RecordingState::Recording);
    wait_for(&mut controller, "cam-b", RecordingState::Recording);
    assert_eq!(controller.status(&a).unwrap().finalized_segments, 1);
    assert_eq!(controller.status(&b).unwrap().finalized_segments, 1);

    let stopping = controller.stop(&a).unwrap();
    assert_eq!(stopping.state, RecordingState::Stopping);
    wait_for(&mut controller, "cam-a", RecordingState::Stopped);
    assert!(controller.is_owned(&b).unwrap());
    controller.stop(&b).unwrap();
    wait_for(&mut controller, "cam-b", RecordingState::Stopped);
}

#[test]
fn permanent_runner_failure_becomes_failed_without_infinite_restart() {
    let mut controller = RecordingController::new(Arc::new(FailingRunner));
    controller
        .start(CameraId::parse("cam-a").unwrap(), desired("cam-a"))
        .unwrap();
    wait_for(&mut controller, "cam-a", RecordingState::Failed);
    let status = controller
        .status(&CameraId::parse("cam-a").unwrap())
        .unwrap();
    assert_eq!(status.failure_category.as_deref(), Some("storage_failed"));
    assert_eq!(controller.active_camera().unwrap(), None);
}

fn assert_terminal_before_return_blocks_same_camera_restart(first_terminal: &'static str) {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runner = Arc::new(TerminalBeforeReturnRunner {
        first_terminal,
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let mut controller = RecordingController::new(runner);
    let a = CameraId::parse("cam-a").unwrap();

    controller.start(a.clone(), desired("cam-a")).unwrap();
    entered.wait();
    assert_ne!(controller.status(&a).unwrap().state, RecordingState::Failed);
    assert_ne!(
        controller.status(&a).unwrap().state,
        RecordingState::Stopped
    );
    assert_eq!(
        controller.start(a.clone(), desired("cam-a")).unwrap_err(),
        RecordingControllerError::AlreadyRecording
    );

    let (lock, condvar) = &*release;
    *lock.lock().unwrap() = true;
    condvar.notify_all();
    let expected_terminal = if first_terminal == "failed" {
        RecordingState::Failed
    } else {
        RecordingState::Stopped
    };
    wait_for(&mut controller, "cam-a", expected_terminal);
    controller.start(a.clone(), desired("cam-a")).unwrap();
    wait_for(&mut controller, "cam-a", RecordingState::Recording);
    controller.stop(&a).unwrap();
    wait_for(&mut controller, "cam-a", RecordingState::Stopped);
}

#[test]
fn failed_terminal_observation_before_runner_return_cannot_admit_second_run() {
    assert_terminal_before_return_blocks_same_camera_restart("failed");
}

#[test]
fn completed_terminal_observation_before_runner_return_cannot_admit_second_run() {
    assert_terminal_before_return_blocks_same_camera_restart("completed");
}

#[test]
fn runner_panic_becomes_failed_and_controller_remains_reusable() {
    let runner = Arc::new(PanicThenHappyRunner {
        calls: AtomicUsize::new(0),
    });
    let mut controller = RecordingController::new(runner);
    controller
        .start(CameraId::parse("cam-a").unwrap(), desired("cam-a"))
        .unwrap();

    wait_for(&mut controller, "cam-a", RecordingState::Failed);
    let failed = controller
        .status(&CameraId::parse("cam-a").unwrap())
        .unwrap();
    assert_eq!(
        failed.failure_category.as_deref(),
        Some("worker_unavailable")
    );
    assert_eq!(failed.camera_id.as_deref(), Some("cam-a"));

    controller
        .start(CameraId::parse("cam-b").unwrap(), desired("cam-b"))
        .unwrap();
    wait_for(&mut controller, "cam-b", RecordingState::Recording);
    assert_eq!(
        controller
            .status(&CameraId::parse("cam-b").unwrap())
            .unwrap()
            .camera_id
            .as_deref(),
        Some("cam-b")
    );
    controller.stop(&CameraId::parse("cam-b").unwrap()).unwrap();
    wait_for(&mut controller, "cam-b", RecordingState::Stopped);
}

#[test]
fn thread_spawn_failure_rolls_back_ownership_and_next_start_is_usable() {
    let spawner = Arc::new(FailFirstSpawner {
        calls: AtomicUsize::new(0),
    });
    let mut controller = RecordingController::with_spawner(Arc::new(HappyRunner), spawner);

    let error = controller
        .start(CameraId::parse("cam-a").unwrap(), desired("cam-a"))
        .unwrap_err();
    assert_eq!(error, RecordingControllerError::ThreadStart);

    let failed = controller
        .status(&CameraId::parse("cam-a").unwrap())
        .unwrap();
    assert_eq!(failed.state, RecordingState::Failed);
    assert_eq!(failed.camera_id.as_deref(), Some("cam-a"));
    assert_eq!(
        failed.failure_category.as_deref(),
        Some("worker_unavailable")
    );
    assert_eq!(controller.active_camera().unwrap(), None);

    controller
        .start(CameraId::parse("cam-b").unwrap(), desired("cam-b"))
        .unwrap();
    wait_for(&mut controller, "cam-b", RecordingState::Recording);
    assert_eq!(
        controller.active_camera().unwrap().unwrap().as_str(),
        "cam-b"
    );
    controller.stop(&CameraId::parse("cam-b").unwrap()).unwrap();
    wait_for(&mut controller, "cam-b", RecordingState::Stopped);
}

#[test]
fn desired_recording_debug_never_exposes_source_secret() {
    let desired = desired("cam-a");
    let rendered = format!("{desired:?}");
    assert!(!rendered.contains("SENTINEL"));
    assert!(!rendered.contains("rtsp://"));
}
