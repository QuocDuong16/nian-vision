//! Stream-copy muxing into Matroska files.
//!
//! This is the exact mechanism the M2 recorder will use for segment writing:
//! compressed packets go in untouched — no decode, no re-encode — with
//! timestamps rescaled from the input stream time base to the output stream
//! time base by `av_packet_rescale_ts`.

use std::ffi::{CString, c_int};
use std::path::{Path, PathBuf};

use nian_domain::MediaRational;
use nian_ffmpeg_sys as sys;
use nian_media::{MediaError, MediaPacket};

use crate::error_util::{ErrorKind, error_for};
use crate::input::MediaInput;
use crate::interrupt::InterruptHandle;

/// Writes a Matroska file by copying packets from an open [`MediaInput`].
pub struct MatroskaMuxer {
    context: *mut sys::AVFormatContext,
    scratch: *mut sys::AVPacket,
    /// Input stream time bases, indexed by output stream index.
    input_time_bases: Vec<MediaRational>,
    output_path: PathBuf,
    finalized: bool,
}

impl MatroskaMuxer {
    /// Creates a Matroska output at `output_path`, cloning every stream of
    /// `input` (codec parameters copied verbatim).
    pub fn create(
        input: &mut MediaInput,
        output_path: &Path,
        interrupt: &InterruptHandle,
    ) -> Result<Self, MediaError> {
        crate::version::global_init()?;

        let path_text = output_path.to_string_lossy().into_owned();
        let path_c = CString::new(path_text.clone()).map_err(|_| MediaError::WriteFailed {
            message: "output path contains interior NUL bytes".to_owned(),
        })?;

        let mut context: *mut sys::AVFormatContext = std::ptr::null_mut();
        // SAFETY: all pointer arguments are valid; on success `context` holds
        // a freshly allocated output context for the matroska muxer.
        let code = unsafe {
            sys::avformat_alloc_output_context2(
                &mut context,
                std::ptr::null(),
                c"matroska".as_ptr(),
                path_c.as_ptr(),
            )
        };
        if code < 0 || context.is_null() {
            return Err(error_for(
                code,
                "create matroska output",
                ErrorKind::Write,
                false,
            ));
        }
        interrupt.install_on(context);

        let mut input_time_bases = Vec::new();
        for (index, info) in input.streams().into_iter().enumerate() {
            // SAFETY: context is valid; avformat_new_stream returns NULL on
            // failure, checked below.
            let out_stream = unsafe { sys::avformat_new_stream(context, std::ptr::null()) };
            if out_stream.is_null() {
                // SAFETY: context is valid and owned by us; nothing has been
                // written to disk yet.
                unsafe { sys::avformat_free_context(context) };
                return Err(MediaError::WriteFailed {
                    message: "out of memory creating output stream".to_owned(),
                });
            }

            if let Some(in_parameters) = input.codec_parameters(index) {
                // SAFETY: both parameter structs belong to valid streams.
                let code =
                    unsafe { sys::avcodec_parameters_copy((*out_stream).codecpar, in_parameters) };
                if code < 0 {
                    // SAFETY: context is valid and owned by us.
                    unsafe { sys::avformat_free_context(context) };
                    return Err(error_for(
                        code,
                        "copy stream parameters",
                        ErrorKind::Write,
                        false,
                    ));
                }
                // The codec tag comes from the source container; Matroska
                // assigns its own.
                // SAFETY: codecpar is valid.
                unsafe { (*(*out_stream).codecpar).codec_tag = 0 };
            }

            input_time_bases.push(
                info.time_base
                    .unwrap_or(MediaRational { num: 1, den: 90000 }),
            );
        }

        // SAFETY: context is valid; pb is NULL before this call and owned by
        // us afterwards.
        let code = unsafe {
            sys::avio_open(
                &mut (*context).pb,
                path_c.as_ptr(),
                sys::AVIO_FLAG_WRITE as c_int,
            )
        };
        if code < 0 {
            // SAFETY: context is valid and owned by us.
            unsafe { sys::avformat_free_context(context) };
            return Err(error_for(
                code,
                "open output file",
                ErrorKind::Write,
                interrupt.state().should_abort(),
            ));
        }

        // SAFETY: context is valid with all streams and I/O prepared.
        let code = unsafe { sys::avformat_write_header(context, std::ptr::null_mut()) };
        if code < 0 {
            // SAFETY: pb was opened above and both are owned by us.
            unsafe {
                sys::avio_closep(&mut (*context).pb);
                sys::avformat_free_context(context);
            }
            return Err(error_for(
                code,
                "write matroska header",
                ErrorKind::Write,
                interrupt.state().should_abort(),
            ));
        }

        // SAFETY: av_packet_alloc has no preconditions.
        let scratch = unsafe { sys::av_packet_alloc() };
        if scratch.is_null() {
            // SAFETY: pb + context are owned by us.
            unsafe {
                sys::avio_closep(&mut (*context).pb);
                sys::avformat_free_context(context);
            }
            return Err(MediaError::WriteFailed {
                message: "out of memory allocating packet scratch".to_owned(),
            });
        }

        Ok(Self {
            context,
            scratch,
            input_time_bases,
            output_path: output_path.to_path_buf(),
            finalized: false,
        })
    }

    /// Writes one packet with timestamp rescaling into the output streams.
    pub fn write_packet(&mut self, packet: &MediaPacket) -> Result<(), MediaError> {
        let index = packet.metadata.stream_index as usize;
        let Some(input_time_base) = self.input_time_bases.get(index) else {
            return Err(MediaError::WriteFailed {
                message: format!("packet references unknown stream index {index}"),
            });
        };
        let input_time_base = sys::AVRational {
            num: input_time_base.num,
            den: input_time_base.den,
        };

        // SAFETY: streams array is valid and `index` was accepted by
        // `input_time_bases.get` above, so it is below nb_streams.
        let out_time_base = unsafe {
            let out_stream = *(*self.context).streams.add(index);
            (*out_stream).time_base
        };

        // SAFETY: scratch is a valid packet owned by self. The field writes
        // prepare a non-reference-counted packet; av_interleaved_write_frame
        // copies non-reference-counted data, so `packet.data` stays valid and
        // owned by the caller.
        unsafe {
            let scratch = &mut *self.scratch;
            sys::av_packet_unref(self.scratch);
            scratch.data = packet.data.as_ptr() as *mut u8;
            scratch.size = packet.data.len() as c_int;
            scratch.stream_index = packet.metadata.stream_index as c_int;
            scratch.pts = packet.metadata.pts.unwrap_or(sys::NIAN_AV_NOPTS_VALUE);
            scratch.dts = packet.metadata.dts.unwrap_or(sys::NIAN_AV_NOPTS_VALUE);
            scratch.duration = packet.metadata.duration.unwrap_or(0);
            scratch.flags = if packet.metadata.keyframe {
                sys::AV_PKT_FLAG_KEY as c_int
            } else {
                0
            };
            sys::av_packet_rescale_ts(self.scratch, input_time_base, out_time_base);
        }

        // SAFETY: context + scratch are valid; the call consumes the packet
        // contents (unref'd at the top of the next write or in Drop).
        let code = unsafe { sys::av_interleaved_write_frame(self.context, self.scratch) };
        if code < 0 {
            return Err(error_for(code, "write packet", ErrorKind::Write, false));
        }
        Ok(())
    }

    /// Writes the trailer, closes the file and returns the finalized path.
    pub fn finalize(mut self) -> Result<PathBuf, MediaError> {
        // SAFETY: context is valid with a fully written header.
        let code = unsafe { sys::av_write_trailer(self.context) };
        if code < 0 {
            return Err(error_for(
                code,
                "write matroska trailer",
                ErrorKind::Write,
                false,
            ));
        }
        // SAFETY: pb was opened by create() and is owned by us.
        unsafe { sys::avio_closep(&mut (*self.context).pb) };
        self.finalized = true;
        Ok(self.output_path.clone())
    }
}

impl Drop for MatroskaMuxer {
    fn drop(&mut self) {
        // SAFETY: context + scratch are valid and owned by us. On the
        // non-finalized path the partial file stays on disk by design
        // (crash-recovery semantics, master spec §9).
        unsafe {
            if !self.finalized {
                let pb = (*self.context).pb;
                if !pb.is_null() {
                    sys::avio_closep(&mut (*self.context).pb);
                }
            }
            sys::av_packet_free(&mut self.scratch);
            sys::avformat_free_context(self.context);
        }
    }
}

impl std::fmt::Debug for MatroskaMuxer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatroskaMuxer")
            .field("output", &self.output_path)
            .field("finalized", &self.finalized)
            .finish_non_exhaustive()
    }
}
