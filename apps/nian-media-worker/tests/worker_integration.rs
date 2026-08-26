//! End-to-end tests of the worker binary over its real stdio IPC channel.
//!
//! Covers the startup contract (fail-fast init, hello/describe capability
//! report) and the FFmpeg native-log secret boundary: credentials handed to
//! the worker through the environment must never appear in any output
//! stream, even when FFmpeg itself processes the credentialed URL.

// Tests assert with panicking macros by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use nian_ipc::PROTOCOL_VERSION;
use nian_ipc::message::{Envelope, event, method};

fn spawn_run() -> Child {
    Command::new(env!("CARGO_BIN_EXE_nian-media-worker"))
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nian-media-worker run")
}

/// Reads one framed message from the child's stdout.
fn read_message(reader: &mut BufReader<std::process::ChildStdout>) -> Envelope {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read framed message");
    serde_json::from_str(&line).expect("valid envelope on worker stdout")
}

fn request(id: u64, name: &str) -> String {
    serde_json::json!({
        "type": "request",
        "v": PROTOCOL_VERSION,
        "id": id,
        "method": name,
        "params": serde_json::Value::Null,
    })
    .to_string()
        + "\n"
}

#[test]
fn hello_and_describe_report_ffmpeg_capability_after_successful_init() {
    let mut child = spawn_run();
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().expect("piped stdin");

    // The very first message is the hello event with a populated ffmpeg
    // object — never `null`, because `run` fails fast when init fails.
    let Envelope::Event { name, data, .. } = read_message(&mut reader) else {
        panic!("expected an event envelope");
    };
    assert_eq!(name, event::HELLO);
    assert_eq!(data["worker"], "nian-media-worker");
    let ffmpeg = &data["ffmpeg"];
    assert!(
        ffmpeg.is_object(),
        "hello must carry ffmpeg versions after fail-fast init: {ffmpeg}"
    );
    assert!(ffmpeg["libavformat_major"].is_u64());
    assert!(ffmpeg["libavcodec_major"].is_u64());
    assert!(ffmpeg["libavutil_major"].is_u64());
    assert_eq!(
        ffmpeg["libavformat_major"].as_u64(),
        Some(u64::from(nian_ffmpeg_sys::LIBAVFORMAT_VERSION_MAJOR))
    );

    stdin
        .write_all(request(1, method::DESCRIBE).as_bytes())
        .expect("send describe");
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("expected a response envelope");
    };
    assert_eq!(id, 1);
    assert!(ok);
    assert!(
        result["ffmpeg"]["libavcodec_major"].is_u64(),
        "describe must not silently report ffmpeg:null: {result}"
    );

    // Clean shutdown keeps the serve loop contract intact.
    stdin
        .write_all(request(2, method::SHUTDOWN).as_bytes())
        .expect("send shutdown");
    let Envelope::Response {
        id: shutdown_id,
        ok,
        ..
    } = read_message(&mut reader)
    else {
        panic!("expected shutdown reply");
    };
    assert_eq!(shutdown_id, 2);
    assert!(ok);

    let status = child.wait().expect("wait for worker");
    assert!(status.success(), "clean shutdown must exit successfully");
}

#[test]
fn rtsp_credentials_never_reach_any_output_stream() {
    // Sentinel credential pair; if any code path (FFmpeg native logging,
    // Rust error rendering, tracing) leaks the URL, these strings appear in
    // the captured output and fail the test.
    const SENTINEL_USER: &str = "sentinel-user-7qz";
    const SENTINEL_PASSWORD: &str = "S3ntin3l-P4ss:w0rd/";
    // Port 9 (discard) refuses immediately on loopback; the connect error is
    // expected, what matters is what the failure text contains.
    let url = format!("rtsp://{SENTINEL_USER}:{SENTINEL_PASSWORD}@127.0.0.1:9/stream1");

    let output = Command::new(env!("CARGO_BIN_EXE_nian-media-worker"))
        .args(["probe", "--rtsp-from-env"])
        .env("NIAN_VISION_RTSP_URL", &url)
        .output()
        .expect("spawn probe");

    assert!(!output.status.success(), "unreachable camera must fail");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [SENTINEL_PASSWORD, SENTINEL_USER] {
        assert!(
            !combined.contains(secret),
            "credential leaked into worker output: {combined:?}"
        );
    }
}

#[test]
fn failed_probe_prints_no_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_nian-media-worker"))
        .args(["probe", "/nonexistent/nian-vision-missing.mkv"])
        .output()
        .expect("spawn probe");

    assert!(!output.status.success());
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout_text.contains("Connected"),
        "failed probe must not print a report"
    );
}
