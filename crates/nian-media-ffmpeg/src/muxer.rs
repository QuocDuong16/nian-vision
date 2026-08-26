//! Stream-copy muxing into Matroska files.
//!
//! This is the exact mechanism the M2 recorder will use for segment writing:
//! compressed packets go in untouched — no decode, no re-encode — with
//! timestamps rescaled from the input stream time base to the output stream
//! time base by `av_packet_rescale_ts`.
//!
//! # Stream mapping
//!
//! Output streams are never addressed by position. [`MatroskaMuxer::create`]
//! builds an explicit mapping
//! `input stream index -> output stream index + input time base`
//! keyed by [`MediaStreamInfo::stream_index`], and every packet is translated
//! through that mapping. Streams can be left unselected
//! ([`MatroskaMuxer::create_with_selection`]); packets belonging to unselected
//! streams are skipped deliberately (see [`MatroskaMuxer::write_packet`]).
//!
//! # Interrupt ownership invariant
//!
//! The `AVIOInterruptCB` installed on the output `AVFormatContext` (and copied
//! into the protocol layer by `avio_open2`) holds a raw pointer into the
//! `Arc` behind an [`InterruptHandle`]. That handle is **owned by the muxer
//! itself**, so the callback target provably outlives the context: Rust drops
//! struct fields only after `Drop::drop` has freed the FFmpeg objects. A
//! caller dropping its own clone of the handle mid-recording can never leave
//! a dangling callback behind.

use std::ffi::{CString, c_int};
use std::path::{Path, PathBuf};

use nian_domain::MediaRational;
use nian_ffmpeg_sys as sys;
use nian_media::{MediaError, MediaPacket};

use crate::error_util::{ErrorKind, error_for};
use crate::input::MediaInput;
use crate::interrupt::InterruptHandle;

/// Translation table entry for one copied stream, ordered by output index.
#[derive(Debug, Clone)]
struct StreamMapping {
    /// `MediaStreamInfo::stream_index` of the source stream this output
    /// stream was cloned from.
    input_stream_index: u32,
    /// Time base packets of the source stream arrive in.
    input_time_base: MediaRational,
}

/// Writes a Matroska file by copying packets from an open [`MediaInput`].
pub struct MatroskaMuxer {
    context: *mut sys::AVFormatContext,
    scratch: *mut sys::AVPacket,
    /// Owner of the interrupt state referenced by
    /// `context.interrupt_callback.opaque`; also polled by
    /// [`MatroskaMuxer::write_packet`] so cancellation stops segment writes.
    /// Must outlive every use of `context`: field drop order guarantees it
    /// survives until after `Drop::drop` freed the FFmpeg objects.
    interrupt: InterruptHandle,
    /// `output index -> mapping`; lookup by `input_stream_index`.
    stream_map: Vec<StreamMapping>,
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
        Self::create_with_selection(input, output_path, interrupt, |_| true)
    }

    /// Like [`MatroskaMuxer::create`], but copies only the streams for which
    /// `selector` returns `true`. Selection is keyed on
    /// [`MediaStreamInfo::stream_index`], not on enumeration position, so the
    /// produced input→output mapping stays correct when the selection skips
    /// streams.
    ///
    /// Fails when nothing is selected; a stream-less Matroska file is never a
    /// useful recording segment.
    pub fn create_with_selection<F>(
        input: &mut MediaInput,
        output_path: &Path,
        interrupt: &InterruptHandle,
        mut selector: F,
    ) -> Result<Self, MediaError>
    where
        F: FnMut(&nian_domain::MediaStreamInfo) -> bool,
    {
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
        // The callback installed here is backed by `interrupt`'s Arc. From
        // this point on, that handle must live as long as `context`; the
        // owning clone stored in `Self` provides exactly that guarantee.
        interrupt.install_on(context);

        let mut stream_map = Vec::new();
        for info in input.streams() {
            if !selector(&info) {
                continue;
            }

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

            if let Some(in_parameters) = input.codec_parameters(info.stream_index as usize) {
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

            stream_map.push(StreamMapping {
                input_stream_index: info.stream_index,
                input_time_base: info
                    .time_base
                    .unwrap_or(MediaRational { num: 1, den: 90000 }),
            });
        }

        if stream_map.is_empty() {
            // SAFETY: context is valid and owned by us; no file was opened.
            unsafe { sys::avformat_free_context(context) };
            return Err(MediaError::WriteFailed {
                message: "stream selection matched no input streams".to_owned(),
            });
        }

        // SAFETY: context is valid; pb is NULL before this call and owned by
        // us afterwards. avio_open2 copies `context.interrupt_callback` into
        // the protocol layer so even file I/O honors cancellation; the
        // callback target's lifetime is guaranteed by the retained handle.
        let code = unsafe {
            sys::avio_open2(
                &mut (*context).pb,
                path_c.as_ptr(),
                sys::AVIO_FLAG_WRITE as c_int,
                &(*context).interrupt_callback,
                std::ptr::null_mut(),
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
            // Best-effort close: avio_closep always frees the context's I/O
            // and nulls the pointer even when the final flush fails, so this
            // cannot leak; the close result cannot change the outcome (the
            // header never landed, so there is no durable segment either way).
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
            // Same best-effort-close reasoning as the header failure above.
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
            interrupt: interrupt.clone(),
            stream_map,
            output_path: output_path.to_path_buf(),
            finalized: false,
        })
    }

    /// Input stream indices that were selected/mapped into the output,
    /// ordered by output stream index.
    pub fn mapped_input_stream_indices(&self) -> Vec<u32> {
        self.stream_map
            .iter()
            .map(|mapping| mapping.input_stream_index)
            .collect()
    }

    /// Writes one packet with timestamp rescaling into the output streams.
    ///
    /// Packets whose stream was not selected are **skipped deliberately**
    /// (returns `Ok(())`): the recorder feeds the full demuxed packet stream
    /// while recording only the selected subset, and an unexpected extra
    /// stream must degrade gracefully instead of failing a healthy segment.
    /// Use [`MatroskaMuxer::mapped_input_stream_indices`] to account for
    /// skipped packets.
    pub fn write_packet(&mut self, packet: &MediaPacket) -> Result<(), MediaError> {
        if self.interrupt.state().should_abort() {
            return Err(MediaError::Interrupted {
                operation: "write packet",
            });
        }

        let Some((output_index, input_time_base)) = self
            .stream_map
            .iter()
            .enumerate()
            .find(|(_, mapping)| mapping.input_stream_index == packet.metadata.stream_index)
            .map(|(index, mapping)| (index, mapping.input_time_base))
        else {
            return Ok(()); // deliberate skip, documented above
        };
        let input_time_base = sys::AVRational {
            num: input_time_base.num,
            den: input_time_base.den,
        };

        // SAFETY: streams array is valid and `output_index` is below
        // nb_streams because `stream_map.len()` output streams were created.
        let out_time_base = unsafe {
            let out_stream = *(*self.context).streams.add(output_index);
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
            // The output context knows this packet under the MAPPED output
            // index, not the input one.
            scratch.stream_index = output_index as c_int;
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

    /// Writes the trailer, flushes and closes the file, returning the
    /// finalized path.
    ///
    /// The segment counts as finalized **only if both** the trailer write and
    /// the final I/O flush/close succeed. Any failure leaves the file on disk
    /// in `.partial`-recovery shape (never treated as a completed segment)
    /// and reports [`MediaError::WriteFailed`] (or
    /// [`MediaError::Interrupted`]).
    pub fn finalize(mut self) -> Result<PathBuf, MediaError> {
        // SAFETY: context is valid with a fully written header.
        let trailer_code = unsafe { sys::av_write_trailer(self.context) };
        if trailer_code < 0 {
            // `finalized` stays false: Drop performs the best-effort close
            // below and the partial file remains eligible for recovery.
            return Err(error_for(
                trailer_code,
                "write matroska trailer",
                ErrorKind::Write,
                self.interrupt.state().should_abort(),
            ));
        }

        // SAFETY: pb was opened by create() and is owned by us. avio_closep
        // flushes remaining buffered data, closes the descriptor, frees the
        // AVIOContext and nulls the pointer (even on failure), so Drop will
        // not close it again.
        //
        // A failed final flush means bytes may be missing from the file, so
        // the error MUST propagate: silently ignoring it could present a
        // truncated file as a completed segment.
        let close_code = unsafe { sys::avio_closep(&mut (*self.context).pb) };
        if close_code < 0 {
            return Err(error_for(
                close_code,
                "close matroska output",
                ErrorKind::Write,
                self.interrupt.state().should_abort(),
            ));
        }

        self.finalized = true;
        Ok(self.output_path.clone())
    }
}

impl Drop for MatroskaMuxer {
    fn drop(&mut self) {
        // SAFETY: context + scratch are valid and owned by us. On the
        // non-finalized path the partial file stays on disk by design
        // (crash-recovery semantics, master spec §9); the close here is
        // deliberately best-effort because a non-finalized file is already
        // classified as incomplete regardless of whether its last buffered
        // block made it to disk. avio_closep nulls `pb` even on failure, so
        // no double-close is possible.
        //
        // Fields (including the retained interrupt handle backing the
        // callback) drop only after this body has freed the context, keeping
        // the AVIOInterruptCB target alive for every possible FFmpeg call.
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
            .field("streams", &self.mapped_input_stream_indices())
            .field("finalized", &self.finalized)
            .finish_non_exhaustive()
    }
}
