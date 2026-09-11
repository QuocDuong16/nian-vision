//! M11 per-process live-view job.
//!
//! One desktop live session owns one worker process. H.264 video packets are copied
//! without decoding into short, independently finalized fragmented-MP4 chunks. The
//! application owns retention; the worker enforces a hard per-chunk size bound.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nian_domain::{MediaRational, MediaType};
use nian_media::{MediaSource, RtspUrl};
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
use serde::Serialize;

const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_SECONDS: [u64; 5] = [1, 2, 4, 8, 15];
const MIN_FRAGMENT_TARGET: Duration = Duration::from_millis(500);
const MAX_FRAGMENT_TARGET: Duration = Duration::from_secs(10);
const MIN_FRAGMENT_BYTES: u64 = 1024 * 1024;
const MAX_FRAGMENT_BYTES: u64 = 64 * 1024 * 1024;
const FRAGMENT_OVERHEAD_RESERVE: u64 = 512 * 1024;
const MAX_PACKET_BYTES: u64 = 4 * 1024 * 1024;
const MIN_FRAGMENT_COUNT: usize = 2;
const MAX_FRAGMENT_COUNT: usize = 32;
const FRAGMENT_DIGITS: usize = 12;

pub mod code {
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const BUSY: &str = "live_busy";
    pub const WORKER_UNAVAILABLE: &str = "worker_unavailable";
}

pub struct LiveSpec {
    source_url: String,
    output_dir: PathBuf,
    fragment_target: Duration,
    max_fragment_bytes: u64,
    max_fragment_count: usize,
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
        let output_dir = params
            .get("output_dir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or("missing output_dir")?;
        if !output_dir.is_absolute() {
            return Err("output_dir must be absolute");
        }
        let fragment_target_ms = params
            .get("fragment_target_ms")
            .and_then(serde_json::Value::as_u64)
            .ok_or("missing fragment_target_ms")?;
        let fragment_target = Duration::from_millis(fragment_target_ms);
        if !(MIN_FRAGMENT_TARGET..=MAX_FRAGMENT_TARGET).contains(&fragment_target) {
            return Err("fragment target is outside live bounds");
        }
        let max_fragment_bytes = params
            .get("max_fragment_bytes")
            .and_then(serde_json::Value::as_u64)
            .ok_or("missing max_fragment_bytes")?;
        if !(MIN_FRAGMENT_BYTES..=MAX_FRAGMENT_BYTES).contains(&max_fragment_bytes) {
            return Err("fragment byte limit is outside live bounds");
        }
        let max_fragment_count = params
            .get("max_fragment_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or("missing max_fragment_count")?;
        if !(MIN_FRAGMENT_COUNT..=MAX_FRAGMENT_COUNT).contains(&max_fragment_count) {
            return Err("fragment count limit is outside live bounds");
        }
        Ok(Self {
            source_url: source_url.to_owned(),
            output_dir,
            fragment_target,
            max_fragment_bytes,
            max_fragment_count,
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
        let metadata = std::fs::metadata(&spec.output_dir).map_err(|_| code::WORKER_UNAVAILABLE)?;
        if !metadata.is_dir() {
            return Err(code::WORKER_UNAVAILABLE);
        }
        cleanup_worker_partials(&spec.output_dir);
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

struct ActiveFragment {
    muxer: MatroskaMuxer,
    partial_path: PathBuf,
    final_path: PathBuf,
    started_at: Instant,
    start_timestamp: Option<i64>,
    payload_bytes: u64,
    packets: u64,
}

fn run_live(
    spec: LiveSpec,
    status: Arc<Mutex<LiveStatus>>,
    stop: Arc<AtomicBool>,
    current_interrupt: Arc<Mutex<Option<InterruptHandle>>>,
) {
    let mut next_fragment = 0_u64;
    for attempt in 0..MAX_RECONNECT_ATTEMPTS {
        if stop.load(Ordering::Acquire) {
            mark_cancelled(&status);
            return;
        }
        set_status(&status, "connecting", None, attempt);

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
        let Some(time_base) = video.time_base else {
            clear_interrupt(&current_interrupt);
            set_status(&status, "failed", Some("unsupported_codec"), attempt);
            return;
        };
        let video_index = video.stream_index;
        set_status(&status, "live", None, attempt);

        let mut active: Option<ActiveFragment> = None;
        let mut retry_failure = "media_read_failed";
        loop {
            if stop.load(Ordering::Acquire) {
                interrupt.cancel();
            }
            let packet = {
                let _deadline = interrupt.scoped_deadline(READ_TIMEOUT);
                input.next_packet()
            };
            let packet = match packet {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(error) if stop.load(Ordering::Acquire) || error.is_interrupted() => {
                    discard_fragment(active.take());
                    clear_interrupt(&current_interrupt);
                    mark_cancelled(&status);
                    return;
                }
                Err(_) => {
                    retry_failure = "media_read_failed";
                    break;
                }
            };
            let metadata = packet.metadata();
            if metadata.stream_index != video_index {
                continue;
            }
            if active.is_none() {
                if !metadata.keyframe {
                    continue;
                }
                match wait_for_fragment_capacity(&spec.output_dir, spec.max_fragment_count, &stop) {
                    Ok(true) => {}
                    Ok(false) => {
                        clear_interrupt(&current_interrupt);
                        mark_cancelled(&status);
                        return;
                    }
                    Err(()) => {
                        retry_failure = "media_fragment_capacity_failed";
                        break;
                    }
                }
                match create_fragment(
                    &mut input,
                    &spec,
                    &interrupt,
                    video_index,
                    next_fragment,
                    metadata.dts.or(metadata.pts),
                ) {
                    Ok(fragment) => active = Some(fragment),
                    Err(_) => {
                        retry_failure = "media_fragment_create_failed";
                        break;
                    }
                }
            }

            let packet_bytes = packet.data().len() as u64;
            if packet_bytes > MAX_PACKET_BYTES {
                retry_failure = "media_packet_too_large";
                break;
            }
            let should_rotate = active.as_ref().is_some_and(|fragment| {
                fragment.packets > 0
                    && metadata.keyframe
                    && (fragment_target_reached(
                        fragment,
                        metadata.dts.or(metadata.pts),
                        time_base,
                        spec.fragment_target,
                    ) || fragment
                        .payload_bytes
                        .saturating_add(packet_bytes)
                        .saturating_add(FRAGMENT_OVERHEAD_RESERVE)
                        > spec.max_fragment_bytes)
            });
            if should_rotate {
                let Some(fragment) = active.take() else {
                    retry_failure = "media_fragment_finalize_failed";
                    break;
                };
                if finish_fragment(fragment, spec.max_fragment_bytes).is_err() {
                    retry_failure = "media_fragment_finalize_failed";
                    break;
                }
                next_fragment = next_fragment.saturating_add(1);
                match wait_for_fragment_capacity(&spec.output_dir, spec.max_fragment_count, &stop) {
                    Ok(true) => {}
                    Ok(false) => {
                        clear_interrupt(&current_interrupt);
                        mark_cancelled(&status);
                        return;
                    }
                    Err(()) => {
                        retry_failure = "media_fragment_capacity_failed";
                        break;
                    }
                }
                match create_fragment(
                    &mut input,
                    &spec,
                    &interrupt,
                    video_index,
                    next_fragment,
                    metadata.dts.or(metadata.pts),
                ) {
                    Ok(fragment) => active = Some(fragment),
                    Err(_) => {
                        retry_failure = "media_fragment_create_failed";
                        break;
                    }
                }
            }

            let Some(fragment) = active.as_mut() else {
                continue;
            };
            if fragment
                .payload_bytes
                .saturating_add(packet_bytes)
                .saturating_add(FRAGMENT_OVERHEAD_RESERVE)
                > spec.max_fragment_bytes
            {
                retry_failure = "media_fragment_limit_exceeded";
                break;
            }
            if fragment.muxer.write_packet(&packet).is_err() {
                retry_failure = "media_mux_write_failed";
                break;
            }
            fragment.payload_bytes = fragment.payload_bytes.saturating_add(packet_bytes);
            fragment.packets = fragment.packets.saturating_add(1);
            if std::fs::metadata(&fragment.partial_path)
                .map(|metadata| metadata.len() > spec.max_fragment_bytes)
                .unwrap_or(false)
            {
                retry_failure = "media_fragment_limit_exceeded";
                break;
            }
        }

        if let Some(fragment) = active.take() {
            if stop.load(Ordering::Acquire) {
                discard_fragment(Some(fragment));
            } else if fragment.packets > 0
                && finish_fragment(fragment, spec.max_fragment_bytes).is_ok()
            {
                next_fragment = next_fragment.saturating_add(1);
            } else {
                retry_failure = "media_fragment_finalize_failed";
            }
        }
        clear_interrupt(&current_interrupt);
        if stop.load(Ordering::Acquire) {
            mark_cancelled(&status);
            return;
        }
        if !retry_or_fail(&status, &stop, attempt, retry_failure) {
            return;
        }
    }
}

fn create_fragment(
    input: &mut MediaInput,
    spec: &LiveSpec,
    interrupt: &InterruptHandle,
    video_index: u32,
    sequence: u64,
    start_timestamp: Option<i64>,
) -> Result<ActiveFragment, ()> {
    let base = format!("fragment-{sequence:0width$}", width = FRAGMENT_DIGITS);
    let partial_path = spec.output_dir.join(format!("{base}.partial.mp4"));
    let final_path = spec.output_dir.join(format!("{base}.mp4"));
    if final_path.exists() {
        return Err(());
    }
    let claim = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial_path)
        .map_err(|_| ())?;
    drop(claim);
    let muxer = MatroskaMuxer::create_live_fragmented_mp4_with_selection(
        input,
        &partial_path,
        interrupt,
        |stream| stream.stream_index == video_index,
    )
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&partial_path);
    })
    .map_err(|_| ())?;
    Ok(ActiveFragment {
        muxer,
        partial_path,
        final_path,
        started_at: Instant::now(),
        start_timestamp,
        payload_bytes: 0,
        packets: 0,
    })
}

fn finish_fragment(fragment: ActiveFragment, max_fragment_bytes: u64) -> Result<(), ()> {
    let partial_path = fragment.partial_path.clone();
    let final_path = fragment.final_path.clone();
    finish_owned_partial(&partial_path, &final_path, max_fragment_bytes, || {
        fragment.muxer.finalize().map(|_| ()).map_err(|_| ())
    })
}

fn finish_owned_partial(
    partial_path: &Path,
    final_path: &Path,
    max_fragment_bytes: u64,
    finalize: impl FnOnce() -> Result<(), ()>,
) -> Result<(), ()> {
    let result = (|| {
        finalize()?;
        let bytes = std::fs::metadata(partial_path).map_err(|_| ())?.len();
        if bytes == 0 || bytes > max_fragment_bytes || final_path.exists() {
            return Err(());
        }
        std::fs::rename(partial_path, final_path).map_err(|_| ())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(partial_path);
    }
    result
}

fn discard_fragment(fragment: Option<ActiveFragment>) {
    if let Some(fragment) = fragment {
        let path = fragment.partial_path.clone();
        drop(fragment);
        let _ = std::fs::remove_file(path);
    }
}

fn fragment_target_reached(
    fragment: &ActiveFragment,
    current_timestamp: Option<i64>,
    time_base: MediaRational,
    target: Duration,
) -> bool {
    if let (Some(start), Some(current)) = (fragment.start_timestamp, current_timestamp)
        && current > start
        && time_base.num > 0
        && time_base.den > 0
    {
        let ticks = i128::from(current - start);
        let nanos = ticks
            .saturating_mul(i128::from(time_base.num))
            .saturating_mul(1_000_000_000)
            / i128::from(time_base.den);
        if nanos >= target.as_nanos() as i128 {
            return true;
        }
    }
    fragment.started_at.elapsed() >= target
}

fn finalized_fragment_count(output_dir: &Path) -> Result<usize, ()> {
    let entries = std::fs::read_dir(output_dir).map_err(|_| ())?;
    let mut count = 0_usize;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let valid = name
            .strip_prefix("fragment-")
            .and_then(|name| name.strip_suffix(".mp4"))
            .is_some_and(|sequence| {
                sequence.len() == FRAGMENT_DIGITS
                    && sequence.bytes().all(|byte| byte.is_ascii_digit())
            });
        if !valid {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && !file_type.is_symlink() {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn wait_for_fragment_capacity(
    output_dir: &Path,
    max_fragment_count: usize,
    stop: &AtomicBool,
) -> Result<bool, ()> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        if finalized_fragment_count(output_dir)? < max_fragment_count {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn cleanup_worker_partials(output_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(output_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let valid = name
            .strip_prefix("fragment-")
            .and_then(|name| name.strip_suffix(".partial.mp4"))
            .is_some_and(|sequence| {
                sequence.len() == FRAGMENT_DIGITS
                    && sequence.bytes().all(|byte| byte.is_ascii_digit())
            });
        if !valid {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && !file_type.is_symlink() {
            let _ = std::fs::remove_file(entry.path());
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn live_spec_accepts_only_rtsp_absolute_directory_and_bounded_fragment_policy() {
        let absolute = std::env::temp_dir();
        let absolute_live = absolute.join("nian-live-test");
        let absolute_source = absolute.join("nian-live-source.mkv");
        let valid = LiveSpec::from_params(&serde_json::json!({
            "source": {"kind": "rtsp", "url": "rtsp://user:secret@127.0.0.1:9/stream1"},
            "output_dir": absolute.clone(),
            "fragment_target_ms": 2_000,
            "max_fragment_bytes": 16 * 1024 * 1024_u64,
            "max_fragment_count": 8,
        }))
        .unwrap();
        assert!(valid.source_url.starts_with("rtsp://"));
        assert!(valid.output_dir.is_absolute());
        assert_eq!(valid.fragment_target, Duration::from_secs(2));
        assert_eq!(valid.max_fragment_count, 8);

        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "file", "path": absolute_source},
                "output_dir": absolute_live.clone(),
                "fragment_target_ms": 2_000,
                "max_fragment_bytes": 16 * 1024 * 1024_u64,
                "max_fragment_count": 8,
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "http://camera/stream"},
                "output_dir": absolute_live.clone(),
                "fragment_target_ms": 2_000,
                "max_fragment_bytes": 16 * 1024 * 1024_u64,
                "max_fragment_count": 8,
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "rtsp://camera/stream"},
                "output_dir": "relative/live",
                "fragment_target_ms": 2_000,
                "max_fragment_bytes": 16 * 1024 * 1024_u64,
                "max_fragment_count": 8,
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "rtsp://camera/stream"},
                "output_dir": absolute_live.clone(),
                "fragment_target_ms": 50,
                "max_fragment_bytes": 16 * 1024 * 1024_u64,
                "max_fragment_count": 8,
            }))
            .is_err()
        );
        assert!(
            LiveSpec::from_params(&serde_json::json!({
                "source": {"kind": "rtsp", "url": "rtsp://camera/stream"},
                "output_dir": absolute_live,
                "fragment_target_ms": 2_000,
                "max_fragment_bytes": 16 * 1024 * 1024_u64,
                "max_fragment_count": MAX_FRAGMENT_COUNT + 1,
            }))
            .is_err()
        );
    }

    #[test]
    fn fragment_names_are_fixed_width_and_worker_cleanup_ignores_unowned_files() {
        let temp = tempfile::tempdir().unwrap();
        let owned = temp.path().join("fragment-000000000007.partial.mp4");
        let foreign = temp.path().join("notes.txt");
        std::fs::write(&owned, b"partial").unwrap();
        std::fs::write(&foreign, b"keep").unwrap();
        cleanup_worker_partials(temp.path());
        assert!(!owned.exists());
        assert!(foreign.exists());
    }

    #[test]
    fn finalized_fragment_capacity_counts_only_owned_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("fragment-000000000001.mp4"), b"one").unwrap();
        std::fs::write(
            temp.path().join("fragment-000000000002.partial.mp4"),
            b"partial",
        )
        .unwrap();
        std::fs::write(temp.path().join("notes.mp4"), b"foreign").unwrap();
        assert_eq!(finalized_fragment_count(temp.path()).unwrap(), 1);
    }

    #[test]
    fn fragment_capacity_wait_is_cancellable_at_the_hard_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        for sequence in 0..2 {
            std::fs::write(
                temp.path().join(format!("fragment-{sequence:012}.mp4")),
                b"fragment",
            )
            .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let path = temp.path().to_path_buf();
        let waiter =
            std::thread::spawn(move || wait_for_fragment_capacity(&path, 2, &stop_for_thread));
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Release);
        assert_eq!(waiter.join().unwrap(), Ok(false));
    }

    #[test]
    fn failed_fragment_finalization_always_removes_owned_partial() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("fragment-000000000001.partial.mp4");
        let final_path = temp.path().join("fragment-000000000001.mp4");

        std::fs::write(&partial, b"partial").unwrap();
        assert!(finish_owned_partial(&partial, &final_path, 1024, || Err(())).is_err());
        assert!(!partial.exists());
        assert!(!final_path.exists());

        std::fs::write(&partial, vec![1_u8; 2048]).unwrap();
        assert!(finish_owned_partial(&partial, &final_path, 1024, || Ok(())).is_err());
        assert!(!partial.exists());
        assert!(!final_path.exists());

        std::fs::write(&partial, b"new").unwrap();
        std::fs::write(&final_path, b"existing-final").unwrap();
        assert!(finish_owned_partial(&partial, &final_path, 1024, || Ok(())).is_err());
        assert!(!partial.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing-final");
    }

    #[test]
    fn successful_fragment_finalization_preserves_final_and_removes_partial_name() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("fragment-000000000002.partial.mp4");
        let final_path = temp.path().join("fragment-000000000002.mp4");
        std::fs::write(&partial, b"complete-fragment").unwrap();

        finish_owned_partial(&partial, &final_path, 1024, || Ok(())).unwrap();

        assert!(!partial.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete-fragment");
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
        let started = Instant::now();

        assert!(!retry_or_fail(&status, &stop, 0, "source_open_failed"));
        assert!(started.elapsed() < Duration::from_millis(250));
        let status = status.lock().unwrap().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.failure_category, Some("lifecycle_cancelled"));
    }
}
