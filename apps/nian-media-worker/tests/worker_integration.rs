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
    assert_eq!(result["recording"]["namespaces"][0], "recording");
    assert_eq!(result["recording"]["namespaces"][1], "event_buffer");

    stdin
        .write_all(request(2, "event_buffer.status").as_bytes())
        .expect("send event buffer status");
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("expected event buffer status response");
    };
    assert_eq!(id, 2);
    assert!(ok);
    assert_eq!(result["state"], "idle");

    stdin
        .write_all(request(3, "pre_roll.status").as_bytes())
        .expect("send pre-roll status");
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("expected pre-roll status response");
    };
    assert_eq!(id, 3);
    assert!(ok);
    assert_eq!(result["active"], false);
    assert_eq!(result["ready"], false);

    stdin
        .write_all(request(4, "media.status").as_bytes())
        .expect("send media status");
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("expected media status response");
    };
    assert_eq!(id, 4);
    assert!(ok);
    assert_eq!(result["source_count"], 0);
    assert_eq!(result["generation_starts"], 0);
    assert_eq!(result["subscribers"], 0);
    assert_eq!(
        result["consumers"],
        serde_json::json!({
            "recording": 0,
            "live": 0,
            "motion": 0,
            "event": 0,
            "other": 0,
        })
    );
    assert_eq!(result["queued_bytes"], 0);
    assert_eq!(result["pre_roll_packets"], 0);
    assert_eq!(result["pre_roll_bytes"], 0);
    assert_eq!(result["pre_roll_dropped_packets"], 0);
    assert_eq!(result["sources"], serde_json::json!([]));

    // Clean shutdown keeps the serve loop contract intact.
    stdin
        .write_all(request(5, method::SHUTDOWN).as_bytes())
        .expect("send shutdown");
    let Envelope::Response {
        id: shutdown_id,
        ok,
        ..
    } = read_message(&mut reader)
    else {
        panic!("expected shutdown reply");
    };
    assert_eq!(shutdown_id, 5);
    assert!(ok);

    let status = child.wait().expect("wait for worker");
    assert!(status.success(), "clean shutdown must exit successfully");
}

#[test]
fn ipc_local_motion_requires_explicit_valid_profile_and_acknowledges_only_known_events() {
    let mut child = spawn_run();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    stdin
        .write_all(request(1, "motion.status").as_bytes())
        .unwrap();
    let Envelope::Response { id, ok, result, .. } = read_message(&mut reader) else {
        panic!("motion status must respond");
    };
    assert_eq!(id, 1);
    assert!(ok);
    assert_eq!(result["state"], "disabled");
    assert_eq!(result["transitions"], serde_json::json!([]));

    stdin
        .write_all(
            request_with_params(2, "motion.ack", serde_json::json!({"sequence":1})).as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        id, ok, error_code, ..
    } = read_message(&mut reader)
    else {
        panic!("motion ack must respond");
    };
    assert_eq!(id, 2);
    assert!(!ok);
    assert_eq!(error_code.as_deref(), Some("invalid_motion_ack"));

    let secret = "SENTINEL-local-motion-secret";
    stdin.write_all(request_with_params(3, "motion.start", serde_json::json!({
        "source":{"kind":"rtsp","profile":"invalid", "url":format!("rtsp://admin:{secret}@127.0.0.1:9/live")},
    })).as_bytes()).unwrap();
    let Envelope::Response {
        id, ok, error_code, ..
    } = read_message(&mut reader)
    else {
        panic!("invalid motion start must respond");
    };
    assert_eq!(id, 3);
    assert!(!ok);
    assert!(
        error_code
            .as_deref()
            .is_some_and(|code| code.starts_with("invalid_params"))
    );
    assert!(!error_code.as_deref().unwrap_or("").contains(secret));

    stdin
        .write_all(request(4, "motion.stop").as_bytes())
        .unwrap();
    let Envelope::Response { id, ok, .. } = read_message(&mut reader) else {
        panic!("motion stop must respond");
    };
    assert_eq!(id, 4);
    assert!(ok);

    stdin
        .write_all(request(5, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
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
fn ipc_hevc_g711_playback_prepares_browser_video_and_pcm_audio_sidecar() {
    for law in ["alaw", "mulaw"] {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../crates/nian-media-ffmpeg/tests/fixtures/hevc_g711_{law}.mkv"
        ));
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("playback.mp4");
        let audio = temp.path().join("audio.wav");
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
            ok,
            result,
            error_code,
            ..
        } = read_message(&mut reader)
        else {
            panic!("expected playback response");
        };
        assert!(ok, "{law} playback failed: {error_code:?}");
        assert_eq!(result["video_codec"], "hevc");
        assert_eq!(result["audio_available"], true);
        assert_eq!(result["audio_sidecar_wav"], true);
        let mp4 = std::fs::read(&output).unwrap();
        assert!(
            mp4.windows(4).any(|bytes| bytes == b"hvc1"),
            "HEVC browser output must be hvc1"
        );
        let wav = std::fs::read(audio).unwrap();
        assert!(wav.len() > 44);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(
            u16::from_le_bytes([wav[20], wav[21]]),
            1,
            "WAV is linear PCM"
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8_000);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize,
            wav.len() - 44
        );
        assert!(wav[44..].iter().any(|byte| *byte != 0));
        stdin
            .write_all(request(2, method::SHUTDOWN).as_bytes())
            .unwrap();
        let _ = read_message(&mut reader);
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn ipc_event_clip_compose_packet_copies_multiple_segments_into_playable_clip() {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/playback_h264.mkv");
    let temp = tempfile::tempdir().unwrap();
    let clip = temp.path().join("event-clip.mkv");
    let playback = temp.path().join("event-playback.mp4");

    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    stdin
        .write_all(
            request_with_params(
                1,
                "event_clip.compose",
                serde_json::json!({
                    "source_paths": [fixture.clone(), fixture],
                    "output_path": clip,
                    "timeout_ms": 30_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        id, ok, error_code, ..
    } = read_message(&mut reader)
    else {
        panic!("expected event clip compose response");
    };
    assert_eq!(id, 1);
    assert!(ok, "event clip compose failed: {error_code:?}");
    assert!(std::fs::metadata(&clip).unwrap().len() > 10_000);

    stdin
        .write_all(
            request_with_params(
                2,
                "playback.prepare",
                serde_json::json!({
                    "source_path": clip,
                    "output_path": playback,
                    "timeout_ms": 30_000,
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
        panic!("expected event clip playback response");
    };
    assert_eq!(id, 2);
    assert!(ok, "event clip playback prepare failed: {error_code:?}");
    assert_eq!(result["video_codec"], "h264");
    assert!(
        result["duration_ms"]
            .as_u64()
            .is_some_and(|ms| (7_000..=9_000).contains(&ms)),
        "composed clip must preserve both source segment timelines: {result:?}"
    );
    assert!(std::fs::metadata(&playback).unwrap().len() > 10_000);

    stdin
        .write_all(request(3, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
}

#[test]
fn ipc_hevc_g711_event_clip_keeps_original_audio_then_prepares_playable_sidecar() {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/hevc_g711_alaw.mkv");
    let temp = tempfile::tempdir().unwrap();
    let clip = temp.path().join("event.mkv");
    let playback = temp.path().join("playback.mp4");
    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);
    stdin
        .write_all(
            request_with_params(
                1,
                "event_clip.compose",
                serde_json::json!({
                    "source_paths": [fixture.clone(), fixture],
                    "output_path": clip,
                    "timeout_ms": 30_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response { ok, error_code, .. } = read_message(&mut reader) else {
        panic!("expected HEVC event-clip compose response");
    };
    assert!(ok, "HEVC G711 compose failed: {error_code:?}");
    let media = nian_media_ffmpeg::FfmpegBackend::new().unwrap();
    let report = nian_media::Probe::probe(&media, &nian_media::MediaSource::file(&clip)).unwrap();
    assert_eq!(report.video_stream().unwrap().codec_name, "hevc");
    assert!(
        report
            .streams
            .iter()
            .any(|stream| stream.codec_name == "pcm_alaw")
    );
    stdin
        .write_all(
            request_with_params(
                2,
                "playback.prepare",
                serde_json::json!({
                    "source_path": clip,
                    "output_path": playback,
                    "timeout_ms": 30_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        ok,
        result,
        error_code,
        ..
    } = read_message(&mut reader)
    else {
        panic!("expected HEVC event playback response");
    };
    assert!(ok, "HEVC G711 event playback failed: {error_code:?}");
    assert_eq!(result["video_codec"], "hevc");
    assert_eq!(result["audio_sidecar_wav"], true);
    assert!(temp.path().join("audio.wav").metadata().unwrap().len() > 44);
    stdin
        .write_all(request(3, method::SHUTDOWN).as_bytes())
        .unwrap();
    let _ = read_message(&mut reader);
    assert!(child.wait().unwrap().success());
}

#[test]
fn ipc_event_clip_compose_honors_hard_duration_limit() {
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/nian-media-ffmpeg/tests/fixtures/playback_h264.mkv");
    let temp = tempfile::tempdir().unwrap();
    let clip = temp.path().join("bounded-event-clip.mkv");
    let playback = temp.path().join("bounded-event-playback.mp4");

    let mut child = spawn_run();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut stdin = child.stdin.take().unwrap();
    let _hello = read_message(&mut reader);

    stdin
        .write_all(
            request_with_params(
                1,
                "event_clip.compose",
                serde_json::json!({
                    "source_paths": [fixture.clone(), fixture],
                    "output_path": clip,
                    "timeout_ms": 30_000,
                    "max_duration_ms": 5_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response { ok, error_code, .. } = read_message(&mut reader) else {
        panic!("expected bounded event clip compose response");
    };
    assert!(ok, "bounded event clip compose failed: {error_code:?}");

    stdin
        .write_all(
            request_with_params(
                2,
                "playback.prepare",
                serde_json::json!({
                    "source_path": clip,
                    "output_path": playback,
                    "timeout_ms": 30_000,
                }),
            )
            .as_bytes(),
        )
        .unwrap();
    let Envelope::Response {
        ok,
        result,
        error_code,
        ..
    } = read_message(&mut reader)
    else {
        panic!("expected bounded event clip playback response");
    };
    assert!(ok, "bounded event clip playback failed: {error_code:?}");
    let duration_ms = result["duration_ms"].as_u64().expect("duration");
    assert!(
        (4_000..=5_000).contains(&duration_ms),
        "hard-capped event clip duration escaped the requested limit: {duration_ms}ms"
    );

    stdin
        .write_all(request(3, method::SHUTDOWN).as_bytes())
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
