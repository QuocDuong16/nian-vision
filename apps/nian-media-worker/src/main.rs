//! nian-media-worker process entry point.
//!
//! Subcommands:
//!
//! ```text
//! nian-media-worker probe <path|credential-free-rtsp-url>
//! nian-media-worker probe --rtsp-from-env     # URL from NIAN_VISION_RTSP_URL
//! nian-media-worker run                       # NDJSON IPC loop on stdio
//! nian-media-worker record --storage <DIR> --camera <ID> [options]
//! ```
//!
//! Secrets policy (master spec §4/§7): RTSP URLs containing credentials are
//! rejected as command-line arguments — process listings are world-readable.
//! Use `--rtsp-from-env` with `NIAN_VISION_RTSP_URL` instead. Logs go to
//! stderr; stdout is reserved for the IPC protocol (and `probe` output,
//! which is the CLI product). Recorded file paths are safe to print; source
//! URLs never are, and no code path prints them.

#![forbid(unsafe_code)]
// As a CLI, stdout (probe output, usage) and stderr (diagnostics) are this
// binary's product channels; the IPC `run` mode keeps stdout protocol-clean
// by writing logs through `tracing` instead.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod job;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use nian_ipc::message::{Envelope, event, method};
use nian_ipc::{FramedWriter, serve};
use nian_media::{MediaSource, Probe, RtspUrl};
use nian_media_ffmpeg::{
    FfmpegBackend, InterruptHandle, MatroskaMuxer, MediaInput, RuntimeVersions,
};
use serde_json::json;

const USAGE: &str = "usage: nian-media-worker probe <path|credential-free-rtsp-url>
       nian-media-worker probe --rtsp-from-env
       nian-media-worker run
       nian-media-worker record --storage <DIR> --camera <ID>
                                [--segment-target <SECONDS>] [--no-audio]
                                [--duration <SECONDS> | --until-stdin-eof]
                                (--rtsp-from-env | <path|credential-free-rtsp-url>)

record stop modes (mutually exclusive, at most one):
  neither flag        Ctrl+C only (--duration/--until-stdin-eof omitted)
  --duration N        stop automatically after N seconds, or Ctrl+C
  --until-stdin-eof   stop when stdin reaches EOF, or Ctrl+C";

/// Environment variable holding a credential-bearing RTSP URL for manual
/// smoke tests; never committed, never logged.
const RTSP_URL_ENV: &str = "NIAN_VISION_RTSP_URL";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => match cmd_probe(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("probe failed: {message}");
                ExitCode::FAILURE
            }
        },
        Some("run") if args.len() == 1 => match cmd_run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("worker run failed: {error}");
                ExitCode::FAILURE
            }
        },
        Some("record") if args.len() > 1 => match cmd_record(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("record failed: {error}");
                ExitCode::FAILURE
            }
        },
        Some("--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn cmd_probe(args: &[String]) -> Result<(), String> {
    let source = match args {
        [flag] if flag == "--rtsp-from-env" => {
            let url = std::env::var(RTSP_URL_ENV).map_err(|_| {
                format!("{RTSP_URL_ENV} is not set; it must hold the full rtsp:// URL")
            })?;
            MediaSource::Rtsp {
                url: RtspUrl::new(url),
            }
        }
        [source_argument] => resolve_source(source_argument)?,
        _ => return Err("probe takes exactly one source argument".to_owned()),
    };

    let backend = FfmpegBackend::new().map_err(|error| error.to_string())?;
    let report = backend.probe(&source).map_err(|error| error.to_string())?;
    print_report(&report);
    Ok(())
}

/// Command-line sources must be credential-free; the env route exists for
/// credentialed URLs.
fn resolve_source(argument: &str) -> Result<MediaSource, String> {
    if argument.starts_with("rtsp://") || argument.starts_with("rtsps://") {
        let after_scheme = argument
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(argument);
        if after_scheme.contains('@') {
            return Err("refusing rtsp url with credentials on the command line; \
                 put the full url in NIAN_VISION_RTSP_URL and use `probe --rtsp-from-env`"
                .to_owned());
        }
        return Ok(MediaSource::Rtsp {
            url: RtspUrl::new(argument),
        });
    }
    Ok(MediaSource::file(PathBuf::from(argument)))
}

/// Renders the probe result in the format specified by the master prompt §M1.
#[allow(clippy::print_stdout)] // probe output is the CLI product
fn print_report(report: &nian_domain::MediaProbeReport) {
    println!("Connected");
    println!();

    for media_type in ["video", "audio"] {
        println!("{media_type}:");
        let stream = report
            .streams
            .iter()
            .find(|stream| stream.media_type.as_str() == media_type);
        match stream {
            Some(stream) => {
                println!("  codec: {}", stream.codec_name);
                if let Some(width) = stream.width {
                    println!("  width: {width}");
                    println!("  height: {}", stream.height.unwrap_or(0));
                }
                if let Some(rate) = stream.sample_rate {
                    println!("  sample_rate: {rate}");
                }
            }
            None => println!("  (none)"),
        }
        println!();
    }

    println!("streams: {}", report.streams.len());
    println!("format: {}", report.format_name);
    if let Some(duration) = report.duration {
        println!("duration: {:.3}s", duration.as_secs_f64());
    }
}

fn cmd_run() -> Result<(), String> {
    // Fail fast (ADR-0002): a worker that cannot prove its FFmpeg runtime
    // must exit non-zero instead of serving IPC traffic. Serving anyway would
    // make `hello`/`describe` report `ffmpeg: null` and every later call fail
    // mid-session, hiding an installation problem that is knowable up front.
    let media = FfmpegBackend::new()
        .map_err(|error| format!("ffmpeg startup validation failed: {error}"))?;

    // Same fail-fast invariant for the version read-out: after a successful
    // init the ABI check is guaranteed to succeed, so an error here is a real
    // anomaly and is propagated (never flattened into Option/`ffmpeg: null`).
    let versions = nian_media_ffmpeg::runtime_versions()
        .map_err(|error| format!("ffmpeg runtime validation failed: {error}"))?;
    let hello = json!({
        "worker": "nian-media-worker",
        "protocol": nian_ipc::PROTOCOL_VERSION,
        "ffmpeg": versions_json(versions),
    });

    // Hello goes out before the serve loop takes over stdout.
    let stdout = std::io::stdout();
    {
        let mut lock = stdout.lock();
        FramedWriter::new(&mut lock)
            .send(&Envelope::event(event::HELLO, hello))
            .map_err(|error| error.to_string())?;
    }

    let mut handler = WorkerHandler {
        versions,
        jobs: job::RecordingJobManager::new(),
        media,
    };
    serve(std::io::stdin().lock(), stdout.lock(), &mut handler)
        .map_err(|error| error.to_string())?;

    // M3 remediation §8: a REAL shutdown lifecycle replaces the old fixed
    // 5-second sleep. Graceful stop → bounded grace join (larger than any
    // normal bounded read + finalization headroom) → force-cancel only if
    // grace expires → absolute-bound join. Only the pathological forced
    // path may leave an active output partial — explicit, never a sleep.
    match handler.jobs.shutdown() {
        job::ShutdownDisposition::CleanExit => Ok(()),
        job::ShutdownDisposition::ForcedCancellationSurvived => {
            eprintln!("shutdown required forced cancellation of blocking media I/O");
            Ok(())
        }
        job::ShutdownDisposition::UnsafeTermination => {
            Err("forced-shutdown bound expired; active output left partial for recovery".to_owned())
        }
    }
}

fn versions_json(versions: RuntimeVersions) -> serde_json::Value {
    json!({
        "libavformat_major": versions.avformat,
        "libavcodec_major": versions.avcodec,
        "libavutil_major": versions.avutil,
    })
}

/// Manual-command ownership wrapper.
///
/// No production `RecordingSession` may write a canonical recording tree
/// unless its caller holds the matching `CameraLease` for the entire session
/// lifetime. The supervised job enforces that boundary in `job.rs`; this
/// wrapper enforces the same boundary for the narrow manual smoke command.
///
/// `session` stays inside an `Option` so `run(self)` can consume it while the
/// wrapper continues to own `_lease`. The session therefore finishes all mux,
/// finalization/publication and teardown work before the lease can drop.
#[derive(Debug)]
struct ManualRecordingSession {
    session: Option<nian_recorder::RecordingSession>,
    _lease: nian_storage::CameraLease,
}

impl ManualRecordingSession {
    fn open(
        source: &MediaSource,
        layout: nian_storage::RecordingsLayout,
        config: nian_recorder::RecorderConfig,
    ) -> Result<Self, String> {
        let lease =
            nian_storage::CameraLease::try_acquire(&layout, &config.camera).map_err(|error| {
                match error {
                    nian_storage::StorageError::CameraAlreadyActive { camera_id, .. } => {
                        format!("camera {camera_id} is already active")
                    }
                    other => format!("cannot acquire camera recording lease: {other}"),
                }
            })?;

        // Ownership comes first. A competing manual process must not even run
        // the ordinary write/delete probe, much less open media or create a
        // canonical partial, while another process owns this camera.
        layout
            .ensure_camera_dir(&config.camera)
            .map_err(|error| format!("storage preflight failed: {error}"))?;

        let session = nian_recorder::RecordingSession::open(source, layout, config)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            session: Some(session),
            _lease: lease,
        })
    }

    fn stop_flag(&self) -> Option<nian_recorder::StopFlag> {
        self.session
            .as_ref()
            .map(nian_recorder::RecordingSession::stop_flag)
    }

    fn interrupt_handle(&self) -> Option<nian_media_ffmpeg::InterruptHandle> {
        self.session
            .as_ref()
            .map(|session| session.interrupt_handle().clone())
    }

    fn run(
        mut self,
        events: &mut dyn FnMut(nian_recorder::RecordingEvent),
    ) -> Result<nian_recorder::RecordingSummary, String> {
        let Some(session) = self.session.take() else {
            return Err("manual recording session was already consumed".to_owned());
        };
        // `self` (and therefore `_lease`) remains alive for this entire call.
        // RecordingSession::run consumes the session and completes segment
        // finalization/publication before returning.
        session.run(events).map_err(|error| error.to_string())
    }
}

impl Drop for ManualRecordingSession {
    fn drop(&mut self) {
        // Explicitly destroy any not-yet-run session before Rust proceeds to
        // drop `_lease`. This preserves the same ordering on early-return
        // paths such as Ctrl+C handler installation failure.
        let _ = self.session.take();
    }
}

/// Manual development command (M2 §15, stop modes made explicit in M3):
/// record from a local file or `NIAN_VISION_RTSP_URL` into the recordings
/// layout. Stop modes are mutually exclusive and explicit:
///
/// * no `--duration` / no `--until-stdin-eof`: Ctrl+C is the only stop;
/// * `--duration N`: automatic timer stop OR Ctrl+C;
/// * `--until-stdin-eof`: stdin EOF (pipe close / Ctrl+D) OR Ctrl+C.
///
/// Ctrl+C itself is two-stage (first press graceful, second force-cancel).
/// A killed process leaves the active `.partial.mkv` recoverable — by
/// design, that is what reconciliation is for. Never run in CI; there is no
/// physical camera there.
///
/// Credential handling matches `probe`: argv sources must be
/// credential-free, the env route carries credentials and is never echoed.
/// Event output goes to stderr and contains file paths only.
fn cmd_record(args: &[String]) -> Result<(), String> {
    use nian_recorder::{AudioPolicy, RecorderConfig};
    use nian_storage::RecordingsLayout;
    use std::time::Duration as StdDuration;

    /// Explicit stop-mode selection; parsed from exactly one of the three
    /// shapes above. Before M3 this was implicit (`--until-stdin-eof` was
    /// accepted but any duration-less run also started the stdin watcher).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StopMode {
        /// Run until Ctrl+C; no watcher thread at all.
        SignalOnly,
        /// Request a graceful stop after this much wall-clock time.
        AfterDuration(StdDuration),
        /// Request a graceful stop when stdin reaches EOF.
        OnStdinEof,
    }

    let mut storage: Option<PathBuf> = None;
    let mut camera_name: Option<String> = None;
    let mut segment_target = nian_recorder::DEFAULT_SEGMENT_TARGET;
    let mut audio = AudioPolicy::CopyAll;
    let mut stop_mode: Option<StopMode> = None;
    let mut source: Option<MediaSource> = None;

    let mut rest = args;
    while let Some(flag) = rest.first() {
        let mut shift = 1;
        match flag.as_str() {
            "--storage" => {
                storage = Some(PathBuf::from(rest.get(1).ok_or("--storage needs a value")?));
                shift = 2;
            }
            "--camera" => {
                camera_name = Some(rest.get(1).ok_or("--camera needs a value")?.clone());
                shift = 2;
            }
            "--segment-target" => {
                let value = rest.get(1).ok_or("--segment-target needs seconds")?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "--segment-target must be a number of seconds")?;
                if seconds == 0 {
                    return Err("--segment-target must be greater than zero".to_owned());
                }
                segment_target = StdDuration::from_secs(seconds);
                shift = 2;
            }
            "--duration" => {
                if stop_mode.is_some() {
                    return Err(
                        "--duration and --until-stdin-eof are mutually exclusive".to_owned()
                    );
                }
                let value = rest.get(1).ok_or("--duration needs seconds")?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "--duration must be a number of seconds")?;
                stop_mode = Some(StopMode::AfterDuration(StdDuration::from_secs(seconds)));
                shift = 2;
            }
            "--no-audio" => audio = AudioPolicy::Exclude,
            "--until-stdin-eof" => {
                if stop_mode.is_some() {
                    return Err(
                        "--duration and --until-stdin-eof are mutually exclusive".to_owned()
                    );
                }
                stop_mode = Some(StopMode::OnStdinEof);
            }
            "--rtsp-from-env" => {
                let url = std::env::var(RTSP_URL_ENV).map_err(|_| {
                    format!("{RTSP_URL_ENV} is not set; it must hold the full rtsp:// URL")
                })?;
                source = Some(MediaSource::Rtsp {
                    url: RtspUrl::new(url),
                });
            }
            other if !other.starts_with("--") && source.is_none() => {
                source = Some(resolve_source(other)?);
            }
            other => return Err(format!("unknown record argument: {other}")),
        }
        rest = &rest[shift..];
    }

    let Some(storage) = storage else {
        return Err("--storage <DIR> is required".to_owned());
    };
    let Some(camera_name) = camera_name else {
        return Err("--camera <ID> is required".to_owned());
    };
    // Absent flags are a deliberate mode, not a default alias: signal-only.
    let stop_mode = stop_mode.unwrap_or(StopMode::SignalOnly);
    let Some(source) = source else {
        return Err(
            "a source is required: --rtsp-from-env or a path / credential-free rtsp url".to_owned(),
        );
    };
    let camera = nian_domain::CameraId::parse(&camera_name)
        .map_err(|error| format!("invalid --camera: {error}"))?;

    let layout = RecordingsLayout::new(storage).map_err(|error| error.to_string())?;
    let config = RecorderConfig::new(camera)
        .with_segment_target(segment_target)
        .with_audio(audio);
    let session = ManualRecordingSession::open(&source, layout, config)?;

    // Two-stage Ctrl+C (M2 review §9): the first press requests a graceful
    // stop; a second press forces interrupt cancellation. The graceful flag
    // alone takes effect between packets; since M3 a blocked read also has
    // its own stall deadline, but the force-cancel path remains for the
    // operator who will not wait that long.
    let signal_presses = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let signal_stop = session
        .stop_flag()
        .ok_or("manual recording session is unavailable")?;
    let signal_cancel = session
        .interrupt_handle()
        .ok_or("manual recording session is unavailable")?;
    let handler_presses = std::sync::Arc::clone(&signal_presses);
    ctrlc::set_handler(move || {
        use std::sync::atomic::Ordering;
        match handler_presses.fetch_add(1, Ordering::SeqCst) {
            0 => {
                eprintln!("Ctrl+C: stopping gracefully (press again to force-cancel)");
                signal_stop.request();
            }
            1 => {
                eprintln!("Ctrl+C: forcing cancellation of blocking media I/O");
                signal_cancel.cancel();
            }
            _ => {
                // Further presses are ignored; SIGKILL remains the operator's
                // hard exit, leaving recoverable partials behind.
            }
        }
    })
    .map_err(|error| format!("cannot install Ctrl+C handler: {error}"))?;

    // The stop trigger runs on its own thread so blocking FFmpeg reads do
    // not starve it. Only the EXPLICIT modes spawn one — signal-only runs
    // have no automatic stop by definition.
    match stop_mode {
        StopMode::SignalOnly => {}
        StopMode::AfterDuration(duration) => {
            let stop_flag = session
                .stop_flag()
                .ok_or("manual recording session is unavailable")?;
            std::thread::spawn(move || {
                std::thread::sleep(duration);
                stop_flag.request();
            });
        }
        StopMode::OnStdinEof => {
            let stop_flag = session
                .stop_flag()
                .ok_or("manual recording session is unavailable")?;
            std::thread::spawn(move || {
                // Read stdin to EOF: piping/closing the input or Ctrl+D
                // stops recording gracefully.
                let mut stdin = std::io::stdin().lock();
                let _ = std::io::copy(&mut stdin, &mut std::io::sink());
                stop_flag.request();
            });
        }
    }

    eprintln!("recording started");
    let summary = session.run(&mut |event| print_recording_event(&event))?;
    eprintln!(
        "recording finished ({:?}): {} segment(s), {} bytes",
        summary.end_reason, summary.finalized_segments, summary.bytes_written
    );
    Ok(())
}

#[cfg(test)]
mod manual_recording_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn manual_record_refuses_owned_camera_before_source_open_or_tree_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let layout = nian_storage::RecordingsLayout::new(root).unwrap();
        let camera = nian_domain::CameraId::parse("cam-manual-lease").unwrap();

        let holder = nian_storage::CameraLease::try_acquire(&layout, &camera).unwrap();
        let claim = layout
            .claim_segment(&camera, chrono::Local::now().naive_local())
            .unwrap();
        let live_partial = claim.partial_path().to_path_buf();
        let live_bytes = b"holder-live-partial-must-remain-untouched".to_vec();
        std::fs::write(&live_partial, &live_bytes).unwrap();

        // Deliberately invalid: reaching RecordingSession::open would surface
        // a media/source error. The ownership fence must win first.
        let invalid_source = MediaSource::file(temp.path().join("must-not-open.mkv"));
        let blocked = ManualRecordingSession::open(
            &invalid_source,
            layout.clone(),
            nian_recorder::RecorderConfig::new(camera.clone()),
        )
        .expect_err("the second manual writer must be rejected by the lease");

        assert_eq!(blocked, format!("camera {camera} is already active"));
        assert_eq!(std::fs::read(&live_partial).unwrap(), live_bytes);

        let day_dir = live_partial.parent().unwrap();
        let names: Vec<String> = std::fs::read_dir(day_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".recovered.mkv")),
            "manual conflict must not publish recovery output: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains(".recovery-")),
            "manual conflict must not create recovery scratch: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.ends_with(".done")),
            "manual conflict must not create recovery tombstones: {names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| name.ends_with(".partial.mkv"))
                .count(),
            1,
            "the blocked writer must not create another canonical partial"
        );

        drop(claim);
        drop(holder);

        // Once the holder is genuinely gone, the SAME manual entry path can
        // acquire ownership and reach media open normally.
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv"
        );
        let source = MediaSource::file(PathBuf::from(fixture));
        let reopened = ManualRecordingSession::open(
            &source,
            layout,
            nian_recorder::RecorderConfig::new(camera),
        );
        assert!(
            reopened.is_ok(),
            "manual ownership should be acquirable after release"
        );
    }
}

/// Renders recorder events for the operator; paths only — never URLs.
#[allow(clippy::print_stderr)]
fn print_recording_event(event: &nian_recorder::RecordingEvent) {
    match event {
        nian_recorder::RecordingEvent::SegmentStarted { partial_path, .. } => {
            eprintln!("segment started: {}", partial_path.display());
        }
        nian_recorder::RecordingEvent::SegmentFinalized {
            final_path,
            media_duration,
            size_bytes,
            ..
        } => {
            let duration = media_duration
                .map(|value| format!("{:.1}s", value.as_secs_f64()))
                .unwrap_or_else(|| "unknown".to_owned());
            eprintln!(
                "segment finalized: {} ({duration}, {size_bytes} bytes)",
                final_path.display()
            );
        }
        nian_recorder::RecordingEvent::SegmentAbandoned {
            partial_path,
            reason,
        } => {
            eprintln!("segment abandoned: {} ({reason})", partial_path.display());
        }
        nian_recorder::RecordingEvent::RecordingStarted { .. }
        | nian_recorder::RecordingEvent::RecordingStopped { .. } => {}
    }
}

/// IPC method implementations; the protocol lives in `nian-ipc`.
///
/// `versions` is held directly (not as `Option`): the run command validated
/// startup and versions before serving, so capability reporting can never
/// degrade to `ffmpeg: null` mid-session.
///
/// Since M3 the handler owns the single recording job
/// ([`job::RecordingJobManager`]) and serves the `recording.*` namespace:
/// `recording.start` validates and spawns the supervised job (streaming a
/// `recording.status` event through `writer` before replying), while
/// `recording.stop` escalates gracefully→forced across two presses.
struct WorkerHandler {
    versions: RuntimeVersions,
    jobs: job::RecordingJobManager,
    media: FfmpegBackend,
}

/// Names served by the recording namespace (kept next to their payloads).
pub mod recording_method {
    pub const START: &str = "recording.start";
    pub const STOP: &str = "recording.stop";
    pub const STATUS: &str = "recording.status";
}

pub mod camera_method {
    pub const PROBE: &str = "camera.probe";
}

pub mod playback_method {
    pub const PREPARE: &str = "playback.prepare";
}

impl<W: std::io::Write> nian_ipc::Handler<W> for WorkerHandler {
    fn handle(
        &mut self,
        method_name: &str,
        params: &serde_json::Value,
        writer: &mut FramedWriter<W>,
    ) -> nian_ipc::Dispatch {
        match method_name {
            method::PING => nian_ipc::Dispatch::Reply(Ok(json!({
                "pong": true,
            }))),
            method::DESCRIBE => nian_ipc::Dispatch::Reply(Ok(json!({
                "worker": "nian-media-worker",
                "protocol": nian_ipc::PROTOCOL_VERSION,
                "ffmpeg": versions_json(self.versions),
                "recording": {
                    "one_job_per_worker": true,
                    "reconnect_supervised": true,
                },
            }))),
            recording_method::START => match job::JobSpec::from_params(params) {
                Err(reason) => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(
                    with_reason(job::code::INVALID_PARAMS, reason),
                ))),
                Ok(spec) => match self.jobs.start(spec) {
                    Ok(()) => {
                        // Immediate progress signal: the supervised job is
                        // connecting; further updates ride status requests
                        // and the finished event below.
                        let _ = writer.send(&Envelope::event(
                            "recording.status",
                            self.jobs.status().to_json(),
                        ));
                        nian_ipc::Dispatch::Reply(Ok(json!({
                            "started": true,
                        })))
                    }
                    Err(code) => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(code))),
                },
            },
            recording_method::STOP => match self.jobs.stop() {
                Ok(presses) => nian_ipc::Dispatch::Reply(Ok(json!({
                    "stop_presses": presses,
                    "graceful": presses == 1,
                }))),
                Err(code) => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(code))),
            },
            recording_method::STATUS => {
                // Canonical wire shape (final remediation §1): the result IS
                // the JobStatus object (`finished`, `end_kind`,
                // `failure_category`, `recovery`, …) — no wrapper layer, so
                // the parent's terminal-state parser consumes exactly what
                // this serves.
                nian_ipc::Dispatch::Reply(Ok(self.jobs.status().to_json()))
            }
            camera_method::PROBE => match probe_params(params) {
                Ok((source, timeout)) => match self.media.probe_with_timeout(&source, timeout) {
                    Ok(report) => {
                        let video = report.video_stream();
                        let audio_stream_count = report
                            .streams
                            .iter()
                            .filter(|stream| stream.media_type == nian_domain::MediaType::Audio)
                            .count();
                        nian_ipc::Dispatch::Reply(Ok(json!({
                            "reachable": true,
                            "video_stream_found": video.is_some(),
                            "codec": video.map(|stream| stream.codec_name.clone()),
                            "width": video.and_then(|stream| stream.width),
                            "height": video.and_then(|stream| stream.height),
                            "audio_stream_count": audio_stream_count,
                        })))
                    }
                    Err(error) => {
                        let code = if error.is_timed_out() {
                            "source_timeout"
                        } else if error.is_interrupted() {
                            "cancelled"
                        } else if matches!(error, nian_media::MediaError::OpenFailed { .. }) {
                            "source_open_failed"
                        } else {
                            "source_probe_failed"
                        };
                        nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(code)))
                    }
                },
                Err(reason) => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(
                    with_reason(job::code::INVALID_PARAMS, reason),
                ))),
            },
            playback_method::PREPARE => match playback_prepare(params) {
                Ok(result) => nian_ipc::Dispatch::Reply(Ok(result)),
                Err(code) => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new(code))),
            },
            method::SHUTDOWN => {
                // Final remediation §4: process shutdown is its OWN
                // orchestration — one IDEMPOTENT graceful request here; the
                // run loop's bounded grace/force-cancel/join sequence in
                // cmd_run does the waiting. The operator two-press counter
                // is never consulted, so a shutdown can never escalate to a
                // forced cancellation by itself.
                self.jobs.request_graceful_stop();
                nian_ipc::Dispatch::ShutdownReply(Ok(json!({"bye": true})))
            }
            _ => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new("method_not_found"))),
        }
    }
}

fn probe_params(
    params: &serde_json::Value,
) -> Result<(MediaSource, std::time::Duration), &'static str> {
    let source = params.get("source").ok_or("missing 'source'")?;
    let kind = source
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing 'source.kind'")?;
    let source = match kind {
        "file" => {
            let path = source
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing 'source.path'")?;
            MediaSource::File(PathBuf::from(path))
        }
        "rtsp" => {
            let url = source
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing 'source.url'")?;
            MediaSource::Rtsp {
                url: RtspUrl::new(url.to_owned()),
            }
        }
        _ => return Err("unknown 'source.kind' (file|rtsp)"),
    };
    let timeout_ms = params
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(10_000);
    if !(100..=60_000).contains(&timeout_ms) {
        return Err("'timeout_ms' must be between 100 and 60000");
    }
    Ok((source, std::time::Duration::from_millis(timeout_ms)))
}

fn playback_prepare(params: &serde_json::Value) -> Result<serde_json::Value, &'static str> {
    let source_path = params
        .get("source_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or("internal")?;
    let output_path = params
        .get("output_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or("internal")?;
    let timeout_ms = params
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(60_000);
    if !source_path.is_absolute()
        || !output_path.is_absolute()
        || source_path == output_path
        || !(100..=120_000).contains(&timeout_ms)
    {
        return Err("internal");
    }

    let metadata = std::fs::symlink_metadata(&source_path).map_err(|_| "media_unreadable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("media_unreadable");
    }

    // The host owns a unique per-session directory; create_new makes the
    // worker's cache-file ownership explicit before FFmpeg opens that claim.
    let claim = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|_| "internal")?;
    drop(claim);

    let interrupt = InterruptHandle::new();
    let _deadline = interrupt.scoped_deadline(Duration::from_millis(timeout_ms));
    let mut input = match MediaInput::open(&MediaSource::File(source_path), &interrupt) {
        Ok(input) => input,
        Err(error) => {
            let _ = std::fs::remove_file(&output_path);
            return if error.is_timed_out() || error.is_interrupted() {
                Err("worker_unavailable")
            } else {
                Err("media_unreadable")
            };
        }
    };
    let streams = input.streams();
    let Some(video) = streams
        .iter()
        .find(|stream| stream.media_type == nian_domain::MediaType::Video)
        .cloned()
    else {
        let _ = std::fs::remove_file(&output_path);
        return Err("unsupported_codec");
    };
    if video.codec_name != "h264" {
        let _ = std::fs::remove_file(&output_path);
        return Err("unsupported_codec");
    }
    let audio_indices: std::collections::HashSet<u32> = streams
        .iter()
        .filter(|stream| {
            stream.media_type == nian_domain::MediaType::Audio && stream.codec_name == "aac"
        })
        .map(|stream| stream.stream_index)
        .collect();
    let video_index = video.stream_index;
    let mut muxer = MatroskaMuxer::create_fragmented_mp4_with_selection(
        &mut input,
        &output_path,
        &interrupt,
        |stream| stream.stream_index == video_index || audio_indices.contains(&stream.stream_index),
    )
    .map_err(|_| "unsupported_container")?;

    while let Some(packet) = input.next_packet().map_err(|error| {
        if error.is_timed_out() || error.is_interrupted() {
            "worker_unavailable"
        } else {
            "media_unreadable"
        }
    })? {
        muxer
            .write_packet(&packet)
            .map_err(|_| "unsupported_container")?;
    }
    muxer.finalize().map_err(|_| "unsupported_container")?;

    let duration_ms = input
        .duration()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    Ok(json!({
        "duration_ms": duration_ms,
        "video_codec": video.codec_name,
        "width": video.width,
        "height": video.height,
        "audio_available": !audio_indices.is_empty(),
        "container_compatibility": "fragmented_mp4",
        "seekable": true,
    }))
}

/// Combines a stable error code with a short, secret-free reason so hosts
/// can display WHY validation failed without parsing payloads ad hoc.
fn with_reason(code: &str, reason: &str) -> String {
    format!("{code}:{reason}")
}
