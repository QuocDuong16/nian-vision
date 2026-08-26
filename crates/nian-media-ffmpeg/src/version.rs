//! Startup validation of the FFmpeg runtime against the compiled-in ABI.

use std::sync::OnceLock;

use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

/// Library majors this build was generated against (from vendored headers).
pub const EXPECTED_AVFORMAT_MAJOR: u32 = sys::LIBAVFORMAT_VERSION_MAJOR;
pub const EXPECTED_AVCODEC_MAJOR: u32 = sys::LIBAVCODEC_VERSION_MAJOR;
pub const EXPECTED_AVUTIL_MAJOR: u32 = sys::LIBAVUTIL_VERSION_MAJOR;

/// Majors reported by the FFmpeg shared libraries loaded into this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeVersions {
    /// `libavformat` major.
    pub avformat: u32,
    /// `libavcodec` major.
    pub avcodec: u32,
    /// `libavutil` major.
    pub avutil: u32,
}

/// Reads the runtime library versions and verifies they match the ABI the
/// bindings were generated for.
///
/// # SAFETY
/// `avformat_version`/`avcodec_version`/`avutil_version` take no arguments
/// and touch no global state; safe to call from any thread.
pub fn check_runtime_abi() -> Result<RuntimeVersions, MediaError> {
    let avformat = unsafe { sys::avformat_version() } as u32;
    let avcodec = unsafe { sys::avcodec_version() } as u32;
    let avutil = unsafe { sys::avutil_version() } as u32;

    let found = RuntimeVersions {
        avformat: avformat >> 16,
        avcodec: avcodec >> 16,
        avutil: avutil >> 16,
    };

    let expected = [
        ("libavformat", EXPECTED_AVFORMAT_MAJOR, found.avformat),
        ("libavcodec", EXPECTED_AVCODEC_MAJOR, found.avcodec),
        ("libavutil", EXPECTED_AVUTIL_MAJOR, found.avutil),
    ];
    for (library, expected_major, found_major) in expected {
        if expected_major != found_major {
            return Err(MediaError::AbiMismatch {
                library,
                expected: expected_major,
                found: found_major,
            });
        }
    }

    Ok(found)
}

/// Runs [`check_runtime_abi`] and `avformat_network_init` once per process
/// (racing threads may run the idempotent init twice; the result is shared).
/// Every backend entry point calls this first.
pub fn global_init() -> Result<(), MediaError> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();

    RESULT
        .get_or_init(|| {
            if let Err(error) = check_runtime_abi() {
                return Err(error.to_string());
            }

            // SAFETY: avformat_network_init initializes internal network
            // state; it is documented as idempotent and thread-safe.
            let code = unsafe { sys::avformat_network_init() };
            if code < 0 {
                return Err(super::error_util::averr_to_string(code));
            }
            Ok(())
        })
        .clone()
        .map_err(|message| MediaError::InitFailed { message })
}

/// Returns the runtime versions if (and only if) startup validation ran.
///
/// Used by the worker's `describe` IPC method; returns `None` before the
/// first media operation.
pub fn runtime_versions() -> Option<RuntimeVersions> {
    check_runtime_abi().ok()
}
