//! Optional software-decoding consumer of a shared compressed packet stream.
//!
//! This decoder never opens a source: callers supply an independent stream
//! template plus `av_packet_ref`-backed packets from the shared ingest. Only
//! 32×18 luma thumbnails cross the FFmpeg boundary, not decoded full frames.

use nian_domain::MediaType;
use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

use crate::error_util::{ErrorKind, error_for};
use crate::{FfmpegPacket, MediaStreamTemplate};

pub const LUMA_WIDTH: usize = 32;
pub const LUMA_HEIGHT: usize = 18;
const LUMA_PIXELS: usize = LUMA_WIDTH * LUMA_HEIGHT;
const MAX_DECODE_WIDTH: usize = 1920;
const MAX_DECODE_HEIGHT: usize = 1080;
const MAX_FRAMES_PER_PACKET: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LumaThumbnail {
    pixels: [u8; LUMA_PIXELS],
}

impl LumaThumbnail {
    pub fn from_pixels(pixels: [u8; LUMA_PIXELS]) -> Self {
        Self { pixels }
    }

    pub fn pixels(&self) -> &[u8; LUMA_PIXELS] {
        &self.pixels
    }
}

/// One decoder per subscriber/generation. The owner must not share it between
/// threads. Drop frees the scratch frame before the codec context.
pub struct LumaDecoder {
    context: *mut sys::AVCodecContext,
    frame: *mut sys::AVFrame,
    video_stream: u32,
    sample_every: u64,
    decoded_frames: u64,
}

impl LumaDecoder {
    pub fn open(template: &MediaStreamTemplate) -> Result<Self, MediaError> {
        let stream = template
            .streams()
            .into_iter()
            .find(|stream| {
                stream.media_type == MediaType::Video
                    && matches!(stream.codec_name.as_str(), "h264" | "hevc")
            })
            .ok_or_else(|| MediaError::OpenFailed {
                message: "motion decoder requires an H.264 or HEVC video stream".to_owned(),
            })?;
        let width = stream.width.unwrap_or(0) as usize;
        let height = stream.height.unwrap_or(0) as usize;
        if width == 0 || height == 0 || width > MAX_DECODE_WIDTH || height > MAX_DECODE_HEIGHT {
            return Err(MediaError::OpenFailed {
                message: "motion decoder rejects missing or oversized video dimensions".to_owned(),
            });
        }
        let parameters = template
            .codec_parameters(stream.stream_index as usize)
            .ok_or_else(|| MediaError::OpenFailed {
                message: "motion decoder stream parameters are unavailable".to_owned(),
            })?;
        // SAFETY: parameters is owned by the caller's live template and only
        // read here. The decoder lookup returns a library-owned static pointer.
        let codec = unsafe { sys::avcodec_find_decoder((*parameters).codec_id) };
        if codec.is_null() {
            return Err(MediaError::OpenFailed {
                message: "motion decoder codec is unavailable".to_owned(),
            });
        }
        // SAFETY: codec was returned by FFmpeg, and allocation is NULL-checked.
        let mut context = unsafe { sys::avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err(MediaError::OpenFailed {
                message: "motion decoder allocation failed".to_owned(),
            });
        }
        // SAFETY: context is freshly allocated, and parameters remains alive
        // during this call; FFmpeg duplicates its extradata into context.
        let copied = unsafe { sys::avcodec_parameters_to_context(context, parameters) };
        if copied < 0 {
            // SAFETY: context is our sole allocation, freed exactly once.
            unsafe { sys::avcodec_free_context(&mut context) };
            return Err(error_for(
                copied,
                "copy motion decoder parameters",
                ErrorKind::Open,
                None,
            ));
        }
        // SAFETY: codec matches the allocated context; no options dictionary.
        let opened = unsafe { sys::avcodec_open2(context, codec, std::ptr::null_mut()) };
        if opened < 0 {
            // SAFETY: context is our sole allocation, freed exactly once.
            unsafe { sys::avcodec_free_context(&mut context) };
            return Err(error_for(
                opened,
                "open motion decoder",
                ErrorKind::Open,
                None,
            ));
        }
        // SAFETY: av_frame_alloc takes no arguments, NULL is checked.
        let frame = unsafe { sys::av_frame_alloc() };
        if frame.is_null() {
            // SAFETY: context remains ours and no frame was allocated.
            unsafe { sys::avcodec_free_context(&mut context) };
            return Err(MediaError::OpenFailed {
                message: "motion decoder frame allocation failed".to_owned(),
            });
        }
        let sample_every = stream
            .frame_rate
            .and_then(|rate| {
                (rate.den > 0 && rate.num > 0).then(|| {
                    ((f64::from(rate.num) / f64::from(rate.den) / 5.0).round() as u64).clamp(1, 12)
                })
            })
            .unwrap_or(5);
        Ok(Self {
            context,
            frame,
            video_stream: stream.stream_index,
            sample_every,
            decoded_frames: 0,
        })
    }

    /// Decode without copying the source packet or interfering with recorder
    /// ownership. Return only sampled luma thumbnails (nominally 5 fps).
    pub fn push(&mut self, packet: &FfmpegPacket) -> Result<Vec<LumaThumbnail>, MediaError> {
        if packet.metadata().stream_index != self.video_stream {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        // SAFETY: context is open and packet stays alive throughout the call.
        // libavcodec does not consume the caller's AVPacket reference.
        let mut code = unsafe { sys::avcodec_send_packet(self.context, packet.as_raw()) };
        if code == sys::NIAN_AVERROR_EAGAIN {
            self.drain(&mut output)?;
            // SAFETY: draining returned EAGAIN; retry the same packet once.
            code = unsafe { sys::avcodec_send_packet(self.context, packet.as_raw()) };
        }
        if code < 0 {
            return Err(error_for(
                code,
                "submit motion decoder packet",
                ErrorKind::Read,
                None,
            ));
        }
        self.drain(&mut output)?;
        Ok(output)
    }

    fn drain(&mut self, output: &mut Vec<LumaThumbnail>) -> Result<(), MediaError> {
        for _ in 0..MAX_FRAMES_PER_PACKET {
            // SAFETY: frame is an allocated, reusable scratch frame, context
            // is open; FFmpeg unreferences previous frame on each receive.
            let code = unsafe { sys::avcodec_receive_frame(self.context, self.frame) };
            if code == sys::NIAN_AVERROR_EAGAIN || code == sys::NIAN_AVERROR_EOF {
                return Ok(());
            }
            if code < 0 {
                return Err(error_for(
                    code,
                    "receive motion decoder frame",
                    ErrorKind::Read,
                    None,
                ));
            }
            self.decoded_frames = self.decoded_frames.saturating_add(1);
            if self.decoded_frames.is_multiple_of(self.sample_every) {
                output.push(self.thumbnail()?);
            }
            // SAFETY: scratch frame owns this frame's refs and is reused by the
            // next receive; this eagerly releases full-resolution buffers.
            unsafe { sys::av_frame_unref(self.frame) };
        }
        Err(MediaError::ReadFailed {
            message: "motion decoder produced excessive frames from one packet".to_owned(),
        })
    }

    fn thumbnail(&self) -> Result<LumaThumbnail, MediaError> {
        // SAFETY: frame contains a successfully returned, owned FFmpeg frame.
        // The signed stride and plane pointer are valid by the AVFrame ABI;
        // pixel-format and dimension bounds are checked before reading.
        let frame = unsafe { &*self.frame };
        let width = usize::try_from(frame.width).unwrap_or(0);
        let height = usize::try_from(frame.height).unwrap_or(0);
        let format = frame.format;
        let supported_format = [
            sys::AV_PIX_FMT_YUV420P,
            sys::AV_PIX_FMT_YUVJ420P,
            sys::AV_PIX_FMT_NV12,
            sys::AV_PIX_FMT_GRAY8,
        ]
        .iter()
        .any(|value| i64::from(format) == i64::from(*value));
        let stride = frame.linesize[0];
        if !supported_format
            || width == 0
            || height == 0
            || width > MAX_DECODE_WIDTH
            || height > MAX_DECODE_HEIGHT
            || frame.data[0].is_null()
            || (stride as i64).unsigned_abs() < width as u64
        {
            return Err(MediaError::ReadFailed {
                message: "motion decoder encountered unsupported or invalid luma frame".to_owned(),
            });
        }
        let mut pixels = [0; LUMA_PIXELS];
        for (index, target) in pixels.iter_mut().enumerate() {
            let x = ((index % LUMA_WIDTH) * width + width / 2) / LUMA_WIDTH;
            let y = ((index / LUMA_WIDTH) * height + height / 2) / LUMA_HEIGHT;
            let x = x.min(width - 1);
            let y = y.min(height - 1);
            let offset = (stride as isize) * (y as isize) + (x as isize);
            // SAFETY: x < width <= abs(stride), y < height, and AVFrame
            // guarantees each luma row is backed by its referenced buffer.
            // Negative stride is legal and data[0] points at the first row.
            *target = unsafe { *frame.data[0].offset(offset) };
        }
        Ok(LumaThumbnail { pixels })
    }
}

impl Drop for LumaDecoder {
    fn drop(&mut self) {
        // SAFETY: both allocations belong solely to this decoder. Releasing
        // the frame first drops its references before freeing the codec.
        unsafe {
            sys::av_frame_free(&mut self.frame);
            sys::avcodec_free_context(&mut self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InterruptHandle, MediaInput};

    #[test]
    fn shared_packet_decoder_accepts_real_hevc_without_opening_another_source() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hevc_g711_alaw.mkv");
        let mut input = MediaInput::open(
            &nian_media::MediaSource::file(path),
            &InterruptHandle::new(),
        )
        .unwrap();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let mut decoder = LumaDecoder::open(&template).unwrap();
        let mut thumbnails = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            thumbnails.extend(decoder.push(&packet).unwrap());
        }
        assert!(
            !thumbnails.is_empty(),
            "the HEVC decoder must produce actual frames"
        );
        assert!(
            thumbnails.len() < 200,
            "motion sampling must remain bounded"
        );
        assert!(
            thumbnails
                .iter()
                .any(|frame| frame.pixels().iter().any(|pixel| *pixel > 0))
        );
    }

    #[test]
    fn shared_packet_decoder_emits_bounded_luma_thumbnails_from_real_h264_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/playback_h264.mkv");
        let mut input = MediaInput::open(
            &nian_media::MediaSource::file(path),
            &InterruptHandle::new(),
        )
        .unwrap();
        let template = MediaStreamTemplate::capture(&input).unwrap();
        let mut decoder = LumaDecoder::open(&template).unwrap();
        let mut thumbnails = Vec::new();
        while let Some(packet) = input.next_packet().unwrap() {
            thumbnails.extend(decoder.push(&packet).unwrap());
        }
        assert!(
            !thumbnails.is_empty(),
            "fixture must decode real video frames"
        );
        assert!(thumbnails.len() < 200, "sampling must remain bounded");
        assert!(
            thumbnails
                .iter()
                .any(|frame| frame.pixels().iter().any(|pixel| *pixel > 0))
        );
    }
}
