//! Browser-compatible PCM WAV sidecar for G.711 audio in archived recordings.
//!
//! Keep the camera's original G.711 packets in Matroska. Playback converts
//! only the audio samples, once per prepared session, without a CLI process or
//! transcoding video. The output is owned by the playback session cache.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nian_domain::{MediaStreamInfo, MediaType};
use nian_media_ffmpeg::FfmpegPacket;

// A finite cache bound independent of a camera's claimed duration. WAV uses
// 32-bit RIFF sizes; 256 MiB is also enough for more than two hours of 8k mono.
const MAX_PCM_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TIMESTAMP_GAP_SECONDS: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Law {
    ALaw,
    MuLaw,
}

fn decode(code: u8, law: Law) -> i16 {
    match law {
        Law::ALaw => {
            let value = code ^ 0x55;
            let segment = (value & 0x70) >> 4;
            let mut magnitude = i32::from(value & 0x0f) * 16 + 8;
            if segment != 0 {
                magnitude += 0x100;
                magnitude <<= u32::from(segment - 1);
            }
            let magnitude = magnitude as i16;
            if value & 0x80 != 0 {
                magnitude
            } else {
                -magnitude
            }
        }
        Law::MuLaw => {
            let value = !code;
            let magnitude =
                ((i32::from(value & 0x0f) << 3) + 0x84) << u32::from((value & 0x70) >> 4);
            let sample = (magnitude - 0x84) as i16;
            if value & 0x80 != 0 { -sample } else { sample }
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Single-stream G.711 to PCM16LE streaming conversion. Never buffers a clip.
pub struct G711WavWriter {
    file: File,
    path: PathBuf,
    stream_index: u32,
    rate: u32,
    channels: u16,
    law: Law,
    time_base: nian_domain::MediaRational,
    first_timestamp: Option<i64>,
    frames_written: u64,
    data_bytes: u64,
    finished: bool,
}

impl G711WavWriter {
    pub fn create(path: &Path, stream: &MediaStreamInfo, channels: u16) -> io::Result<Self> {
        if stream.media_type != MediaType::Audio || !(1..=2).contains(&channels) {
            return Err(invalid("G.711 playback requires mono/stereo audio"));
        }
        let law = match stream.codec_name.as_str() {
            "pcm_alaw" => Law::ALaw,
            "pcm_mulaw" => Law::MuLaw,
            _ => return Err(invalid("unsupported G.711 law")),
        };
        let rate = stream
            .sample_rate
            .filter(|rate| (8_000..=48_000).contains(rate))
            .ok_or_else(|| invalid("invalid G.711 sample rate"))?;
        let time_base = stream
            .time_base
            .filter(|base| base.num > 0 && base.den > 0)
            .ok_or_else(|| invalid("G.711 stream has no time base"))?;
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = Self {
            file,
            path: path.to_path_buf(),
            stream_index: stream.stream_index,
            rate,
            channels,
            law,
            time_base,
            first_timestamp: None,
            frames_written: 0,
            data_bytes: 0,
            finished: false,
        };
        writer.write_header(0)?;
        Ok(writer)
    }

    fn write_header(&mut self, size: u32) -> io::Result<()> {
        let alignment = self.channels * 2;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(b"RIFF")?;
        self.file.write_all(&(36_u32 + size).to_le_bytes())?;
        self.file.write_all(b"WAVEfmt ")?;
        self.file.write_all(&16_u32.to_le_bytes())?;
        self.file.write_all(&1_u16.to_le_bytes())?; // PCM, not G.711.
        self.file.write_all(&self.channels.to_le_bytes())?;
        self.file.write_all(&self.rate.to_le_bytes())?;
        self.file
            .write_all(&(self.rate * u32::from(alignment)).to_le_bytes())?;
        self.file.write_all(&alignment.to_le_bytes())?;
        self.file.write_all(&16_u16.to_le_bytes())?;
        self.file.write_all(b"data")?;
        self.file.write_all(&size.to_le_bytes())?;
        Ok(())
    }

    fn append_silence(&mut self, frames: u64) -> io::Result<()> {
        let bytes = frames
            .checked_mul(u64::from(self.channels) * 2)
            .ok_or_else(|| invalid("audio duration overflow"))?;
        self.reserve(bytes)?;
        let zeros = [0_u8; 4096];
        let mut left = bytes;
        while left > 0 {
            let count = usize::try_from(left.min(zeros.len() as u64)).unwrap_or(zeros.len());
            self.file.write_all(&zeros[..count])?;
            left -= count as u64;
        }
        self.frames_written += frames;
        self.data_bytes += bytes;
        Ok(())
    }

    fn reserve(&self, additional: u64) -> io::Result<()> {
        if self
            .data_bytes
            .checked_add(additional)
            .is_none_or(|size| size > MAX_PCM_BYTES)
        {
            return Err(invalid("G.711 playback audio exceeds cache limit"));
        }
        Ok(())
    }

    pub fn write_packet(&mut self, packet: &FfmpegPacket) -> io::Result<()> {
        let metadata = packet.metadata();
        if metadata.stream_index != self.stream_index {
            return Ok(());
        }
        let data = packet.data();
        if !data.len().is_multiple_of(usize::from(self.channels)) {
            return Err(invalid("G.711 packet has a partial sample frame"));
        }
        let mut skip_frames = 0_u64;
        if let Some(timestamp) = metadata.pts.or(metadata.dts) {
            let first = *self.first_timestamp.get_or_insert(timestamp);
            let elapsed = (i128::from(timestamp) - i128::from(first))
                .checked_mul(i128::from(self.time_base.num))
                .and_then(|value| value.checked_mul(i128::from(self.rate)))
                .and_then(|value| value.checked_div(i128::from(self.time_base.den)))
                .ok_or_else(|| invalid("invalid G.711 timestamp"))?;
            let target =
                u64::try_from(elapsed.max(0)).map_err(|_| invalid("G.711 time overflow"))?;
            if target > self.frames_written {
                let gap = target - self.frames_written;
                if gap > u64::from(self.rate) * MAX_TIMESTAMP_GAP_SECONDS {
                    return Err(invalid("G.711 timeline has a large discontinuity"));
                }
                self.append_silence(gap)?;
            } else {
                skip_frames = self.frames_written - target;
            }
        }
        let skip_bytes = usize::try_from(skip_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(usize::from(self.channels)))
            .unwrap_or(usize::MAX)
            .min(data.len());
        let remaining = &data[skip_bytes..];
        let bytes = u64::try_from(remaining.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(2);
        self.reserve(bytes)?;
        for &sample in remaining {
            self.file
                .write_all(&decode(sample, self.law).to_le_bytes())?;
        }
        self.frames_written += (remaining.len() / usize::from(self.channels)) as u64;
        self.data_bytes += bytes;
        Ok(())
    }

    pub fn finalize(mut self) -> io::Result<()> {
        let size = u32::try_from(self.data_bytes).map_err(|_| invalid("WAV too long"))?;
        self.write_header(size)?;
        self.file.flush()?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for G711WavWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_g711_laws_decode_known_silence_and_polarities() {
        assert_eq!(decode(0xd5, Law::ALaw), 8);
        assert_eq!(decode(0x55, Law::ALaw), -8);
        assert_eq!(decode(0xff, Law::MuLaw), 0);
        assert_eq!(decode(0x7f, Law::MuLaw), 0);
        assert!(decode(0x80, Law::MuLaw) > 0);
        assert!(decode(0x00, Law::MuLaw) < 0);
    }
}
