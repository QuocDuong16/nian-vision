//! Small helpers for translating FFmpeg return codes into `MediaError`.

use std::ffi::{c_char, c_int};

use nian_ffmpeg_sys as sys;
use nian_media::MediaError;

use crate::interrupt::{AbortCause, InterruptHandle};

/// Size mandated by `AV_ERROR_MAX_STRING_SIZE`.
const ERROR_BUFFER_SIZE: usize = sys::AV_ERROR_MAX_STRING_SIZE as usize;

/// Maps the current abort state of `interrupt` onto the matching media
/// error, or `None` while nothing demands abortion.
///
/// This is the single place where "why did this operation abort" turns into
/// a typed error: operator cancellation becomes [`MediaError::Interrupted`],
/// an expired deadline becomes [`MediaError::TimedOut`]. Callers downstream
/// (recorder, supervisors) never re-derive the distinction themselves.
pub(crate) fn interrupt_error(
    interrupt: &InterruptHandle,
    operation: &'static str,
) -> Option<MediaError> {
    match interrupt.state().abort_cause() {
        AbortCause::Cancelled => Some(MediaError::Interrupted { operation }),
        AbortCause::DeadlineExceeded => Some(MediaError::TimedOut { operation }),
        AbortCause::None => None,
    }
}

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
/// `interrupt` is the handle whose interrupt callback was active during the
/// blocking operation (`None` for operations that cannot be interrupted,
/// such as pure in-memory packet refcounting). The interrupt state — not the
/// FFmpeg error code — is the authority on WHY an operation aborted: a
/// cancellation becomes [`MediaError::Interrupted`], an expired deadline
/// becomes [`MediaError::TimedOut`], and anything else maps through `kind`.
pub(crate) fn error_for(
    code: c_int,
    operation: &'static str,
    kind: ErrorKind,
    interrupt: Option<&InterruptHandle>,
) -> MediaError {
    match interrupt.map(|handle| handle.state().abort_cause()) {
        Some(crate::interrupt::AbortCause::Cancelled) => MediaError::Interrupted { operation },
        Some(crate::interrupt::AbortCause::DeadlineExceeded) => MediaError::TimedOut { operation },
        _ => {
            let detail = averr_to_string(code);
            match kind {
                ErrorKind::Open => MediaError::OpenFailed { message: detail },
                ErrorKind::Read => MediaError::ReadFailed { message: detail },
                ErrorKind::Write => MediaError::WriteFailed { message: detail },
            }
        }
    }
}

/// Which failure variant a non-interrupted negative return code maps to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ErrorKind {
    Open,
    Read,
    Write,
}
