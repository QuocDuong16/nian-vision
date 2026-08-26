//! Safe ownership of an FFmpeg input (`AVFormatContext`) for reading.

use std::ffi::CString;
use std::time::Duration;

use nian_domain::{MediaRational, MediaStreamInfo, MediaType};
use nian_ffmpeg_sys as sys;
use nian_media::{MediaError, MediaSource};

use crate::error_util::{ErrorKind, error_for};
use crate::interrupt::InterruptHandle;
use crate::packet::FfmpegPacket;

/// An opened media source: local container file or live RTSP stream.
///
/// Not `Send`/`Sync` (raw FFI context); use it from the thread that opened
/// it, which matches the worker's single-threaded media loop.
pub struct MediaInput {
    context: *mut sys::AVFormatContext,
    packet: *mut sys::AVPacket,
    interrupt: InterruptHandle,
}

impl MediaInput {
    /// Opens `source` and reads the container header.
    ///
    /// `interrupt` is consulted by FFmpeg during every blocking operation;
    /// use it for connect timeouts and cancellation.
    pub fn open(source: &MediaSource, interrupt: &InterruptHandle) -> Result<Self, MediaError> {
        crate::version::global_init()?;

        let (url, is_rtsp) = match source {
            MediaSource::File(path) => {
                let path_text = path.to_string_lossy().into_owned();
                (
                    CString::new(path_text).map_err(|_| MediaError::OpenFailed {
                        message: "path contains interior NUL bytes".to_owned(),
                    })?,
                    false,
                )
            }
            MediaSource::Rtsp { url } => (
                CString::new(url.expose().to_owned()).map_err(|_| MediaError::OpenFailed {
                    message: "rtsp url contains interior NUL bytes".to_owned(),
                })?,
                true,
            ),
        };

        // Allocate the context ourselves so the interrupt callback is active
        // during avformat_open_input (connect + handshake happen there).
        let mut context: *mut sys::AVFormatContext =
            // SAFETY: avformat_alloc_context has no preconditions; NULL is
            // checked immediately below.
            unsafe { sys::avformat_alloc_context() };
        if context.is_null() {
            return Err(MediaError::OpenFailed {
                message: "out of memory allocating media context".to_owned(),
            });
        }
        interrupt.install_on(context);

        let mut options: *mut sys::AVDictionary = std::ptr::null_mut();
        if is_rtsp {
            // TCP interleaving is the reliable mode for recording; UDP loses
            // packets on Wi-Fi and punches holes we do not need.
            set_option(&mut options, "rtsp_transport", "tcp");
        }

        // SAFETY: `context` is a valid context pointer (avformat_open_input
        // takes ownership even on failure and nulls it), `url` is a valid
        // NUL-terminated C string, no custom demuxer, options dict is ours.
        let code = unsafe {
            sys::avformat_open_input(&mut context, url.as_ptr(), std::ptr::null(), &mut options)
        };
        // SAFETY: options is either NULL or still owned by us after the call.
        unsafe { sys::av_dict_free(&mut options) };
        if code < 0 {
            return Err(error_for(
                code,
                "open media source",
                ErrorKind::Open,
                interrupt.state().should_abort(),
            ));
        }

        // SAFETY: context is a successfully opened context.
        let code = unsafe { sys::avformat_find_stream_info(context, std::ptr::null_mut()) };
        if code < 0 {
            // avformat_close_input frees the context on all paths below.
            // SAFETY: context is a valid opened context.
            unsafe { sys::avformat_close_input(&mut context) };
            return Err(error_for(
                code,
                "analyze media streams",
                ErrorKind::Open,
                interrupt.state().should_abort(),
            ));
        }

        // SAFETY: av_packet_alloc has no preconditions; NULL checked below.
        let packet = unsafe { sys::av_packet_alloc() };
        if packet.is_null() {
            // SAFETY: context is a valid opened context.
            unsafe { sys::avformat_close_input(&mut context) };
            return Err(MediaError::OpenFailed {
                message: "out of memory allocating packet scratch".to_owned(),
            });
        }

        Ok(Self {
            context,
            packet,
            interrupt: interrupt.clone(),
        })
    }

    /// Handle that cancels or time-boxes further blocking operations
    /// (`next_packet`, and any reconnect logic built on top).
    pub fn interrupt_handle(&self) -> &InterruptHandle {
        &self.interrupt
    }

    /// Demuxer short name, e.g. `matroska,webm` or `rtsp`.
    pub fn format_name(&self) -> String {
        // SAFETY: context is valid; iformat is set by avformat_open_input and
        // its `name` is a static NUL-terminated string.
        unsafe {
            let iformat = (*self.context).iformat;
            if iformat.is_null() || (*iformat).name.is_null() {
                return "unknown".to_owned();
            }
            cstr_to_string((*iformat).name)
        }
    }

    /// Container duration when the demuxer reports one.
    pub fn duration(&self) -> Option<Duration> {
        // SAFETY: context is valid; duration is a plain integer field in
        // AV_TIME_BASE (microsecond) units, negative when unknown.
        let micros = unsafe { (*self.context).duration };
        if micros <= 0 {
            return None;
        }
        Some(Duration::from_micros(micros as u64))
    }

    /// Snapshot of all streams discovered in the source.
    pub fn streams(&self) -> Vec<MediaStreamInfo> {
        // SAFETY: context is valid; nb_streams/streams are maintained by
        // libavformat and stable after avformat_find_stream_info, which
        // open() has already completed.
        unsafe {
            let count = (*self.context).nb_streams as usize;
            let streams = (*self.context).streams;
            (0..count)
                .filter_map(|index| {
                    let stream = *streams.add(index);
                    if stream.is_null() {
                        return None;
                    }
                    stream_info(stream, index as u32)
                })
                .collect()
        }
    }

    /// Reads the next compressed packet.
    ///
    /// Returns an owned [`FfmpegPacket`] that shares the demuxer's
    /// refcounted payload buffer and preserves the complete packet — side
    /// data and all flags included — for packet-faithful stream copy.
    ///
    /// Returns `Ok(None)` on clean end-of-stream and
    /// [`MediaError::Interrupted`] when the interrupt handle fired.
    pub fn next_packet(&mut self) -> Result<Option<FfmpegPacket>, MediaError> {
        // SAFETY: packet is our valid scratch packet; unref is always safe.
        unsafe { sys::av_packet_unref(self.packet) };

        // SAFETY: context and packet are valid and owned by self.
        let code = unsafe { sys::av_read_frame(self.context, self.packet) };
        if code < 0 {
            if code == sys::NIAN_AVERROR_EOF {
                return Ok(None);
            }
            return Err(error_for(
                code,
                "read packet",
                ErrorKind::Read,
                self.interrupt.state().should_abort(),
            ));
        }

        // Hand the read result to the caller as a separate owned reference
        // instead of copying bytes out of the scratch packet.
        //
        // SAFETY: av_packet_alloc has no preconditions; NULL checked below.
        let owned = unsafe { sys::av_packet_alloc() };
        if owned.is_null() {
            // Drop the data we cannot hand over so the next call starts
            // from a blank scratch packet.
            // SAFETY: self.packet is our valid scratch packet.
            unsafe { sys::av_packet_unref(self.packet) };
            return Err(MediaError::ReadFailed {
                message: "out of memory allocating output packet".to_owned(),
            });
        }
        // SAFETY: both packets are valid. Per the FFmpeg 8.0.3 contract,
        // success makes `owned` share the payload buffer (or deep-copy it
        // once for non-refcounted sources) and copies every other field,
        // side data included; failure leaves `owned` blank.
        let code = unsafe { sys::av_packet_ref(owned, self.packet) };
        if code < 0 {
            // SAFETY: both packets are ours. The scratch packet stays with
            // the struct (freed in Drop); only the failed allocation is
            // released, and the scratch reference is dropped so the next
            // call starts from a blank packet.
            unsafe {
                sys::av_packet_unref(self.packet);
                let mut failed = owned;
                sys::av_packet_free(&mut failed);
            }
            return Err(error_for(
                code,
                "reference packet",
                ErrorKind::Read,
                self.interrupt.state().should_abort(),
            ));
        }

        Ok(Some(FfmpegPacket::from_owned(owned)))
    }

    /// Codec parameters of stream `index` for stream-copy muxing.
    pub(crate) fn codec_parameters(&self, index: usize) -> Option<*mut sys::AVCodecParameters> {
        // SAFETY: context is valid; nb_streams/streams are stable after open.
        unsafe {
            let count = (*self.context).nb_streams as usize;
            if index >= count {
                return None;
            }
            let stream = *(*self.context).streams.add(index);
            (!stream.is_null() && !(*stream).codecpar.is_null()).then_some((*stream).codecpar)
        }
    }
}

impl Drop for MediaInput {
    fn drop(&mut self) {
        // SAFETY: context is either NULL (never — open() succeeded) or a
        // valid opened context; close_input frees it and nulls the pointer.
        unsafe { sys::avformat_close_input(&mut self.context) };
        // SAFETY: packet is a valid allocated packet or NULL.
        unsafe { sys::av_packet_free(&mut self.packet) };
    }
}

impl std::fmt::Debug for MediaInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaInput")
            .field("format", &self.format_name())
            .finish_non_exhaustive()
    }
}

fn set_option(dict: &mut *mut sys::AVDictionary, key: &str, value: &str) {
    let Ok(key) = CString::new(key) else {
        return;
    };
    let Ok(value) = CString::new(value) else {
        return;
    };
    // SAFETY: dict is a valid out-pointer we own; key/value are valid C
    // strings; flags 0 means "set, fail on conflict" which we ignore because
    // a rejected transport option only degrades to FFmpeg defaults.
    unsafe {
        sys::av_dict_set(dict, key.as_ptr(), value.as_ptr(), 0);
    }
}

/// # SAFETY
/// `pointer` must reference a valid NUL-terminated UTF-8 C string (all FFmpeg
/// name strings are static ASCII).
unsafe fn cstr_to_string(pointer: *const std::ffi::c_char) -> String {
    // SAFETY: contract above.
    unsafe {
        std::ffi::CStr::from_ptr(pointer)
            .to_string_lossy()
            .into_owned()
    }
}

/// # SAFETY
/// `stream` must be a valid `AVStream` owned by an open context.
unsafe fn stream_info(stream: *mut sys::AVStream, index: u32) -> Option<MediaStreamInfo> {
    // SAFETY: contract above; codecpar is set by libavformat for every stream.
    let parameters = unsafe {
        let stream_ref = &*stream;
        if stream_ref.codecpar.is_null() {
            return None;
        }
        &*stream_ref.codecpar
    };

    let media_type = match parameters.codec_type {
        sys::AVMEDIA_TYPE_VIDEO => MediaType::Video,
        sys::AVMEDIA_TYPE_AUDIO => MediaType::Audio,
        sys::AVMEDIA_TYPE_DATA => MediaType::Data,
        sys::AVMEDIA_TYPE_SUBTITLE => MediaType::Subtitle,
        _ => MediaType::Unknown,
    };

    // SAFETY: avcodec_get_name is a pure lookup on the codec id.
    let codec_name = unsafe { cstr_to_string(sys::avcodec_get_name(parameters.codec_id)) };

    let time_base = unsafe {
        let stream_ref = &*stream;
        MediaRational::new(stream_ref.time_base.num, stream_ref.time_base.den).ok()
    };

    let is_video = media_type == MediaType::Video;
    let is_audio = media_type == MediaType::Audio;

    Some(MediaStreamInfo {
        stream_index: index,
        media_type,
        codec_name,
        width: (is_video && parameters.width > 0).then_some(parameters.width as u32),
        height: (is_video && parameters.height > 0).then_some(parameters.height as u32),
        sample_rate: (is_audio && parameters.sample_rate > 0)
            .then_some(parameters.sample_rate as u32),
        time_base,
    })
}
