//! Media source abstraction.

use std::path::{Path, PathBuf};

/// A location a media operation can read from.
///
/// RTSP URLs built here may embed credentials for the connect call only;
/// `Debug`/`Display` render a redacted form.
#[derive(Clone)]
pub enum MediaSource {
    /// A local container file.
    File(PathBuf),
    /// A live network stream (RTSP today).
    Rtsp {
        /// Full connect URL, credentials included when configured.
        url: RtspUrl,
    },
}

/// A credential-bearing URL whose text representation is always redacted.
#[derive(Clone)]
pub struct RtspUrl(String);

impl RtspUrl {
    /// Wraps a fully-built URL (credentials included).
    ///
    /// Construction is deliberate so accidental `format!`-into-log paths are
    /// visible during review.
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// Grants access to the raw URL for the media connect call only.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Host[:port] portion with credentials stripped, safe for logs.
    pub fn redacted(&self) -> String {
        match (self.0.find("://"), self.0.rfind('@')) {
            (Some(scheme_end), Some(at)) if at > scheme_end => {
                format!("{}***@{}", &self.0[..scheme_end + 3], &self.0[at + 1..])
            }
            _ => self.0.clone(),
        }
    }
}

impl std::fmt::Debug for MediaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Rtsp { url } => f
                .debug_struct("Rtsp")
                .field("url", &url.redacted())
                .finish(),
        }
    }
}

impl std::fmt::Display for RtspUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted())
    }
}

impl MediaSource {
    /// Wraps a local path after canonicalizing intent (no symlink resolution
    /// here; the media layer opens it as-is).
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self::File(path.as_ref().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtsp_url_debug_and_display_are_redacted() {
        let url = RtspUrl::new("rtsp://admin:hunter2@cam.local:554/stream1");
        let rendered_display = url.to_string();
        let rendered_debug = format!("{:?}", MediaSource::Rtsp { url: url.clone() });

        for leaked in ["hunter2", "admin"] {
            assert!(
                !rendered_display.contains(leaked),
                "Display leak: {rendered_display}"
            );
            assert!(
                !rendered_debug.contains(leaked),
                "Debug leak: {rendered_debug}"
            );
        }
        assert!(url.redacted().starts_with("rtsp://***@"));
        assert_eq!(url.expose(), "rtsp://admin:hunter2@cam.local:554/stream1");
    }
}
