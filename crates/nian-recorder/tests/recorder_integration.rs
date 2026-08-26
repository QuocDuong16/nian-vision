//! Deterministic M2 recorder integration tests.
//!
//! Sources are generated FFmpeg-native fixtures (LGPL encoders, synthetic
//! lavfi inputs — see `scripts/generate-fixture.sh`); no physical camera
//! and no GPL component participates. Segments are inspected through the
//! safe media API ([`MediaInput`] packets + [`Probe`]) rather than shelling
//! out to ffprobe.
//!
//! Fixture facts these tests rely on:
//!
//! * `session_av.mkv` — 30 s, 10 fps MPEG-4 video (keyframe every 2 s,
//!   `-g 20`) plus AAC audio; long enough for several rotations at a 5 s
//!   target.
//! * `reordered_av.mkv` — audio is stream 0, video is stream **1**.
//! * `audio_only.mkv` — no video at all.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use nian_domain::{CameraId, MediaType};
use nian_media::{MediaSource, Probe};
use nian_media_ffmpeg::{FfmpegBackend, InterruptHandle, MediaInput};
use nian_recorder::{
    AudioPolicy, RecorderConfig, RecordingEndReason, RecordingError, RecordingEvent,
    RecordingSession,
};
use nian_storage::RecordingsLayout;
use nian_storage::paths::parse_segment_file_name;

/// Keyframe interval of `session_av.mkv` (rate 10 fps / `-g 20`).
const SESSION_GOP: Duration = Duration::from_millis(2000);

fn fixtures_dir() -> PathBuf {
    // This crate lives at crates/nian-recorder; shared fixtures live in
    // crates/nian-media-ffmpeg/tests/fixtures.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/nian-media-ffmpeg/tests/fixtures")
}

fn fixture(name: &str) -> MediaSource {
    let path = fixtures_dir().join(name);
    assert!(path.is_file(), "fixture missing: {}", path.display());
    MediaSource::File(path)
}

fn camera() -> CameraId {
    CameraId::parse("rec-test").unwrap()
}

fn layout_in(root: &Path) -> RecordingsLayout {
    RecordingsLayout::new(root.join("recordings")).unwrap()
}

/// Every file below `dir`, recursively (the tree stays small).
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue; // directory does not exist (yet)
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

fn final_files(root: &Path) -> Vec<PathBuf> {
    all_files(root)
        .into_iter()
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "mkv")
                && !path.to_string_lossy().contains(".partial.")
        })
        .collect()
}

fn partial_files(root: &Path) -> Vec<PathBuf> {
    all_files(root)
        .into_iter()
        .filter(|path| path.to_string_lossy().contains(".partial."))
        .collect()
}

/// Final paths in publication order, taken from `SegmentFinalized` events.
fn finalized_paths(events: &[RecordingEvent]) -> Vec<PathBuf> {
    events
        .iter()
        .filter_map(|event| match event {
            RecordingEvent::SegmentFinalized { final_path, .. } => Some(final_path.clone()),
            _ => None,
        })
        .collect()
}

/// `(pts_seconds, keyframe)` for the primary video stream of a container,
/// read through the safe packet API.
fn video_timeline(source: &MediaSource) -> Vec<(f64, bool)> {
    let interrupt = InterruptHandle::new();
    let mut input = MediaInput::open(source, &interrupt).expect("container opens");
    let streams = input.streams();
    let video = streams
        .iter()
        .find(|stream| stream.media_type == MediaType::Video)
        .expect("video stream exists");
    let video_index = video.stream_index;
    let time_base = video.time_base.expect("video time base");

    let mut timeline = Vec::new();
    while let Some(packet) = input.next_packet().unwrap() {
        let metadata = packet.metadata();
        if metadata.stream_index != video_index {
            continue;
        }
        let seconds = metadata
            .pts
            .or(metadata.dts)
            .and_then(|value| time_base.duration_of(value))
            .map(|duration| duration.as_secs_f64())
            .expect("fixture timestamps are present");
        timeline.push((seconds, metadata.keyframe));
    }
    timeline
}

#[test]
fn continuous_input_rotates_into_multiple_independent_segments() {
    let temp = tempfile::tempdir().unwrap();
    let mut events = Vec::new();
    let layout = layout_in(temp.path());
    let summary = RecordingSession::open(
        &fixture("session_av.mkv"),
        layout,
        RecorderConfig::new(camera()).with_segment_target(Duration::from_secs(5)),
    )
    .unwrap()
    .run(&mut |event| events.push(event))
    .unwrap();
    assert_eq!(summary.end_reason, RecordingEndReason::EndOfStream);

    // 30 s at a 5 s target with a 2 s GOP ⇒ several rotations.
    assert!(
        summary.finalized_segments >= 4,
        "expected >= 4 segments, got {}",
        summary.finalized_segments
    );

    let finals = final_files(temp.path());
    assert_eq!(
        finals.len(),
        summary.finalized_segments,
        "one published file per SegmentFinalized"
    );
    assert!(
        partial_files(temp.path()).is_empty(),
        "a successful session leaves no partials behind"
    );

    // Every published name is canonical, unique and non-partial.
    let mut identities = Vec::new();
    for path in &finals {
        let parsed = parse_segment_file_name(path.file_name().unwrap().to_str().unwrap())
            .unwrap_or_else(|error| panic!("{path:?} is not canonical: {error:?}"));
        assert!(!parsed.is_partial);
        identities.push((parsed.started_at, parsed.sequence));
    }
    identities.sort();
    let unique_count = identities.len();
    identities.dedup();
    assert_eq!(
        unique_count,
        identities.len(),
        "no two segments share a name"
    );

    // Every segment is independently probeable with the expected streams,
    // rotation targets are respected within one GOP, and the trailing
    // (EOF-closed) segment may be shorter.
    let backend = FfmpegBackend::new().unwrap();
    let mut total_bytes = 0_u64;
    for path in finals.iter() {
        let report = backend
            .probe(&MediaSource::File(path.clone()))
            .unwrap_or_else(|error| panic!("segment {path:?} must probe: {error:?}"));
        let video = report.video_stream().expect("segment keeps its video");
        assert_eq!(video.codec_name, "mpeg4");
        assert_eq!(video.width, Some(160));
        let audio = report
            .streams
            .iter()
            .find(|stream| stream.media_type == MediaType::Audio)
            .expect("audio copied alongside");
        assert_eq!(audio.codec_name, "aac");

        // Stream presence is checked through the probe; rotation timing is
        // NOT: with preserved source timestamps the Matroska duration
        // element reflects the absolute end timestamp (ADR-0004), so the
        // DTS-derived media_duration events are the honest measure.
        assert!(report.duration.is_some());
        total_bytes += std::fs::metadata(path).unwrap().len();
    }
    assert_eq!(
        total_bytes, summary.bytes_written,
        "summary bytes equal the sum of published sizes"
    );

    // Rotation timing: every rotated segment's media duration lands within
    // [target, target + GOP]; the EOF-closed trailing segment may be short.
    let mut media_durations: Vec<Option<Duration>> = events
        .iter()
        .filter_map(|event| match event {
            RecordingEvent::SegmentFinalized { media_duration, .. } => Some(*media_duration),
            _ => None,
        })
        .collect();
    assert_eq!(media_durations.len(), summary.finalized_segments);
    let trailing = media_durations.pop().expect("at least one segment");
    for (index, duration) in media_durations.into_iter().enumerate() {
        let duration = duration.expect("rotated segments know their media duration");
        assert!(
            duration >= Duration::from_secs_f64(4.8),
            "rotated segment {index} shorter than target: {duration:?}"
        );
        assert!(
            duration <= Duration::from_secs_f64(5.0) + SESSION_GOP + Duration::from_millis(300),
            "rotated segment {index} exceeds target+GOP: {duration:?}"
        );
    }
    assert!(trailing.is_some_and(|d| d > Duration::ZERO));

    // Event stream tells the same story and ends with RecordingStopped.
    assert!(matches!(
        events.first(),
        Some(RecordingEvent::RecordingStarted { .. })
    ));
    assert_eq!(finalized_paths(&events).len(), summary.finalized_segments);
    assert!(matches!(
        events.last(),
        Some(RecordingEvent::RecordingStopped { finalized_segments })
            if *finalized_segments == summary.finalized_segments
    ));
}

#[test]
fn segments_start_on_video_keyframes_and_the_boundary_belongs_to_the_new_segment() {
    let temp = tempfile::tempdir().unwrap();
    let mut events = Vec::new();
    let summary = RecordingSession::open(
        &fixture("session_av.mkv"),
        layout_in(temp.path()),
        RecorderConfig::new(camera()).with_segment_target(Duration::from_secs(5)),
    )
    .unwrap()
    .run(&mut |event| events.push(event))
    .unwrap();
    assert!(summary.finalized_segments >= 4);

    // Reference timeline straight from the source fixture.
    let source_timeline = video_timeline(&fixture("session_av.mkv"));

    // Per-segment timelines in publication order.
    let mut segment_timelines = Vec::new();
    for path in finalized_paths(&events) {
        let timeline = video_timeline(&MediaSource::File(path.clone()));
        assert!(!timeline.is_empty(), "{path:?} carries video");
        assert!(timeline[0].1, "{path:?} must START on a video keyframe");
        segment_timelines.push(timeline);
    }

    // Full-session continuity: concatenated segments reproduce the source
    // video stream exactly (the fixture begins on a keyframe, so nothing
    // was dropped after alignment).
    let recorded_packets: usize = segment_timelines.iter().map(Vec::len).sum();
    assert_eq!(
        recorded_packets,
        source_timeline.len(),
        "every source video packet is recorded exactly once"
    );

    // Consecutive segments never overlap, starts advance strictly, and
    // every start lands on a SOURCE keyframe instant — the boundary
    // keyframe belongs to the NEW segment because the previous segment's
    // last video packet precedes it.
    const EPS: f64 = 0.02;
    for window in segment_timelines.windows(2) {
        let previous_end = window[0].last().unwrap().0;
        let next_start = window[1][0].0;
        assert!(
            next_start > previous_end,
            "segments overlap: {previous_end} vs {next_start}"
        );
        assert!(
            source_timeline
                .iter()
                .any(|(seconds, keyframe)| *keyframe && (seconds - next_start).abs() <= EPS),
            "segment start {next_start} is not a source keyframe instant"
        );
    }
    assert!((segment_timelines[0][0].0 - source_timeline[0].0).abs() <= EPS);
}

#[test]
fn startup_alignment_discards_until_first_video_keyframe() {
    // Reference timeline: the fixture's second keyframe is where recording
    // must begin once the first GOP has been drained away.
    let source_timeline = video_timeline(&fixture("session_av.mkv"));
    let alignment_seconds = source_timeline
        .iter()
        .filter(|(_, keyframe)| *keyframe)
        .nth(1)
        .map(|(seconds, _)| *seconds)
        .expect("fixture carries multiple video keyframes");

    // Pre-drain the first keyframe plus a handful of mixed audio/inter
    // packets — stopping well before the next video keyframe (one GOP = 20
    // video frames away) — then hand the mid-stream input to the recorder.
    let interrupt = InterruptHandle::new();
    let mut input = MediaInput::open(&fixture("session_av.mkv"), &interrupt).unwrap();

    let mut drained = 0_u64;
    let mut passed_first_keyframe = false;
    while let Some(packet) = input.next_packet().unwrap() {
        let metadata = packet.metadata();
        drained += 1;
        if metadata.stream_index == 0 && metadata.keyframe {
            passed_first_keyframe = true;
            continue;
        }
        if passed_first_keyframe && drained >= 6 {
            break;
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let mut events = Vec::new();
    let session = RecordingSession::from_input(
        input,
        interrupt,
        layout_in(temp.path()),
        // A short target so the post-alignment remainder still rotates.
        RecorderConfig::new(camera())
            .with_segment_target(Duration::from_secs(5))
            .with_audio(AudioPolicy::CopyAll),
    )
    .expect("mid-stream input plans");
    let summary = session.run(&mut |event| events.push(event)).unwrap();
    assert_eq!(summary.end_reason, RecordingEndReason::EndOfStream);

    // Expected discard count, computed by a reference pass over a fresh
    // copy of the fixture: replay the drain, then count everything up to
    // (excluding) the next selected video keyframe — that is exactly what
    // the recorder must have discarded.
    let mut reference =
        MediaInput::open(&fixture("session_av.mkv"), &InterruptHandle::new()).unwrap();
    for _ in 0..drained {
        reference.next_packet().unwrap().expect("reference pass");
    }
    let mut expected_discarded = 0_u64;
    while let Some(packet) = reference.next_packet().unwrap() {
        let metadata = packet.metadata();
        if metadata.stream_index == 0 && metadata.keyframe {
            break;
        }
        expected_discarded += 1;
    }
    drop(reference);

    assert_eq!(
        summary.discarded_startup_packets, expected_discarded,
        "the recorder discards exactly up to the first post-drain video keyframe"
    );
    assert!(
        summary.finalized_segments >= 4,
        "expected >= 4 segments, got {} ({summary:?})",
        summary.finalized_segments
    );

    // The first published segment opens on THAT keyframe, with the source
    // timestamp preserved.
    let first_path = finalized_paths(&events).remove(0);
    let (first_seconds, first_keyframe) = video_timeline(&MediaSource::File(first_path.clone()))
        .into_iter()
        .next()
        .expect("segment carries video");
    assert!(first_keyframe, "segment opens on a keyframe");
    assert!(
        (first_seconds - alignment_seconds).abs() <= 0.02,
        "recording begins at the post-drain keyframe: {first_seconds} vs {alignment_seconds}"
    );

    // Leading audio was dropped: every audio packet in the first segment
    // sits at/after the opening video keyframe (small tolerance for muxer
    // interleaving).
    let interrupt = InterruptHandle::new();
    let mut recorded =
        MediaInput::open(&MediaSource::File(first_path.clone()), &interrupt).unwrap();
    let streams = recorded.streams();
    let video_index = streams[0].stream_index;
    let audio_tb = streams
        .iter()
        .find(|stream| stream.media_type == MediaType::Audio)
        .and_then(|stream| stream.time_base)
        .expect("audio kept alongside");
    while let Some(packet) = recorded.next_packet().unwrap() {
        let metadata = packet.metadata();
        if metadata.stream_index != video_index {
            let audio_seconds = metadata
                .pts
                .and_then(|value| audio_tb.duration_of(value))
                .map(|duration| duration.as_secs_f64())
                .expect("audio timestamps present");
            assert!(
                audio_seconds + 0.1 >= first_seconds,
                "audio predates the opening keyframe: {audio_seconds} < {first_seconds}"
            );
        }
    }
}

#[test]
fn graceful_stop_finalizes_last_meaningful_segment_without_waiting_for_a_keyframe() {
    let temp = tempfile::tempdir().unwrap();
    let session = RecordingSession::open(
        &fixture("session_av.mkv"),
        layout_in(temp.path()),
        RecorderConfig::new(camera()).with_segment_target(Duration::from_secs(5)),
    )
    .unwrap();
    let flag = session.stop_flag();

    let mut events = Vec::new();
    // Request the stop when the first rotation lands: the loop finishes
    // the current operation, then durably closes what it holds instead of
    // waiting up to another GOP for the next keyframe.
    let mut stop_requested = false;
    let summary = session
        .run(&mut |event| {
            if matches!(event, RecordingEvent::SegmentFinalized { .. }) && !stop_requested {
                flag.request();
                stop_requested = true;
            }
            events.push(event);
        })
        .unwrap();

    assert_eq!(summary.end_reason, RecordingEndReason::StopRequested);
    assert_eq!(
        summary.finalized_segments, 2,
        "one rotated segment plus the mid-GOP tail"
    );
    assert!(
        partial_files(temp.path()).is_empty(),
        "graceful stop leaves nothing unfinished"
    );

    // The tail is short (stop did NOT wait for a keyframe) yet still a
    // complete, independently decodable recording that starts on a
    // keyframe.
    let paths = finalized_paths(&events);
    let tail = paths.last().unwrap();
    let backend = FfmpegBackend::new().unwrap();
    let report = backend.probe(&MediaSource::File(tail.clone())).unwrap();
    assert_eq!(report.video_stream().unwrap().codec_name, "mpeg4");
    // Judge the tail by packet timestamps: the container duration element
    // is in absolute preserved-time units (see the continuous-input test).
    let timeline = video_timeline(&MediaSource::File(tail.clone()));
    assert!(!timeline.is_empty());
    assert!(timeline[0].1, "tail still opens on a keyframe");
    let tail_span = timeline.last().unwrap().0 - timeline[0].0;
    assert!(
        tail_span < SESSION_GOP.as_secs_f64(),
        "tail waited for a keyframe boundary: span {tail_span}"
    );
}

#[test]
fn reordered_source_records_video_from_stream_index_one() {
    let temp = tempfile::tempdir().unwrap();
    let layout = layout_in(temp.path());
    let session = RecordingSession::open(
        &fixture("reordered_av.mkv"),
        layout,
        RecorderConfig::new(camera()).with_segment_target(Duration::from_secs(1)),
    )
    .unwrap();

    // The planner picked the VIDEO stream (container index 1), not index
    // 0, and mapped it first.
    assert_eq!(session.primary_video_stream_index(), 1);
    let selection = session.selected_streams();
    assert_eq!(selection[0].stream_index, 1);
    assert_eq!(selection[0].media_type, MediaType::Video);

    let mut events = Vec::new();
    let summary = session.run(&mut |event| events.push(event)).unwrap();
    assert_eq!(summary.end_reason, RecordingEndReason::EndOfStream);
    assert!(
        summary.finalized_segments >= 2,
        "rotation works off stream 1"
    );

    for path in finalized_paths(&events) {
        let timeline = video_timeline(&MediaSource::File(path.clone()));
        assert!(!timeline.is_empty());
        assert!(
            timeline[0].1,
            "{path:?} opens on the (reindexed) video keyframe"
        );

        let backend = FfmpegBackend::new().unwrap();
        let report = backend.probe(&MediaSource::File(path.clone())).unwrap();
        assert_eq!(report.video_stream().unwrap().codec_name, "mpeg4");
        assert!(
            report
                .streams
                .iter()
                .any(|stream| stream.media_type == MediaType::Audio),
            "audio copied despite being input stream 0"
        );
    }
    assert!(partial_files(temp.path()).is_empty());
}

#[test]
fn audio_only_source_is_rejected_before_any_recording() {
    let temp = tempfile::tempdir().unwrap();
    match RecordingSession::open(
        &fixture("audio_only.mkv"),
        layout_in(temp.path()),
        RecorderConfig::new(camera()),
    ) {
        Err(RecordingError::NoVideoStream { stream_count }) => {
            assert_eq!(stream_count, 1);
        }
        Err(other) => panic!("expected NoVideoStream, got {other:?}"),
        Ok(_) => panic!("audio-only source must be rejected"),
    }
    assert!(all_files(temp.path()).is_empty(), "nothing was claimed");
}

#[test]
fn forced_cancellation_abandons_partials_and_never_publishes() {
    let interrupt = InterruptHandle::new();
    let temp = tempfile::tempdir().unwrap();
    let input = MediaInput::open(&fixture("session_av.mkv"), &interrupt).unwrap();
    let session = RecordingSession::from_input(
        input,
        interrupt.clone(),
        layout_in(temp.path()),
        RecorderConfig::new(camera()).with_audio(AudioPolicy::Exclude),
    )
    .unwrap();

    // Forced cancellation once the session is live: blocking reads abort,
    // and the active segment must be abandoned — never finalized/published.
    interrupt.cancel();
    let outcome = session.run(&mut |_| {});
    match &outcome {
        Err(RecordingError::Media(media)) => assert!(
            media.is_interrupted(),
            "expected interruption, got {media:?}"
        ),
        other => panic!("expected interrupted error, got {other:?}"),
    }

    // Whatever partial got claimed stays recoverable — and NOTHING looks
    // like a completed recording.
    for path in all_files(temp.path()) {
        let parsed = parse_segment_file_name(path.file_name().unwrap().to_str().unwrap())
            .unwrap_or_else(|error| panic!("unexpected non-segment file {path:?}: {error:?}"));
        assert!(
            parsed.is_partial,
            "forced cancellation must never publish: {path:?}"
        );
    }
}

#[test]
fn stop_before_the_first_keyframe_creates_no_segments_at_all() {
    let temp = tempfile::tempdir().unwrap();
    let session = RecordingSession::open(
        &fixture("session_av.mkv"),
        layout_in(temp.path()),
        RecorderConfig::new(camera()),
    )
    .unwrap();
    session.stop_flag().request(); // stop wins the very first loop check

    let mut events = Vec::new();
    let summary = session.run(&mut |event| events.push(event)).unwrap();
    assert_eq!(summary.end_reason, RecordingEndReason::StopRequested);
    assert_eq!(summary.finalized_segments, 0);
    assert!(matches!(
        events.last(),
        Some(RecordingEvent::RecordingStopped { finalized_segments }) if *finalized_segments == 0
    ));
    assert!(
        all_files(temp.path()).is_empty(),
        "no slot was ever claimed"
    );
}
