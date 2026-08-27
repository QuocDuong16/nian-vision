//! Conservative startup recovery for `.partial.mkv` leftovers (M3 §11).
//!
//! The recovery contract, in order:
//!
//! 1. [`nian_storage::scan_camera_partials`] classifies leftovers at
//!    byte/name level (cheap, filesystem facts only);
//! 2. this module performs the MEDIA-level proof and salvage: open the
//!    partial with the real demuxer; if it carries a usable video stream,
//!    discard everything until the first selected video keyframe (the same
//!    alignment rule as live recording), stream-copy every readable packet
//!    into a NEW exclusively-claimed recovery output, finalize it durably,
//!    publish it no-replace — and only then remove the original partial;
//! 3. anything that cannot be PROVEN recoverable keeps its partial file in
//!    place untouched: no invented recordings, no blind renames to final.
//!
//! A truncated-but-readable partial never becomes "the final it was named
//! after": its recovered content is a distinct recording slot claimed like
//! any other segment (name anchored when recovery runs), published with the
//! ordinary no-replace machinery, so an existing final can never be
//! overwritten by recovery. If that publication is refused, the recovered
//! output stays a `.partial.mkv` and the ORIGINAL is kept too.
//!
//! # Failure containment
//!
//! One partial's recovery failure never aborts other files: caller-visible
//! failures are collected per-file, and the source file always survives any
//! failed attempt.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use nian_domain::{CameraId, MediaRational};
use nian_media_ffmpeg::{InterruptHandle, MatroskaMuxer, MediaInput};
use nian_storage::paths::publish_no_replace;
use nian_storage::{
    PartialDisposition, PartialFile, RecordingsLayout, StorageError, scan_camera_partials,
};

/// Result of recovering one partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No partials existed — nothing to do (the common case).
    NothingToDo,

    /// The partial was empty/header-only, was refused by the demuxer, or
    /// yielded no video packets: kept in place untouched (quarantined) and
    /// reported. Never deleted, never renamed to final.
    KeptUnrecoverable {
        /// The quarantined partial.
        partial_path: PathBuf,
        /// Why recovery was refused (human-safe reason; contains no secrets,
        /// at most paths that are already operator-known).
        reason: String,
    },

    /// Readable content was salvaged into a fresh claimed segment,
    /// finalized durably and published no-replace. The ORIGINAL partial was
    /// removed only after that publication succeeded.
    Recovered {
        /// The newly published recording.
        final_path: PathBuf,
        /// Media duration derived from packet timestamps when both ends of
        /// the copied span carried timestamps (`None` otherwise — never
        /// guessed). Measured on the RECOVERED content, which starts at its
        /// own first keyframe.
        media_duration: Option<Duration>,
        /// Size of the published file.
        size_bytes: u64,
        /// Whether the salvage derived from a finalized-but-unpublished
        /// leftover (class C) rather than a truncated crash partial.
        from_finalized_leftover: bool,
    },
}

/// One attempted recovery with its error, when the attempt failed.
#[derive(Debug)]
pub struct RecoveryFailure {
    /// The partial whose recovery failed (kept untouched).
    pub partial_path: PathBuf,
    /// Why it failed.
    pub error: RecoveryError,
}

/// Recovery-specific failures.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The scan or claiming the recovery output failed (filesystem
    /// trouble). Source files remain untouched.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// The demuxer could not prove the partial is readable media (failed
    /// open, no video stream, or trailer/flush failed on the salvage
    /// output).
    #[error("partial is not provably recoverable media: {message}")]
    Unreadable {
        /// Human-safe backend description (secret-free by media-layer
        /// contract).
        message: String,
    },

    /// Publication of the recovered output was refused because the
    /// destination already exists. Neither the recovered output nor the
    /// original partial is removed; a later pass may still salvage under a
    /// different claim.
    #[error("recovered recording destination already exists")]
    DestinationExists,
}

/// Recovers all classifiable partials of one camera.
///
/// Conservative per-file containment: one failure does not abort other
/// files' recovery. Returns outcomes in deterministic (scan) order plus the
/// failures encountered. Nothing outside the canonical layout tree is ever
/// touched.
pub fn recover_camera_partials(
    layout: &RecordingsLayout,
    camera: &CameraId,
) -> (Vec<RecoveryOutcome>, Vec<RecoveryFailure>) {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();

    let partials = match scan_camera_partials(layout, camera) {
        Ok(partials) => partials,
        Err(error) => {
            failures.push(RecoveryFailure {
                // No specific path known; point at the camera root so the
                // report stays actionable without inventing a filename.
                partial_path: layout.camera_dir(camera),
                error: error.into(),
            });
            return (outcomes, failures);
        }
    };

    if partials.is_empty() {
        outcomes.push(RecoveryOutcome::NothingToDo);
        return (outcomes, failures);
    }

    for partial in partials {
        match recover_one(layout, camera, &partial) {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => failures.push(RecoveryFailure {
                partial_path: partial.partial_path.clone(),
                error,
            }),
        }
    }
    (outcomes, failures)
}

/// Recovers ONE partial according to its classification.
fn recover_one(
    layout: &RecordingsLayout,
    camera: &CameraId,
    partial: &PartialFile,
) -> Result<RecoveryOutcome, RecoveryError> {
    match &partial.disposition {
        // Cheap classification proved there cannot be media here. Keep the
        // file (reporting beats deleting); cleanup policy belongs to M4.
        PartialDisposition::EmptyOrHeaderOnly => Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "empty or header-only, nothing to salvage".to_owned(),
        }),

        // Both media classes go through the SAME proven pipeline: demux →
        // keyframe-aligned packet copy → durable finalize → no-replace
        // publish → remove original LAST.
        PartialDisposition::RecoverableMedia { .. }
        | PartialDisposition::FinalizedButUnpublished { .. } => {
            salvage_media(layout, camera, partial)
        }
    }
}

/// The demux→copy→finalize→publish→cleanup pipeline shared by both media
/// classes.
fn salvage_media(
    layout: &RecordingsLayout,
    camera: &CameraId,
    partial: &PartialFile,
) -> Result<RecoveryOutcome, RecoveryError> {
    let from_finalized = matches!(
        partial.disposition,
        PartialDisposition::FinalizedButUnpublished { .. }
    );

    // Media-level proof begins here: a private interrupt handle scoped only
    // to this recovery; no shared cancellation leaks into or out of it. The
    // open budget also bounds stream analysis (same handle).
    let interrupt = InterruptHandle::new();
    let _open_budget = interrupt.scoped_deadline(Duration::from_secs(15));
    let source = partial_source(&partial.partial_path);
    let mut input =
        MediaInput::open(&source, &interrupt).map_err(|error| RecoveryError::Unreadable {
            message: error.to_string(),
        })?;
    drop(_open_budget);

    // Reuse the recorder's planning rules verbatim: explicit primary video
    // (first video stream by container order), validated positive time
    // base — never guessed.
    let streams = input.streams();
    let video = streams
        .iter()
        .find(|stream| stream.media_type == nian_domain::MediaType::Video)
        .ok_or(RecoveryError::Unreadable {
            message: "no video stream inside the partial".to_owned(),
        })?;
    let time_base: MediaRational = video
        .time_base
        .filter(|tb| tb.num > 0 && tb.den > 0)
        .ok_or(RecoveryError::Unreadable {
            message: "video stream has no usable time base".to_owned(),
        })?;
    let video_index = video.stream_index;

    // Fresh EXCLUSIVE claim for the recovered output. Anchored at "now" so
    // the name reflects when recovery ran; the crashed segment's true start
    // time cannot be trusted across clock discontinuities.
    let claim_started = Local::now().naive_local();
    let claim = layout.claim_segment(camera, claim_started)?;

    // Selection mirrors live recording exactly: every discovered stream is
    // copied that the muxer can map (the partial's container declares what
    // exists); unselected packet types are skipped deliberately by the
    // muxer's mapping.
    let selection = streams.clone();
    let mut muxer = MatroskaMuxer::create_with_selection(
        &mut input,
        claim.partial_path(),
        &interrupt,
        |info| {
            selection
                .iter()
                .any(|s| s.stream_index == info.stream_index)
        },
    )
    .map_err(|error| RecoveryError::Unreadable {
        message: format!("salvage output could not be opened: {error}"),
    })?;

    // Packet-copy loop: startup alignment discards until the first selected
    // VIDEO keyframe (identical invariant to M2 live recording), then every
    // READABLE packet is copied until clean EOF, demux failure (expected —
    // truncation is why we are here) or a write failure on the new output.
    let mut aligned = false;
    let mut start_media: Option<i64> = None;
    let mut last_media: Option<i64> = None;
    let mut video_packets: u64 = 0;

    loop {
        match input.next_packet() {
            Ok(Some(packet)) => {
                let metadata = packet.metadata();
                let is_video = metadata.stream_index == video_index;
                if !aligned {
                    if is_video && metadata.keyframe {
                        aligned = true;
                    } else {
                        continue;
                    }
                }
                if muxer.write_packet(&packet).is_err() {
                    // Output-write trouble while salvaging an ALREADY-broken
                    // file: stop copying; whatever landed gets finalized.
                    break;
                }
                if is_video {
                    // Commit bookkeeping strictly AFTER a successful write
                    // (M2 transactional-commit rule applies to recovery too).
                    if let Some(timestamp) = metadata.dts.or(metadata.pts) {
                        start_media.get_or_insert(timestamp);
                        last_media = Some(timestamp);
                    }
                    video_packets += 1;
                }
            }
            Ok(None) => break, // clean EOF: everything readable was copied
            Err(_) => break,   // truncation mid-partial: the expected shape
        }
    }

    // Tiny-output guard mirrors M2 §12: a "recovery" containing zero video
    // packets is not a recording. Keep BOTH files; nothing is published,
    // nothing removed.
    if video_packets == 0 {
        drop(muxer);
        return Ok(RecoveryOutcome::KeptUnrecoverable {
            partial_path: partial.partial_path.clone(),
            reason: "demuxed but no video packets survived alignment".to_owned(),
        });
    }

    // Durable finalize: trailer + flush/close must BOTH succeed before any
    // publication (M2 finalization order).
    muxer
        .finalize()
        .map_err(|error| RecoveryError::Unreadable {
            message: format!("salvaged output failed to finalize: {error}"),
        })?;

    let size_bytes = std::fs::metadata(claim.partial_path())
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    let media_duration = match (start_media, last_media) {
        (Some(start), Some(last)) => time_base.duration_of(last.saturating_sub(start).max(0)),
        _ => None,
    };

    // Atomic no-replace publication of the recovered output. On refusal
    // (destination taken) neither file disappears: the recovered bytes stay
    // under their own `.partial` name for a later pass with a fresh claim.
    publish_no_replace(claim.partial_path(), claim.final_path()).map_err(|error| match error {
        StorageError::DestinationExists { .. } => RecoveryError::DestinationExists,
        other => RecoveryError::Storage(other),
    })?;

    // ONLY NOW may the original disappear: the recovered final is durably
    // on disk under its own canonical name. A failed removal is non-fatal —
    // the original simply re-classifies as `FinalizedButUnpublished` next
    // scan and refuses republish via DestinationExists semantics safely.
    let _ = std::fs::remove_file(&partial.partial_path);

    Ok(RecoveryOutcome::Recovered {
        final_path: claim.final_path().to_path_buf(),
        media_duration,
        size_bytes,
        from_finalized_leftover: from_finalized,
    })
}

/// Builds the demuxer source for a partial path (local file by definition —
/// partials only ever exist inside the storage tree).
fn partial_source(path: &std::path::Path) -> nian_media::MediaSource {
    nian_media::MediaSource::File(path.to_path_buf())
}
