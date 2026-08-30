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

fn request_with_params(id: u64, name: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "type": "request",
        "v": PROTOCOL_VERSION,
        "id": id,
        "method": name,
        "params": params,
    })
    .to_string()
        + "\n"
}

#[test]
fn ipc_camera_probe_reports_local_video_without_creating_recording_artifacts() {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/sample_av.mkv");
    let temp = tempfile::tempdir().unwrap();
    let before = std::fs::read_dir(temp.path()).unwrap().count();

    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    stdin
        .write_all(
            request_with_params(
                1,
                "camera.probe",
                serde_json::json!({
                    "source": {"kind":"file", "path": fixture},
                    "timeout_ms": 5_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("expected probe response");
    };
    assert_eq!(id, 1);
    assert!(ok);
    assert_eq!(result["reachable"], true);
    assert_eq!(result["video_stream_found"], true);
    assert!(result["codec"].is_string());
    assert!(result["width"].is_u64());
    assert!(result["height"].is_u64());

    stdin
        .write_all(request(2, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), before);
}

#[test]
fn ipc_camera_probe_unreachable_source_returns_typed_bounded_failure() {
    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    let started = std::time::Instant::now();
    stdin
        .write_all(
            request_with_params(
                1,
                "camera.probe",
                serde_json::json!({
                    "source": {"kind":"rtsp", "url":"rtsp://127.0.0.1:9/stream1"},
                    "timeout_ms": 1_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        id, ok, error_code, ..
    } = read_message(&mut reader)
    else {
        panic!("expected probe failure response");
    };
    assert_eq!(id, 1);
    assert!(!ok);
    assert!(matches!(
        error_code.as_deref(),
        Some("source_open_failed" | "source_timeout")
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));

    stdin
        .write_all(request(2, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
}

#[test]
fn ipc_playback_prepare_packet_copies_h264_mkv_to_browser_mp4() {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/playback_h264.mkv");
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("playback.mp4");

    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    stdin
        .write_all(
            request_with_params(
                1,
                "playback.prepare",
                serde_json::json!({
                    "source_path": fixture,
                    "output_path": output,
                    "timeout_ms": 10_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        id,
        ok,
        result,
        error_code,
        ..
    } = read_message(&mut reader)
    else {
        panic!("expected playback response");
    };
    assert_eq!(id, 1);
    assert!(ok, "playback prepare failed: {error_code:?}");
    assert_eq!(result["video_codec"], "h264");
    assert_eq!(result["width"], 160);
    assert_eq!(result["height"], 120);
    assert_eq!(result["audio_available"], true);
    assert_eq!(result["container_compatibility"], "fragmented_mp4");
    assert_eq!(result["seekable"], true);
    assert!(
        result["duration_ms"]
            .as_u64()
            .is_some_and(|ms| (3_500..=4_500).contains(&ms))
    );

    let bytes = std::fs::read(&output).unwrap();
    assert!(bytes.len() > 10_000);
    assert!(
        bytes.windows(4).any(|window| window == b"sidx"),
        "worker output must contain the global fragmented-MP4 seek index"
    );

    stdin
        .write_all(request(2, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
}

#[test]
fn ipc_playback_prepare_rejects_malformed_media_with_typed_failure() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("broken.mkv");
    let output = temp.path().join("playback.mp4");
    std::fs::write(&source, b"not media").unwrap();

    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);
    stdin.write_all(request_with_params(1, "playback.prepare", serde_json::json!({"source_path": source, "output_path": output, "timeout_ms": 2_000})).as_bytes()).unwrap();
    let Envelope::Response { ok, error_code, .. } = read_message(&mut reader) else {
        panic!("expected playback failure")
    };
    assert!(!ok);
    assert_eq!(error_code.as_deref(), Some("media_unreadable"));
    stdin
        .write_all(request(2, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
}
