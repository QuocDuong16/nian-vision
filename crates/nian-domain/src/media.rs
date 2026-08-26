//! Media stream and packet descriptions.
//!
//! These types are the domain-facing view of media data. They are produced by
//! the media layer (FFmpeg today) and consumed by application/UI code, which
//! must stay free of any FFmpeg dependency.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Kind of an elementary stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum MediaType {
    /// Video track.
    Video,
    /// Audio track.
    Audio,
    /// Data/metadata track.
    Data,
    /// Subtitle track.
    Subtitle,
    /// Unrecognized kind.
    #[default]
    Unknown,
}

impl MediaType {
    /// Stable lowercase name used in logs and IPC payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Data => "data",
            Self::Subtitle => "subtitle",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rational number as used for media time bases (`num/den` ticks per second).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaRational {
    /// Numerator.
    pub num: i32,
    /// Denominator; never zero for a valid time base.
    pub den: i32,
}

impl MediaRational {
    /// Creates a rational, rejecting a zero denominator.
    pub fn new(num: i32, den: i32) -> Result<Self, crate::error::DomainError> {
        if den == 0 {
            return Err(crate::error::DomainError::InvalidMediaTimeBase { den });
        }
        Ok(Self { num, den })
    }

    /// Converts a duration in these units to [`Duration`], saturating on
    /// overflow. Returns `None` when the denominator is non-positive.
    pub fn duration_of(&self, units: i64) -> Option<Duration> {
        if self.den <= 0 {
            return None;
        }
        let micros = (units as i128)
            .checked_mul(1_000_000)?
            .checked_mul(self.num as i128)?
            .checked_div(self.den as i128)?;
        u64::try_from(micros).ok().map(Duration::from_micros)
    }
}

impl fmt::Display for MediaRational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// Description of one elementary stream discovered in a media source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStreamInfo {
    /// Zero-based position of the stream inside its container.
    pub stream_index: u32,
    /// Kind of content carried by the stream.
    pub media_type: MediaType,
    /// Codec short name, e.g. `h264`, `aac` (informational).
    pub codec_name: String,
    /// Frame width in pixels (video only).
    pub width: Option<u32>,
    /// Frame height in pixels (video only).
    pub height: Option<u32>,
    /// Sample rate in Hz (audio only).
    pub sample_rate: Option<u32>,
    /// Time base that packet timestamps of this stream are expressed in.
    pub time_base: Option<MediaRational>,
}

/// Metadata of a single compressed packet read from a media source.
///
/// Timestamps are expressed in the owning stream's time base; no unit
/// conversion happens in the domain layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPacketMetadata {
    /// Owning stream index.
    pub stream_index: u32,
    /// Presentation timestamp, in stream time base units.
    pub pts: Option<i64>,
    /// Decoding timestamp, in stream time base units.
    pub dts: Option<i64>,
    /// Packet duration, in stream time base units.
    pub duration: Option<i64>,
    /// Whether the packet starts a keyframe (video only meaningful).
    pub keyframe: bool,
}

/// Result of probing a media source without decoding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaProbeReport {
    /// Container format name, e.g. `matroska,webm`.
    pub format_name: String,
    /// Total duration when the container reports one.
    pub duration: Option<Duration>,
    /// Discovered streams.
    pub streams: Vec<MediaStreamInfo>,
}

impl MediaProbeReport {
    /// Returns the first video stream, which NVR recording cares about most.
    pub fn video_stream(&self) -> Option<&MediaStreamInfo> {
        self.streams
            .iter()
            .find(|s| s.media_type == MediaType::Video)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_rejects_zero_denominator() {
        assert!(MediaRational::new(1, 0).is_err());
        let tb = MediaRational::new(1, 1000).unwrap();
        assert_eq!(tb.duration_of(1500), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn duration_conversion_saturates_instead_of_panicking() {
        let tb = MediaRational::new(1, 1).unwrap();
        assert_eq!(tb.duration_of(i64::MAX), None);
        let tb_neg = MediaRational::new(1, -2).unwrap();
        assert_eq!(tb_neg.duration_of(10), None);
    }
}
