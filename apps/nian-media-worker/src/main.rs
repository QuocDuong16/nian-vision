//! nian-media-worker process entry point.
//!
//! Subcommands:
//!
//! ```text
//! nian-media-worker probe <path|credential-free-rtsp-url>
//! nian-media-worker probe --rtsp-from-env     # URL from NIAN_VISION_RTSP_URL
//! nian-media-worker run                       # NDJSON IPC loop on stdio
//! ```
//!
//! Secrets policy (master spec §4/§7): RTSP URLs containing credentials are
//! rejected as command-line arguments — process listings are world-readable.
//! Use `--rtsp-from-env` with `NIAN_VISION_RTSP_URL` instead. Logs go to
//! stderr; stdout is reserved for the IPC protocol (and `probe` output,
//! which is the CLI product).

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
       nian-media-worker run";

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
    let versions = nian_media_ffmpeg::runtime_versions();
    let hello = json!({
        "worker": "nian-media-worker",
        "protocol": nian_ipc::PROTOCOL_VERSION,
        "ffmpeg": versions.map(versions_json),
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

/// IPC method implementations; the protocol lives in `nian-ipc`.
struct WorkerHandler {
    versions: Option<RuntimeVersions>,
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
                "ffmpeg": self.versions.map(versions_json),
            }))),
            method::SHUTDOWN => nian_ipc::Dispatch::ShutdownReply(Ok(json!({"bye": true}))),
            _ => nian_ipc::Dispatch::Reply(Err(nian_ipc::RpcFailure::new("method_not_found"))),
        }
    }
}
