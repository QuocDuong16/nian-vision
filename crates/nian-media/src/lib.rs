//! Safe media facade.
//!
//! Application and worker code talks to media through this crate's types and
//! traits only. Concrete implementations live in separate crates (currently
//! `nian-media-ffmpeg`) and are wired up by the binaries that need them, so
//! this crate has no backend dependency at all. No FFmpeg symbols may appear
//! in this crate's public API.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

// Note: there is deliberately no `packet` module here. The lossy
// `MediaPacket { metadata, Vec<u8> }` type was removed: packets travel as
// `nian_media_ffmpeg::FfmpegPacket`, which carries the complete underlying
// FFmpeg packet (side data, all flags, refcounted payload). Reconstructing
// packets from payload bytes plus a handful of fields is a regression the
// media path must never take again.

pub mod error;
pub mod probe;
pub mod source;

pub use error::MediaError;
pub use probe::Probe;
pub use source::{MediaSource, RtspUrl};
