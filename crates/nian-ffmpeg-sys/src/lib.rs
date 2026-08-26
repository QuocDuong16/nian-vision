//! Raw FFmpeg FFI declarations.
//!
//! Everything here is `unsafe` by definition; nothing in this crate provides
//! memory or thread safety. The only consumer is the safe wrapper in
//! `nian-media-ffmpeg`.
//!
//! The declarations in [`bindings`] are **generated** from the vendored
//! FFmpeg 8.0.1 headers under `thirdparty/ffmpeg` by `cargo run
//! -p bindgen-gen`, and are committed so that normal builds never need clang.
//! They target the FFmpeg 8 ABI (`libavformat` major 62); consumers must
//! verify the loaded runtime matches before calling anything (see
//! `nian-media-ffmpeg`'s startup check).

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

/// Generated declarations; see crate docs for provenance.
#[allow(clippy::all, missing_docs)]
pub mod bindings;

pub use bindings::*;
