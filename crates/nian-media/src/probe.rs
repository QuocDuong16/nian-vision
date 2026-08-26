//! Probing media sources without decoding.

use nian_domain::MediaProbeReport;

use crate::error::MediaError;
use crate::source::MediaSource;

/// Capability of opening a source and describing its streams.
pub trait Probe {
    /// Opens `source`, reads enough data to identify streams, closes it.
    ///
    /// Implementations must be free of side effects besides reading.
    fn probe(&self, source: &MediaSource) -> Result<MediaProbeReport, MediaError>;
}
