//! Owned stream metadata used to create packet-copy muxers away from the
//! demuxing thread.
//!
//! A shared ingest owns the live [`MediaInput`](crate::MediaInput). Consumers
//! such as recording and live-fragment writers still need the source codec
//! parameters when they open a new output segment. `MediaStreamTemplate`
//! snapshots those parameters into independent FFmpeg allocations so consumers
//! do not need access to the source `AVFormatContext`.

use nian_domain::MediaStreamInfo;
use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

use crate::MediaInput;
use crate::error_util::{ErrorKind, error_for};

struct TemplateStream {
    info: MediaStreamInfo,
    codec_parameters: *mut sys::AVCodecParameters,
}

// SAFETY: this allocation is exclusively owned by the TemplateStream and is
// never mutated concurrently. Moving ownership between threads is equivalent
// to moving any other uniquely owned FFmpeg allocation. No Sync impl is
// provided, so the raw parameter object is never concurrently accessed.
unsafe impl Send for TemplateStream {}

impl Drop for TemplateStream {
    fn drop(&mut self) {
        // SAFETY: codec_parameters was returned by avcodec_parameters_alloc
        // and remains exclusively owned by this TemplateStream.
        unsafe { sys::avcodec_parameters_free(&mut self.codec_parameters) };
    }
}

/// Reusable, owned snapshot of an opened input's stream descriptions.
///
/// The value is `Send` but not `Sync`: a consumer may move its snapshot to a
/// dedicated writer thread. Use [`Self::try_clone`] when multiple consumers
/// need independent snapshots.
pub struct MediaStreamTemplate {
    streams: Vec<TemplateStream>,
}

impl MediaStreamTemplate {
    /// Copies every stream's codec parameters from an opened media input.
    pub fn capture(input: &MediaInput) -> Result<Self, MediaError> {
        let infos = input.streams();
        if infos.is_empty() {
            return Err(MediaError::ReadFailed {
                message: "media source exposes no streams to snapshot".to_owned(),
            });
        }

        let mut streams = Vec::with_capacity(infos.len());
        for info in infos {
            let source = input
                .codec_parameters(info.stream_index as usize)
                .ok_or_else(|| MediaError::ReadFailed {
                    message: "media stream codec parameters are unavailable".to_owned(),
                })?;
            streams.push(TemplateStream {
                codec_parameters: copy_codec_parameters(source)?,
                info,
            });
        }
        Ok(Self { streams })
    }

    /// Returns the safe stream metadata captured from the source.
    pub fn streams(&self) -> Vec<MediaStreamInfo> {
        self.streams
            .iter()
            .map(|stream| stream.info.clone())
            .collect()
    }

    /// Creates a deep copy suitable for another independent consumer thread.
    ///
    /// Codec parameter metadata/extradata is duplicated by FFmpeg; compressed
    /// media packets themselves are not part of this template.
    pub fn try_clone(&self) -> Result<Self, MediaError> {
        let mut streams = Vec::with_capacity(self.streams.len());
        for stream in &self.streams {
            streams.push(TemplateStream {
                info: stream.info.clone(),
                codec_parameters: copy_codec_parameters(stream.codec_parameters)?,
            });
        }
        Ok(Self { streams })
    }

    pub(crate) fn codec_parameters(&self, index: usize) -> Option<*const sys::AVCodecParameters> {
        self.streams
            .iter()
            .find(|stream| stream.info.stream_index as usize == index)
            .map(|stream| stream.codec_parameters.cast_const())
    }
}

impl std::fmt::Debug for MediaStreamTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaStreamTemplate")
            .field("streams", &self.streams())
            .finish()
    }
}

fn copy_codec_parameters(
    source: *const sys::AVCodecParameters,
) -> Result<*mut sys::AVCodecParameters, MediaError> {
    // SAFETY: avcodec_parameters_alloc has no preconditions; NULL is checked.
    let destination = unsafe { sys::avcodec_parameters_alloc() };
    if destination.is_null() {
        return Err(MediaError::ReadFailed {
            message: "out of memory snapshotting stream parameters".to_owned(),
        });
    }

    // SAFETY: destination is a fresh parameter allocation and source belongs
    // to a live input/template for the duration of this immutable borrow.
    let code = unsafe { sys::avcodec_parameters_copy(destination, source) };
    if code < 0 {
        let mut failed = destination;
        // SAFETY: failed is still our allocation from above.
        unsafe { sys::avcodec_parameters_free(&mut failed) };
        return Err(error_for(
            code,
            "snapshot stream parameters",
            ErrorKind::Read,
            None,
        ));
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InterruptHandle, MatroskaMuxer};

    fn fixture_path() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.extend(["tests", "fixtures", "sample.mkv"]);
        path
    }

    #[test]
    fn template_outlives_input_and_opens_packet_copy_muxer() {
        fn assert_send<T: Send>() {}
        assert_send::<MediaStreamTemplate>();

        let input_interrupt = InterruptHandle::new();
        let mut input = MediaInput::open(
            &nian_media::MediaSource::file(fixture_path()),
            &input_interrupt,
        )
        .unwrap();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let template_copy = template.try_clone().unwrap();
        let expected_streams = input.streams();
        let mut packets = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            packets.push(packet);
        }
        drop(input);
        drop(template);

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("template-recording.mkv");
        let output_interrupt = InterruptHandle::new();
        let mut muxer = MatroskaMuxer::create_recording_segment_from_template_with_selection(
            &template_copy,
            &output,
            &output_interrupt,
            |_| true,
        )
        .unwrap();
        for packet in &packets {
            muxer.write_packet(packet).unwrap();
        }
        muxer.finalize().unwrap();

        let verify_interrupt = InterruptHandle::new();
        let verify =
            MediaInput::open(&nian_media::MediaSource::file(output), &verify_interrupt).unwrap();
        assert_eq!(verify.streams().len(), expected_streams.len());
        assert_eq!(template_copy.streams(), expected_streams);
    }
}
