//! Camera endpoint and credential model.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// Default RTSP port.
pub const DEFAULT_RTSP_PORT: u16 = 554;

fn encode_userinfo_component(value: &str) -> String {
    // Percent-encode everything outside the RFC 3986 unreserved set so that
    // `:`, `/`, and `@` inside credentials cannot corrupt the URL structure.
    let unreserved =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if unreserved(byte) {
            out.push(byte as char);
        } else {
            let encoded = utf8_percent_byte(byte);
            out.push_str(&encoded);
        }
    }
    out
}

fn utf8_percent_byte(byte: u8) -> String {
    // Percent bytes are only defined for the userinfo charset we accept;
    // multi-byte UTF-8 sequences are encoded per byte, which is what
    // `percent_encoding` would do as well.
    format!("%{byte:02X}")
}

/// Username/password pair used to authenticate against a camera.
///
/// The password is wrapped in [`Secret`] and never rendered.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    password: Secret<String>,
}

impl Credentials {
    /// Creates a credential pair.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: Secret::new(password.into()),
        }
    }

    /// Grants access to the password for connection attempts only.
    pub fn password(&self) -> &str {
        self.password.expose()
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

/// Host portion of a camera endpoint: a DNS hostname or IP literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Host(String);

impl Host {
    /// Validates a hostname or IP literal (IPv6 must be bracket-free here;
    /// brackets are added when building URLs).
    pub fn parse(value: impl AsRef<str>) -> Result<Self, crate::error::DomainError> {
        let value = value.as_ref();

        let valid = !value.is_empty()
            && value.len() <= 253
            && !value.contains(['/', '@', '?', '#', ' ', '\t', '\r', '\n'])
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'));

        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(crate::error::DomainError::InvalidEndpoint {
                reason: format!("invalid camera host {value:?}"),
            })
        }
    }

    /// Borrows the host string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Connection target of an IP camera.
///
/// The endpoint never carries credentials; they are supplied explicitly when
/// building a connectable URL via [`CameraEndpoint::url_with`]. This keeps
/// `Debug`/`Display` output safe to log.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CameraEndpoint {
    host: Host,
    port: u16,
    path: String,
}

impl CameraEndpoint {
    /// Validates and creates an endpoint.
    ///
    /// `path` must start with `/` (a bare stream path such as `/stream2`).
    pub fn new(
        host: Host,
        port: u16,
        path: impl AsRef<str>,
    ) -> Result<Self, crate::error::DomainError> {
        let path = path.as_ref();

        if !path.starts_with('/') {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: "stream path must start with '/'".to_owned(),
            });
        }
        if path.contains([' ', '\t', '\r', '\n', '@'])
            || path.as_bytes().iter().any(u8::is_ascii_control)
        {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: "stream path contains forbidden characters".to_owned(),
            });
        }

        Ok(Self {
            host,
            port,
            path: path.to_owned(),
        })
    }

    /// Creates an endpoint on the default RTSP port.
    pub fn rtsp(host: Host, path: impl AsRef<str>) -> Result<Self, crate::error::DomainError> {
        Self::new(host, DEFAULT_RTSP_PORT, path)
    }

    /// Host component.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Port component.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Path component (starts with `/`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Credential-free URL, safe for logs and UI.
    pub fn display_url(&self) -> String {
        format!("rtsp://{}:{}{}", self.host, self.port, self.path)
    }

    /// Builds a connectable URL, embedding percent-encoded credentials when
    /// provided.
    ///
    /// The result contains the password in plaintext by design — callers must
    /// hand it only to the media layer and never log it.
    pub fn url_with(&self, credentials: Option<&Credentials>) -> String {
        match credentials {
            None => self.display_url(),
            Some(creds) => format!(
                "rtsp://{}:{}@{}:{}{}",
                encode_userinfo_component(&creds.username),
                encode_userinfo_component(creds.password()),
                self.host,
                self.port,
                self.path
            ),
        }
    }
}

/// Lifecycle state of a camera as shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CameraState {
    /// Configured but not asked to record.
    #[default]
    Idle,
    /// Connection attempt in progress.
    Connecting,
    /// Segments are being written.
    Recording,
    /// Connection lost; retrying with backoff.
    Reconnecting,
    /// Unreachable after retries or administratively offline.
    Offline,
    /// Permanent failure requiring operator attention (e.g. bad credentials).
    Error,
}

impl CameraState {
    /// Stable lowercase name used in logs and IPC payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Recording => "recording",
            Self::Reconnecting => "reconnecting",
            Self::Offline => "offline",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for CameraState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> CameraEndpoint {
        CameraEndpoint::rtsp(Host::parse("192.168.1.42").unwrap(), "/stream1").unwrap()
    }

    #[test]
    fn display_url_has_no_credentials() {
        assert_eq!(endpoint().display_url(), "rtsp://192.168.1.42:554/stream1");
    }

    #[test]
    fn debug_of_endpoint_never_contains_password() {
        let creds = Credentials::new("admin", "p@ss:word/x");
        let url = endpoint().url_with(Some(&creds));
        assert_eq!(
            url,
            "rtsp://admin:p%40ss%3Aword%2Fx@192.168.1.42:554/stream1"
        );
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("p@ss"), "leaked via Debug: {rendered}");
    }

    #[test]
    fn special_characters_in_credentials_are_escaped() {
        let creds = Credentials::new("user", "pass~_-word");
        let url = endpoint().url_with(Some(&creds));
        assert_eq!(url, "rtsp://user:pass~_-word@192.168.1.42:554/stream1");
    }

    #[test]
    fn rejects_bad_hosts_and_paths() {
        assert!(Host::parse("").is_err());
        assert!(Host::parse("host with space").is_err());
        assert!(Host::parse("host/path").is_err());
        assert!(CameraEndpoint::rtsp(Host::parse("h1").unwrap(), "no-slash").is_err());
        assert!(CameraEndpoint::rtsp(Host::parse("h1").unwrap(), "/a@b").is_err());
    }

    #[test]
    fn camera_state_names_are_stable() {
        assert_eq!(CameraState::default(), CameraState::Idle);
        assert_eq!(CameraState::Recording.to_string(), "recording");
    }
}
