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
///
/// Concurrency: `OnceLock::get_or_init` guarantees that only one initializer
/// runs to completion as long as it does not panic; racing callers block
/// until that initializer returns and then all observe the same stored
/// result. Only a panicking initializer unlocks the cell for another
/// caller's retry — and every step here is idempotent anyway
/// (`check_runtime_abi` is a pure read, `av_log_set_level` overwrites one
/// global integer, `avformat_network_init` is documented as idempotent and
/// thread-safe), so even the retry path converges on an equivalent result.
pub fn global_init() -> Result<(), MediaError> {
    static RESULT: OnceLock<Result<(), MediaError>> = OnceLock::new();

    RESULT
        .get_or_init(|| perform_init(check_runtime_abi))
        .clone()
}

/// Reads the runtime library majors and verifies them against the ABI this
/// build was generated for.
///
/// This is a live check, independent of [`global_init`]: it can be called at
/// any time and reports the currently loaded libraries. The error is exactly
/// [`MediaError::AbiMismatch`] when the loaded runtime does not match this
/// build — never a stringified stand-in.
///
/// Used by the worker's startup/hello/describe capability reporting; after a
/// successful worker startup it is guaranteed to succeed.
pub fn runtime_versions() -> Result<RuntimeVersions, MediaError> {
    check_runtime_abi()
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
