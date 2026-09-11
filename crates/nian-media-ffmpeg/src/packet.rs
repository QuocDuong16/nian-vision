//! Safe ownership of a single demuxed FFmpeg packet.
//!
//! [`FfmpegPacket`] owns one reference to an `AVPacket` produced by
//! [`MediaInput::next_packet`](crate::MediaInput::next_packet). It is what
//! makes stream copy **packet-faithful**: instead of reconstructing an
//! approximation from payload bytes and a handful of fields, the muxer sees
//! the exact packet the demuxer emitted — side data (new extradata,
//! parameter changes, skip samples, …), every flag, and the refcounted
//! payload buffer shared with the demuxer rather than copied.
//!
//! The public surface is deliberately narrow and safe: generic metadata for
//! recorder logic ([`metadata`](FfmpegPacket::metadata)), read-only payload
//! access ([`data`](FfmpegPacket::data)) and a redacting [`Debug`]. Raw
//! pointers never escape this crate; consumers hand the packet back to
//! [`MatroskaMuxer::write_packet`](crate::MatroskaMuxer::write_packet),
//! which copies a new reference into its output scratch packet and therefore
//! never mutates or consumes the caller's packet.

use std::ffi::c_int;

use nian_domain::MediaPacketMetadata;
use nian_ffmpeg_sys as sys;

/// An owned, reference-counted FFmpeg packet.
///
/// Not `Send`/`Sync` (raw FFI pointer), matching the worker's
/// single-threaded media loop. Payload bytes are shared with the demuxing
/// context's buffer through libavutil refcounting; no copy is made when the
/// source packet is reference-counted (demuxers emit refcounted packets by
/// default; non-refcounted sources are deep-copied once by `av_packet_ref`).
pub struct FfmpegPacket {
    /// Owning AVPacket with at least one live reference to the payload.
    /// Freed with `av_packet_free` in `Drop`.
    packet: *mut sys::AVPacket,
}

impl FfmpegPacket {
    /// Takes ownership of a freshly allocated packet that already holds a
    /// reference (used by `MediaInput` after a successful `av_packet_ref`).
    pub(crate) fn from_owned(packet: *mut sys::AVPacket) -> Self {
        Self { packet }
    }

    /// Generic metadata view used by recorder logic: stream index,
    /// timestamps, duration and keyframe flag.
    pub fn metadata(&self) -> MediaPacketMetadata {
        // SAFETY: self.packet is valid for the lifetime of &self; only
        // plain integer/bool fields are read.
        unsafe {
            let packet = &*self.packet;
            MediaPacketMetadata {
                stream_index: u32::try_from(packet.stream_index).unwrap_or(u32::MAX),
                pts: optional_pts(packet.pts),
                dts: optional_pts(packet.dts),
                duration: optional_pts(packet.duration),
                keyframe: packet.flags & sys::AV_PKT_FLAG_KEY as c_int != 0,
            }
        }
    }

    /// Read-only view of the packet payload.
    pub fn data(&self) -> &[u8] {
        // SAFETY: while a reference is held, `data` points to `size`
        // readable bytes owned by the packet (NULL/0 for blank packets).
        unsafe {
            let packet = &*self.packet;
            let size = usize::try_from(packet.size).unwrap_or(0);
            if packet.data.is_null() || size == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(packet.data.cast_const(), size)
            }
        }
    }

    /// Side data of the given FFmpeg kind, if present.
    ///
    /// Exposed crate-internally (and to tests) so regression tests can prove
    /// side data survives the copy path without widening the public API.
    #[cfg(test)]
    pub(crate) fn side_data(&self, kind: sys::AVPacketSideDataType) -> Option<Vec<u8>> {
        let mut size: usize = 0;
        // SAFETY: self.packet is valid; av_packet_get_side_data only reads
        // it and returns either NULL or a pointer into packet-owned memory
        // of `size` bytes that outlives the call (valid until the packet is
        // mutated/unreferenced — we copy immediately).
        let pointer = unsafe { sys::av_packet_get_side_data(self.packet, kind, &mut size) };
        (!pointer.is_null() && size > 0).then(|| {
            // SAFETY: see above; size bytes are readable at `pointer`.
            unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec()
        })
    }

    /// Raw pointer for the FFmpeg-boundary-internal copy step
    /// (`MatroskaMuxer::write_packet` refs from it); never exposed outside
    /// this crate.
    pub(crate) fn as_raw(&self) -> *const sys::AVPacket {
        self.packet
    }

    /// Test-only seam for reproducing RTSP cameras that omit one timestamp.
    /// Production callers never mutate demuxed packets; normalization happens
    /// on the muxer's private packet reference.
    #[cfg(test)]
    pub(crate) fn set_timestamps_for_test(&self, dts: Option<i64>, pts: Option<i64>) {
        // SAFETY: tests own this packet exclusively for the duration of the
        // mutation and only edit scalar timestamp fields.
        unsafe {
            (*self.packet).dts = dts.unwrap_or(sys::NIAN_AV_NOPTS_VALUE);
            (*self.packet).pts = pts.unwrap_or(sys::NIAN_AV_NOPTS_VALUE);
        }
    }
}

impl Drop for FfmpegPacket {
    fn drop(&mut self) {
        // SAFETY: self.packet was allocated by av_packet_alloc and still
        // holds its reference (write paths ref-copy instead of consuming).
        unsafe { sys::av_packet_free(&mut self.packet) };
    }
}

impl std::fmt::Debug for FfmpegPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metadata = self.metadata();
        f.debug_struct("FfmpegPacket")
            .field("stream_index", &metadata.stream_index)
            .field("pts", &metadata.pts)
            .field("dts", &metadata.dts)
            .field("duration", &metadata.duration)
            .field("keyframe", &metadata.keyframe)
            .field("payload_bytes", &self.data().len())
            .finish_non_exhaustive()
    }
}

fn optional_pts(value: i64) -> Option<i64> {
    (value != sys::NIAN_AV_NOPTS_VALUE).then_some(value)
}

#[cfg(test)]
mod tests {
    //! Regression proof that packet properties beyond payload and PTS
    //! survive the demux→mux path. The side-data injection uses raw FFmpeg
    //! calls, so these tests live inside the FFmpeg boundary crate; the
    //! assertions themselves go through the public safe API.

    use super::*;
    use crate::input::MediaInput;
    use crate::interrupt::InterruptHandle;
    use crate::muxer::MatroskaMuxer;
    use nian_media::Probe as _;

    /// Deterministic sentinel payload for `AV_PKT_DATA_NEW_EXTRADATA`.
    const SENTINEL: &[u8] = b"nian-new-extradata-sentinel-0123456789abcdef";

    fn open_fixture() -> MediaInput {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.extend(["tests", "fixtures", "sample.mkv"]);
        let interrupt = InterruptHandle::new();
        MediaInput::open(&nian_media::MediaSource::file(path), &interrupt).expect("fixture opens")
    }

    fn inject_new_extradata(packet: &FfmpegPacket, payload: &[u8]) {
        // SAFETY: `packet` owns a valid AVPacket for as long as the call;
        // av_packet_new_side_data appends a fresh side-data entry of exactly
        // `payload.len()` bytes owned by the packet and returns NULL on
        // failure. We fill it before any further use.
        let pointer = unsafe {
            sys::av_packet_new_side_data(
                packet.packet,
                sys::AV_PKT_DATA_NEW_EXTRADATA,
                payload.len(),
            )
        };
        assert!(!pointer.is_null(), "side data allocation failed");
        // SAFETY: `pointer` refers to `payload.len()` writable bytes just
        // allocated for this packet.
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, payload.len()) };
    }

    #[test]
    fn output_reference_shares_payload_and_carries_side_data() {
        // Pins the FFmpeg 8.0.3 av_packet_ref contract the muxer relies on:
        // payload shared by refcount, all other fields (side data included)
        // duplicated, source packet untouched.
        let mut input = open_fixture();
        let packet = input
            .next_packet()
            .unwrap()
            .expect("fixture yields a packet");
        assert!(!packet.data().is_empty());
        inject_new_extradata(&packet, SENTINEL);

        // SAFETY: av_packet_alloc has no preconditions.
        let scratch = unsafe { sys::av_packet_alloc() };
        assert!(!scratch.is_null());
        // SAFETY: both packets valid; this is exactly the copy step used by
        // MatroskaMuxer::write_packet.
        let code = unsafe { sys::av_packet_ref(scratch, packet.as_raw()) };
        assert_eq!(code, 0, "av_packet_ref must succeed");

        // Zero-copy: destination shares the source payload buffer.
        let copied = FfmpegPacket::from_owned(scratch); // frees at scope end
        assert_eq!(
            copied.data().as_ptr(),
            packet.data().as_ptr(),
            "refcounted packets must share the payload buffer"
        );
        assert_eq!(
            copied.metadata().stream_index,
            packet.metadata().stream_index
        );

        // Side data survived byte-exactly — and the caller's own packet was
        // not consumed or mutated by taking the reference.
        assert_eq!(
            copied.side_data(sys::AV_PKT_DATA_NEW_EXTRADATA).as_deref(),
            Some(SENTINEL)
        );
        assert_eq!(
            packet.side_data(sys::AV_PKT_DATA_NEW_EXTRADATA).as_deref(),
            Some(SENTINEL)
        );
    }

    #[test]
    fn muxer_write_preserves_side_data_and_caller_ownership_end_to_end() {
        let mut input = open_fixture();
        let interrupt = InterruptHandle::new();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("side-data.mkv");
        let mut muxer = MatroskaMuxer::create(&mut input, &output, &interrupt).unwrap();

        let mut injected_seen = false;
        while let Some(packet) = input.next_packet().unwrap() {
            // Inject exactly once, into the first keyframe packet; the
            // per-iteration flag guards the post-write assertions so they
            // apply only to the packet that actually carries side data.
            let mut carries_sentinel = false;
            if !injected_seen && packet.metadata().keyframe {
                inject_new_extradata(&packet, SENTINEL);
                injected_seen = true;
                carries_sentinel = true;
            }

            let payload_before = packet.data().to_vec();
            muxer.write_packet(&packet).unwrap();

            if carries_sentinel {
                // The write took a reference of its own: the caller's packet
                // still carries the side data and its full payload afterwards.
                assert_eq!(
                    packet.side_data(sys::AV_PKT_DATA_NEW_EXTRADATA).as_deref(),
                    Some(SENTINEL),
                    "caller's side data must survive write_packet"
                );
                assert_eq!(packet.data(), payload_before);
            }
        }
        assert!(injected_seen, "fixture must contain a keyframe packet");
        muxer.finalize().unwrap();

        // The written file is a valid segment.
        let backend = crate::backend::FfmpegBackend::new().unwrap();
        let report = backend
            .probe(&nian_media::MediaSource::file(&output))
            .unwrap();
        assert_eq!(report.video_stream().unwrap().codec_name, "mpeg4");
    }
}
