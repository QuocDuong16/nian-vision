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

use std::path::PathBuf;
use std::process::ExitCode;

use nian_ipc::message::{Envelope, event, method};
use nian_ipc::{FramedWriter, serve};
use nian_media::{MediaSource, Probe, RtspUrl};
use nian_media_ffmpeg::{FfmpegBackend, RuntimeVersions};
use serde_json::json;

const USAGE: &str = "usage: nian-media-worker probe <path|credential-free-rtsp-url>
       nian-media-worker probe --rtsp-from-env
       nian-media-worker run
       nian-media-worker record --storage <DIR> --camera <ID>
                                [--segment-target <SECONDS>] [--no-audio]
                                [--duration <SECONDS> | --until-stdin-eof]
                                (--rtsp-from-env | <path|credential-free-rtsp-url>)";

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
    FfmpegBackend::new().map_err(|error| format!("ffmpeg startup validation failed: {error}"))?;

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

    let mut handler = WorkerHandler { versions };
    serve(std::io::stdin().lock(), stdout.lock(), &mut handler).map_err(|error| error.to_string())
}

fn versions_json(versions: RuntimeVersions) -> serde_json::Value {
    json!({
        "libavformat_major": versions.avformat,
        "libavcodec_major": versions.avcodec,
        "libavutil_major": versions.avutil,
    })
}

/// Manual development command (M2 §15): record from a local file or
/// `NIAN_VISION_RTSP_URL` into the recordings layout for a bounded duration
/// or until stdin closes. Ctrl+C kills the process and leaves the active
/// `.partial.mkv` recoverable — by design, that is what reconciliation is
/// for. Never run in CI; there is no physical camera there.
///
/// Credential handling matches `probe`: argv sources must be
/// credential-free, the env route carries credentials and is never echoed.
/// Event output goes to stderr and contains file paths only.
fn cmd_record(args: &[String]) -> Result<(), String> {
    use nian_recorder::{AudioPolicy, RecorderConfig, RecordingSession};
    use nian_storage::RecordingsLayout;
    use std::time::Duration as StdDuration;

    let mut storage: Option<PathBuf> = None;
    let mut camera_name: Option<String> = None;
    let mut segment_target = nian_recorder::DEFAULT_SEGMENT_TARGET;
    let mut audio = AudioPolicy::CopyAll;
    let mut stop_after: Option<StdDuration> = None;
    let mut until_stdin_eof = false;
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
                let value = rest.get(1).ok_or("--duration needs seconds")?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "--duration must be a number of seconds")?;
                stop_after = Some(StdDuration::from_secs(seconds));
                shift = 2;
            }
            "--no-audio" => audio = AudioPolicy::Exclude,
            "--until-stdin-eof" => until_stdin_eof = true,
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
    if stop_after.is_some() && until_stdin_eof {
        return Err("--duration and --until-stdin-eof are mutually exclusive".to_owned());
    }
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
    let session =
        RecordingSession::open(&source, layout, config).map_err(|error| error.to_string())?;

    // The stop trigger runs on its own thread so blocking FFmpeg reads do
    // not starve it; the graceful flag takes effect between packets only.
    let stop_flag = session.stop_flag();
    std::thread::spawn(move || match stop_after {
        Some(duration) => {
            std::thread::sleep(duration);
            stop_flag.request();
        }
        None => {
            // Read stdin to EOF: piping/closing the input or Ctrl+D stops
            // recording gracefully.
            let mut stdin = std::io::stdin().lock();
            let _ = std::io::copy(&mut stdin, &mut std::io::sink());
            stop_flag.request();
        }
    });

    eprintln!("recording started");
    let summary = session
        .run(&mut |event| print_recording_event(&event))
        .map_err(|error| error.to_string())?;
    eprintln!(
        "recording finished ({:?}): {} segment(s), {} bytes",
        summary.end_reason, summary.finalized_segments, summary.bytes_written
    );
    Ok(())
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
struct WorkerHandler {
    versions: RuntimeVersions,
}

impl nian_ipc::Handler for WorkerHandler {
    fn handle(&mut self, method_name: &str, _params: &serde_json::Value) -> nian_ipc::Dispatch {
        match method_name {
            method::PING => nian_ipc::Dispatch::Reply(Ok(json!({
                "pong": true,
            }))),
            method::DESCRIBE => nian_ipc::Dispatch::Reply(Ok(json!({
                "worker": "nian-media-worker",
                "protocol": nian_ipc::PROTOCOL_VERSION,
                "ffmpeg": versions_json(self.versions),
            }))),
            method::SHUTDOWN => nian_ipc::Dispatch::ShutdownReply(Ok(json!({"bye": true}))),
            _ => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new("method_not_found"))),
        }
    }
}
