//! FFmpeg-backed media implementation.
//!
//! This is the only application crate allowed to contain `unsafe`. Its public
//! API is safe; every unsafe block documents its invariants. Raw FFI lives in
//! `nian-ffmpeg-sys` and never leaks past this boundary.
//!
//! Ownership model:
//!
//! * `MediaInput` owns an `AVFormatContext` + scratch `AVPacket` and frees
//!   them in `Drop` via `avformat_close_input` / `av_packet_free`; each
//!   demuxed packet is handed out as an owned, refcounted `FfmpegPacket`
//!   sharing the payload buffer;
//! * `MatroskaMuxer` owns an output `AVFormatContext` (+ `AVIOContext`) and
//!   frees it in `Drop` via `avio_closep` / `avformat_free_context`;
//!   `write_packet` refs the caller's packet into an internal scratch packet
//!   whose reference `av_interleaved_write_frame` consumes;
//! * interrupt callbacks receive a `Arc::as_ptr` to an `InterruptState` that
//!   the owning wrapper keeps alive for the entire context lifetime;
//! * live input/output contexts stay thread-confined. `FfmpegPacket` and
//!   `MediaStreamTemplate` may move ownership to another thread so shared
//!   ingest can fan out compressed media without exposing raw FFI.

// Unit tests assert with panicking macros by design (integration tests
// declare the same exemption locally).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backend;
mod error_util;
mod fanout;
mod input;
mod interrupt;
mod logging;
mod luma_decoder;
mod muxer;
mod packet;
mod stream_template;
mod version;

pub use backend::FfmpegBackend;
pub use fanout::{
    PacketConsumerCounts, PacketConsumerKind, PacketDeliveryPolicy, PacketFanout,
    PacketFanoutReport, PacketFanoutSnapshot, PacketQueueLimits, PacketSubscription,
};
pub use input::MediaInput;
pub use interrupt::InterruptHandle;
pub use logging::native_logging_quiet;
pub use luma_decoder::{LUMA_HEIGHT, LUMA_WIDTH, LumaDecoder, LumaThumbnail};
pub use muxer::MatroskaMuxer;
pub use packet::FfmpegPacket;
pub use stream_template::MediaStreamTemplate;
pub use version::{RuntimeVersions, runtime_versions};
