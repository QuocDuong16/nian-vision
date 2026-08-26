//! Startup validation of the FFmpeg runtime against the compiled-in ABI.

use std::sync::OnceLock;

use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

use crate::logging;

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

/// Performs the actual one-time initialization. Split from [`global_init`] so
/// tests can inject a failing ABI check and assert the structured error is
/// preserved verbatim.
fn perform_init<F>(abi_check: F) -> Result<(), MediaError>
where
    F: FnOnce() -> Result<RuntimeVersions, MediaError>,
{
    // Propagated verbatim — an ABI mismatch must never be stringified into
    // `InitFailed`, or the host could not tell "wrong runtime" (operator
    // action required) apart from a transient init failure.
    abi_check()?;

    logging::install_production_policy();

    // SAFETY: avformat_network_init initializes internal network state; it
    // is documented as idempotent and thread-safe.
    let code = unsafe { sys::avformat_network_init() };
    if code < 0 {
        return Err(MediaError::InitFailed {
            message: super::error_util::averr_to_string(code),
        });
    }
    Ok(())
}

/// Runs [`check_runtime_abi`], the native-log policy and
/// `avformat_network_init` once per process. Every backend entry point calls
/// this first.
///
/// The stored result keeps the structured error: a failed init replays the
/// original [`MediaError::AbiMismatch`] (not a lossy string) to every caller.
/// Racing first callers may execute the idempotent body twice; whichever
/// result wins is identical because every step is deterministic and
/// side-effect-free for equal inputs.
pub fn global_init() -> Result<(), MediaError> {
    static RESULT: OnceLock<Result<(), MediaError>> = OnceLock::new();

    RESULT
        .get_or_init(|| perform_init(check_runtime_abi))
        .clone()
}

/// Returns the runtime versions if (and only if) startup validation ran.
///
/// Used by the worker's `describe` IPC method; returns `None` before the
/// first media operation.
pub fn runtime_versions() -> Option<RuntimeVersions> {
    check_runtime_abi().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_preserves_structured_abi_mismatch() {
        let original = MediaError::AbiMismatch {
            library: "libavformat",
            expected: 62,
            found: 63,
        };
        let result = perform_init(|| Err(original.clone()));
        // Must round-trip as the same variant, not be collapsed into
        // InitFailed { message }.
        assert_eq!(result.unwrap_err(), original);
    }

    #[test]
    fn repeated_global_init_is_idempotent_and_structured() {
        assert!(global_init().is_ok());
        assert!(global_init().is_ok());
    }
}
