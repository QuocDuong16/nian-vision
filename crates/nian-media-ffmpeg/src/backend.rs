//! The FFmpeg implementation of the media facade's [`Probe`] capability.

use std::time::Duration;

use nian_domain::MediaProbeReport;
use nian_media::{MediaError, MediaSource, Probe};

use crate::input::MediaInput;
use crate::interrupt::InterruptHandle;

/// Default deadline for probe operations; long enough for a slow RTSP
/// handshake, short enough that a dead camera does not hang the caller.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// FFmpeg implementation of the media facade.
#[derive(Debug, Clone, Copy, Default)]
pub struct FfmpegBackend;

impl FfmpegBackend {
    /// Validates the loaded FFmpeg runtime against the compiled-in ABI and
    /// initializes the network layer.
    ///
    /// Call once at process startup; every media operation re-checks the
    /// one-time init internally as well.
    pub fn new() -> Result<Self, MediaError> {
        crate::version::global_init()?;
        Ok(Self)
    }
}

impl Probe for FfmpegBackend {
    fn probe(&self, source: &MediaSource) -> Result<MediaProbeReport, MediaError> {
        let interrupt = InterruptHandle::new();
        interrupt.set_deadline_from_now(PROBE_DEADLINE);

        let input = MediaInput::open(source, &interrupt)?;
        let format_name = input.format_name();
        let duration = input.duration();
        let streams = input.streams();

        Ok(MediaProbeReport {
            format_name,
            duration,
            streams,
        })
    }
}
