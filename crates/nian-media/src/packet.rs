//! A single compressed packet with owned payload bytes.

use nian_domain::MediaPacketMetadata;

/// Compressed data read from a media source, ready for stream-copy muxing.
///
/// The payload is copied out of the demuxer so the safe API has no lifetime
/// ties to the input context; recording throughput is unaffected in practice
/// (packet payloads are a few KB each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPacket {
    /// Timestamps, stream association and keyframe flag.
    pub metadata: MediaPacketMetadata,
    /// Compressed payload bytes.
    pub data: Vec<u8>,
}
