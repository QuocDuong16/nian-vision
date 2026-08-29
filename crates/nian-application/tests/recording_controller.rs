// Integration tests use panicking assertions/setup helpers deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nian_application::{
    DesiredRecording, RecordingController, RecordingControllerError, RecordingRunFailure,
    RecordingRunner, RecordingState, WorkerEnd,
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
            std::thread::sleep(Duration::from_millis(5));
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

fn desired(camera: &str) -> DesiredRecording {
    DesiredRecording {
        camera: camera.to_owned(),
        storage_root: "/tmp/nian-controller-test".to_owned(),
        source_json: serde_json::json!({"kind":"rtsp","url":"rtsp://admin:SENTINEL@cam.local/stream"}),
        segment_target_secs: 300,
        copy_audio: true,
    }
}

fn wait_for(controller: &RecordingController, state: RecordingState) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if controller.status().unwrap().state == state {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "state did not reach {state:?}; current={:?}",
        controller.status().unwrap()
    );
}

#[test]
fn start_recording_stop_transitions_and_second_camera_is_rejected() {
    let mut controller = RecordingController::new(Arc::new(HappyRunner));
    controller
        .start(CameraId::parse("cam-a").unwrap(), desired("cam-a"))
        .unwrap();
    wait_for(&controller, RecordingState::Recording);
    let status = controller.status().unwrap();
    assert_eq!(status.finalized_segments, 1);

    let error = controller
        .start(CameraId::parse("cam-b").unwrap(), desired("cam-b"))
        .unwrap_err();
    assert_eq!(error, RecordingControllerError::AlreadyRecording);

    let stopping = controller.stop().unwrap();
    assert_eq!(stopping.state, RecordingState::Stopping);
    wait_for(&controller, RecordingState::Stopped);
    assert_eq!(controller.active_camera().unwrap(), None);
}

#[test]
fn permanent_runner_failure_becomes_failed_without_infinite_restart() {
    let mut controller = RecordingController::new(Arc::new(FailingRunner));
    controller
        .start(CameraId::parse("cam-a").unwrap(), desired("cam-a"))
        .unwrap();
    wait_for(&controller, RecordingState::Failed);
    let status = controller.status().unwrap();
    assert_eq!(status.failure_category.as_deref(), Some("storage_failed"));
    assert_eq!(controller.active_camera().unwrap(), None);
}

#[test]
fn desired_recording_debug_never_exposes_source_secret() {
    let desired = desired("cam-a");
    let rendered = format!("{desired:?}");
    assert!(!rendered.contains("SENTINEL"));
    assert!(!rendered.contains("rtsp://"));
}
