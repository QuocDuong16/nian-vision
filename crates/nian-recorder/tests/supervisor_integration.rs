//! Real-pipeline supervision tests (M3): the scripted-factory unit tests
//! prove state-machine policy; this file runs the
//! `CameraRecordingSupervisor` over REAL FFmpeg recording sessions opened
//! through the production-shaped factory seam.
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nian_domain::CameraId;
use nian_media_ffmpeg::{InterruptHandle, MediaInput};
use nian_recorder::{
    AttemptOutcome, AudioPolicy, CameraRecordingSupervisor, FailureCategory, NoJitter,
    RecorderConfig, RecordingError, RecordingSession, SourceKind, SupervisorConfig, SupervisorEnd,
    SupervisorEvent, SupervisorState,
};
use nian_storage::RecordingsLayout;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/nian-media-ffmpeg/tests/fixtures")
}

/// Long multi-GOP fixture standing in for a live RTSP camera.
const SESSION_FIXTURE: &str = "session_av.mkv";

/// Production-shaped session factory: one fresh MediaInput + fresh
/// InterruptHandle + fresh RecordingSession per call — the exact structure
/// the worker layer wires for reconnects. A queued error makes the Nth open
/// attempt fail at CONNECT time; afterwards opens succeed.
struct RealSessionFactory {
    source: nian_media::MediaSource,
    camera: CameraId,
    layout: RecordingsLayout,
    /// Attempts that must fail at connect time with a retryable open error
    /// before real opens start (fresh error built per call — the error type
    /// is not Clone by design).
    failing_connects: usize,
    attempt_count: Arc<AtomicUsize>,
}

static WIRED_RUN_STOP: std::sync::Mutex<Option<nian_recorder::StopFlag>> =
    std::sync::Mutex::new(None);

#[allow(clippy::unwrap_used)] // poisoning cannot wedge a test-only flag slot
fn set_wired(flag: nian_recorder::StopFlag) {
    *WIRED_RUN_STOP.lock().unwrap() = Some(flag);
}

impl nian_recorder::supervisor::SessionFactory for RealSessionFactory {
    fn open_session(
        &mut self,
        run_stop: &nian_recorder::StopFlag,
    ) -> Result<Box<dyn nian_recorder::supervisor::ActiveSession>, RecordingError> {
        let attempt = self.attempt_count.fetch_add(1, Ordering::SeqCst);
        // §1 by-construction wiring: record the handed flag — requests made
        // through it are visible to the ACTIVE session by shared control.
        set_wired(run_stop.clone());
        if attempt < self.failing_connects {
            return Err(RecordingError::Media(nian_media::MediaError::OpenFailed {
                message: "connection refused".to_owned(),
            }));
        }

        let interrupt = InterruptHandle::new();
        let input = MediaInput::open(&self.source.clone(), &interrupt)?;
        let session = RecordingSession::from_input(input, self.layout.clone(), self.config())?;
        Ok(Box::new(RealSession { session }))
    }
}

impl RealSessionFactory {
    fn config(&self) -> RecorderConfig {
        RecorderConfig::new(self.camera.clone())
            .with_segment_target(std::time::Duration::from_secs(5))
            .with_audio(AudioPolicy::CopyAll)
    }
}

/// One supervised recording session over a real connection. The factory
/// drops its interrupt clone after construction — the session owns the
/// shared state via `input.interrupt_handle()` (M2 ownership invariant).
struct RealSession {
    session: RecordingSession,
}

impl nian_recorder::supervisor::ActiveSession for RealSession {
    fn run(
        self: Box<Self>,
        events: &mut dyn FnMut(nian_recorder::RecordingEvent),
    ) -> Result<nian_recorder::RecordingSummary, RecordingError> {
        self.session.run(events)
    }

    fn stop_flag(&self) -> nian_recorder::StopFlag {
        // Observability seam only; delivery happens through the session's
        // own flag which shares the supervisor's control domain via the
        // factory wiring above.
        self.session.stop_flag()
    }
}

#[test]
fn supervised_run_recovers_from_connect_failure_and_records_real_segments() {
    let temp = tempfile::tempdir().unwrap();
    let storage_root = temp.path().join("recordings");
    let layout = RecordingsLayout::new(storage_root.clone()).unwrap();
    let camera = CameraId::parse("cam-sup-live").unwrap();

    // Attempt 1: connect fails with a RETRYABLE open error (dead camera).
    // Attempt 2+: real recording of the 30 s fixture until clean EOF. For
    // RTSP semantics EOF means ConnectionLost → backoff → attempt 3 would
    // record again... The run is ended deterministically from inside the
    // event stream once the FIRST successful session finishes: its
    // SessionEnded event triggers the run-level stop flag, so backoff #2
    // exits without reconnecting (stop during Backoff, M3 §16).
    let factory = RealSessionFactory {
        source: nian_media::MediaSource::File(fixtures_dir().join(SESSION_FIXTURE)),
        camera: camera.clone(),
        layout: layout.clone(),
        failing_connects: 1,
        attempt_count: Arc::new(AtomicUsize::new(0)),
    };

    // Waits proceed instantly (CI speed); stop requests flow through the
    // supervisor's own flag requested below inside the event callback.
    struct InstantWaiter;
    impl nian_recorder::supervisor::Waiter for InstantWaiter {
        fn wait(&mut self, _delay: std::time::Duration, stop_requested: &dyn Fn() -> bool) -> bool {
            !stop_requested()
        }
    }

    let supervisor = CameraRecordingSupervisor::new(
        camera.clone(),
        SourceKind::Rtsp,
        SupervisorConfig {
            stable_recording_threshold: std::time::Duration::from_secs(30),
            jitter_half: std::time::Duration::ZERO,
        },
        factory,
        InstantWaiter,
        NoJitter,
    );
    let run_flag = supervisor.stop_flag();

    let mut events: Vec<SupervisorEvent> = Vec::new();
    let mut stopped_trigger_armed = true;
    let outcome = supervisor.run_until_end(&mut |event| {
        // Arm the shutdown when the good session reports its end.
        if matches!(event, SupervisorEvent::SessionEnded { .. }) && stopped_trigger_armed {
            // SessionEnded for the OPEN FAILURE also arrives first — only
            // arm after we saw the first ConnectionEstablished.
            if events
                .iter()
                .any(|e| matches!(e, SupervisorEvent::ConnectionEstablished { .. }))
            {
                stopped_trigger_armed = false;
                run_flag.request();
            }
        }
        events.push(event);
    });

    assert_eq!(
        outcome.end,
        SupervisorEnd::StoppedByOperator { clean: true }
    );

    // Exactly ONE real recording session ran to EOF (attempt 2), publishing
    // all 5-second segments of the 30 s fixture: ≥ 4 segments.
    assert!(
        outcome.finalized_segments >= 4,
        "the good session must publish the fixture's segments, got {}",
        outcome.finalized_segments
    );

    // State machine visited Connecting → (Backoff after dead camera) →
    // Recording → Backoff → Stopped, in one connected chain.
    let states: Vec<SupervisorState> = events
        .iter()
        .filter_map(|event| match event {
            SupervisorEvent::StateChanged { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            SupervisorState::Connecting,
            SupervisorState::Backoff,
            SupervisorState::Connecting,
            SupervisorState::Recording,
            SupervisorState::Backoff,
            SupervisorState::Stopped,
        ],
        "unexpected supervision shape: {events:?}"
    );

    // The failed connect was announced as a retryable typed failure.
    assert!(events.iter().any(|event| matches!(
        event,
        SupervisorEvent::SessionEnded {
            outcome: AttemptOutcome::Failure {
                category: FailureCategory::SourceOpenFailed,
            },
            ..
        }
    )));

    // Every published segment sits in the canonical tree; no partials
    // anywhere; final count == segments published across attempts.
    let finals = walk(&storage_root.join(camera.as_str()))
        .into_iter()
        .filter(|path| !is_partial(path))
        .count();
    assert_eq!(finals, outcome.finalized_segments);
    assert!(walk(&storage_root).iter().all(|path| !is_partial(path)));
}

fn is_partial(path: &Path) -> bool {
    path.to_string_lossy().contains(".partial.")
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
