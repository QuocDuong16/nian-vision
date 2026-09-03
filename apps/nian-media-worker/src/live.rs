//! M11 per-process live-view job.
//!
//! One desktop live session owns one worker process, so this manager intentionally
//! supports only one live job. It copies H.264 packets into fragmented MP4 without
//! transcoding and retries transient source failures with bounded backoff.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nian_domain::MediaType;
use nian_media::{MediaSource, RtspUrl};
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
use serde::Serialize;

const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_SECONDS: [u64; 5] = [1, 2, 4, 8, 15];

pub mod code {
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const BUSY: &str = "live_busy";
    pub const WORKER_UNAVAILABLE: &str = "worker_unavailable";
}

pub struct LiveSpec {
    source_url: String,
    output_path: PathBuf,
}

impl LiveSpec {
    pub fn from_params(params: &serde_json::Value) -> Result<Self, &'static str> {
        let source = params.get("source").ok_or("missing source")?;
        if source.get("kind").and_then(serde_json::Value::as_str) != Some("rtsp") {
            return Err("live source must be rtsp");
        }
        let source_url = source
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing source.url")?;
        if !(source_url.starts_with("rtsp://") || source_url.starts_with("rtsps://")) {
            return Err("invalid rtsp source");
        }
        let output_path = params
            .get("output_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or("missing output_path")?;
        if !output_path.is_absolute() {
            return Err("output_path must be absolute");
        }
        Ok(Self {
            source_url: source_url.to_owned(),
            output_path,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveStatus {
    state: &'static str,
    failure_category: Option<&'static str>,
    reconnect_attempt: u32,
}

impl Default for LiveStatus {
    fn default() -> Self {
        Self {
            state: "starting",
            failure_category: None,
            reconnect_attempt: 0,
        }
    }
}

pub struct LiveJobManager {
    status: Arc<Mutex<LiveStatus>>,
    stop: Arc<AtomicBool>,
    interrupt: Arc<Mutex<Option<InterruptHandle>>>,
    handle: Option<JoinHandle<()>>,
}

impl LiveJobManager {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(LiveStatus::default())),
            stop: Arc::new(AtomicBool::new(false)),
            interrupt: Arc::new(Mutex::new(None)),
            handle: None,
        }
    }

    pub fn start(&mut self, spec: LiveSpec) -> Result<(), &'static str> {
        self.reap_finished();
        if self.handle.is_some() {
            return Err(code::BUSY);
        }

        let claim = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&spec.output_path)
            .map_err(|_| code::WORKER_UNAVAILABLE)?;
        drop(claim);
        self.stop.store(false, Ordering::Release);
        if let Ok(mut status) = self.status.lock() {
            *status = LiveStatus::default();
        }

        let status = self.status.clone();
        let stop = self.stop.clone();
        let interrupt = self.interrupt.clone();
        self.handle = Some(
            std::thread::Builder::new()
                .name("live-view-job".to_owned())
                .spawn(move || run_live(spec, status, stop, interrupt))
                .map_err(|_| code::WORKER_UNAVAILABLE)?,
        );
        Ok(())
    }

    pub fn status_json(&mut self) -> serde_json::Value {
        self.reap_finished();
        self.status
            .lock()
            .ok()
            .and_then(|status| serde_json::to_value(status.clone()).ok())
            .unwrap_or_else(|| {
                serde_json::json!({
                    "state": "failed",
                    "failure_category": "worker_unavailable",
                    "reconnect_attempt": 0,
                })
            })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(guard) = self.interrupt.lock()
            && let Some(interrupt) = guard.as_ref()
        {
            interrupt.cancel();
        }
        if let Ok(mut status) = self.status.lock()
            && status.state != "failed"
        {
            status.state = "stopping";
            status.failure_category = None;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn reap_finished(&mut self) {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for LiveJobManager {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_live(
    spec: LiveSpec,
    status: Arc<Mutex<LiveStatus>>,
    stop: Arc<AtomicBool>,
    current_interrupt: Arc<Mutex<Option<InterruptHandle>>>,
) {
    for attempt in 0..MAX_RECONNECT_ATTEMPTS {
        if stop.load(Ordering::Acquire) {
            mark_cancelled(&status);
            return;
        }
        set_status(&status, "connecting", None, attempt);

        // Reuse the same path identity so the desktop endpoint never becomes an
        // arbitrary file proxy. Frontend reloads the media element after backoff.
        if OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&spec.output_path)
            .is_err()
        {
            set_status(&status, "failed", Some("media_failed"), attempt);
            return;
        }

        let interrupt = InterruptHandle::new();
        if let Ok(mut slot) = current_interrupt.lock() {
            *slot = Some(interrupt.clone());
        }
        let source = MediaSource::Rtsp {
            url: RtspUrl::new(spec.source_url.clone()),
        };
        let mut input = match MediaInput::open(&source, &interrupt) {
            Ok(input) => input,
            Err(error) => {
                clear_interrupt(&current_interrupt);
                if stop.load(Ordering::Acquire) || error.is_interrupted() {
                    mark_cancelled(&status);
                    return;
                }
                if !retry_or_fail(&status, &stop, attempt, "source_open_failed") {
                    return;
                }
                continue;
            }
        };
        let streams = input.streams();
        let Some(video) = streams
            .iter()
            .find(|stream| stream.media_type == MediaType::Video)
            .cloned()
        else {
            clear_interrupt(&current_interrupt);
            set_status(&status, "failed", Some("unsupported_codec"), attempt);
            return;
        };
        if !video.codec_name.eq_ignore_ascii_case("h264") {
            clear_interrupt(&current_interrupt);
            set_status(&status, "failed", Some("unsupported_codec"), attempt);
            return;
        }
        let video_index = video.stream_index;
        let mut muxer = match MatroskaMuxer::create_fragmented_mp4_with_selection(
            &mut input,
            &spec.output_path,
            &interrupt,
            |stream| stream.stream_index == video_index,
        ) {
            Ok(muxer) => muxer,
            Err(_) => {
                clear_interrupt(&current_interrupt);
                set_status(&status, "failed", Some("media_failed"), attempt);
                return;
            }
        };
        set_status(&status, "live", None, attempt);

        loop {
            if stop.load(Ordering::Acquire) {
                interrupt.cancel();
            }
            let packet = {
                let _deadline = interrupt.scoped_deadline(READ_TIMEOUT);
                input.next_packet()
            };
            match packet {
                Ok(Some(packet)) => {
                    if muxer.write_packet(&packet).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) if stop.load(Ordering::Acquire) || error.is_interrupted() => {
                    clear_interrupt(&current_interrupt);
                    mark_cancelled(&status);
                    return;
                }
                Err(_) => break,
            }
        }
        let _ = muxer.finalize();
        clear_interrupt(&current_interrupt);
        if stop.load(Ordering::Acquire) {
            mark_cancelled(&status);
            return;
        }
        if !retry_or_fail(&status, &stop, attempt, "media_failed") {
            return;
        }
    }
}

fn retry_or_fail(
    status: &Arc<Mutex<LiveStatus>>,
    stop: &Arc<AtomicBool>,
    attempt: u32,
    failure: &'static str,
) -> bool {
    let next_attempt = attempt.saturating_add(1);
    if next_attempt >= MAX_RECONNECT_ATTEMPTS {
        set_status(status, "failed", Some(failure), next_attempt);
        return false;
    }
    set_status(status, "backoff", Some(failure), next_attempt);
    let delay = Duration::from_secs(BACKOFF_SECONDS[attempt as usize]);
    let mut elapsed = Duration::ZERO;
    while elapsed < delay {
        if stop.load(Ordering::Acquire) {
            mark_cancelled(status);
            return false;
        }
        let step = Duration::from_millis(100).min(delay - elapsed);
        std::thread::sleep(step);
        elapsed += step;
    }
    true
}

fn set_status(
    status: &Arc<Mutex<LiveStatus>>,
    state: &'static str,
    failure_category: Option<&'static str>,
    reconnect_attempt: u32,
) {
    if let Ok(mut status) = status.lock() {
        status.state = state;
        status.failure_category = failure_category;
        status.reconnect_attempt = reconnect_attempt;
    }
}

fn mark_cancelled(status: &Arc<Mutex<LiveStatus>>) {
    set_status(status, "failed", Some("lifecycle_cancelled"), 0);
}

fn clear_interrupt(current_interrupt: &Arc<Mutex<Option<InterruptHandle>>>) {
    if let Ok(mut slot) = current_interrupt.lock() {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn live_spec_accepts_only_rtsp_and_absolute_worker_owned_output() {
        let absolute = std::env::temp_dir().join("nian-live-test.mp4");
        let valid = LiveSpec::from_params(&serde_json::json!({
            "source": {"kind": "rtsp", "url": "rtsp://user:secret@127.0.0.1:9/stream1"},
            "output_path": absolute,
        }))
        .unwrap();
        assert!(valid.source_url.starts_with("rtsp://"));
        assert!(valid.output_path.is_absolute());

        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "file", "path": "/tmp/source.mkv"},
                "output_path": "/tmp/live.mp4",
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "http://camera/stream"},
                "output_path": "/tmp/live.mp4",
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "rtsp://camera/stream"},
                "output_path": "relative/live.mp4",
            }))
            .is_err()
        );
    }

    #[test]
    fn retry_budget_becomes_typed_failed_state_at_the_bound() {
        let status = Arc::new(Mutex::new(LiveStatus::default()));
        let stop = Arc::new(AtomicBool::new(false));

        assert!(!retry_or_fail(
            &status,
            &stop,
            MAX_RECONNECT_ATTEMPTS - 1,
            "source_open_failed",
        ));
        let status = status.lock().unwrap().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.failure_category, Some("source_open_failed"));
        assert_eq!(status.reconnect_attempt, MAX_RECONNECT_ATTEMPTS);
    }

    #[test]
    fn lifecycle_stop_interrupts_backoff_without_waiting_for_delay() {
        let status = Arc::new(Mutex::new(LiveStatus::default()));
        let stop = Arc::new(AtomicBool::new(true));
        let started = std::time::Instant::now();

        assert!(!retry_or_fail(&status, &stop, 0, "source_open_failed"));
        assert!(started.elapsed() < Duration::from_millis(250));
        let status = status.lock().unwrap().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.failure_category, Some("lifecycle_cancelled"));
    }
}
