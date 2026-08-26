//! Small helpers for translating FFmpeg return codes into `MediaError`.

use std::ffi::{c_char, c_int};

use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

/// Size mandated by `AV_ERROR_MAX_STRING_SIZE`.
const ERROR_BUFFER_SIZE: usize = sys::AV_ERROR_MAX_STRING_SIZE as usize;

/// Renders an FFmpeg error code with `av_strerror`.
///
/// # SAFETY
/// Calls `av_strerror`, which only reads its integer argument and writes into
/// the caller-provided buffer; no aliasing or threading hazards.
pub(crate) fn averr_to_string(code: c_int) -> String {
    let mut buffer = [0 as c_char; ERROR_BUFFER_SIZE];
    let result = unsafe { sys::av_strerror(code, buffer.as_mut_ptr(), ERROR_BUFFER_SIZE) };
    if result == 0 {
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("ffmpeg error {code}")
    }
}

/// Converts a negative FFmpeg return code into the matching `MediaError`.
///
/// `interrupted` must come from the caller's interrupt state (the interrupt
/// callback is the authority on whether an operation was aborted — FFmpeg
/// reports it as a generic error code).
pub(crate) fn error_for(
    code: c_int,
    operation: &'static str,
    kind: ErrorKind,
    interrupted: bool,
) -> MediaError {
    if interrupted {
        return MediaError::Interrupted { operation };
    }
    let detail = averr_to_string(code);
    match kind {
        ErrorKind::Open => MediaError::OpenFailed { message: detail },
        ErrorKind::Read => MediaError::ReadFailed { message: detail },
        ErrorKind::Write => MediaError::WriteFailed { message: detail },
    }
}

/// Which failure variant a non-interrupted negative return code maps to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ErrorKind {
    Open,
    Read,
    Write,
}
