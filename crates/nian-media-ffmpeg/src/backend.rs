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
    /// Probes with a caller-supplied bounded deadline.
    pub fn probe_with_timeout(
        &self,
        source: &MediaSource,
        timeout: Duration,
    ) -> Result<MediaProbeReport, MediaError> {
        let interrupt = InterruptHandle::new();
        let _deadline = interrupt.scoped_deadline(timeout);
        let input = MediaInput::open(source, &interrupt)?;
        Ok(MediaProbeReport {
            format_name: input.format_name(),
            duration: input.duration(),
            streams: input.streams(),
        })
    }

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
        self.probe_with_timeout(source, PROBE_DEADLINE)
    }
}
