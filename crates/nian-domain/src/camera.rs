//! Camera endpoint and credential model.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// Maximum user-facing camera display-name length in bytes.
pub const MAX_DISPLAY_NAME_LEN: usize = 128;
/// Maximum persisted RTSP path length in bytes.
pub const MAX_RTSP_PATH_LEN: usize = 4 * 1024;
/// Maximum camera username length in bytes.
pub const MAX_CAMERA_USERNAME_LEN: usize = 256;
/// Maximum camera password length in bytes.
pub const MAX_CAMERA_PASSWORD_LEN: usize = 512;

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

    /// Validates credentials before they can be persisted or sent over IPC.
    pub fn validate(&self) -> Result<(), crate::error::DomainError> {
        if self.username.trim().is_empty() || self.username.len() > MAX_CAMERA_USERNAME_LEN {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: format!(
                    "camera username must be non-empty and at most {MAX_CAMERA_USERNAME_LEN} bytes"
                ),
            });
        }
        if self.password().is_empty() || self.password().len() > MAX_CAMERA_PASSWORD_LEN {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: format!(
                    "camera password must be non-empty and at most {MAX_CAMERA_PASSWORD_LEN} bytes"
                ),
            });
        }
        Ok(())
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
    /// Validates a hostname or IP literal.
    ///
    /// * hostnames: dot-separated labels of `a-z0-9-`, each 1–63 bytes,
    ///   none starting or ending with `-`;
    /// * IPv6 literals (contain `:`): bracket-free here — brackets are added
    ///   by URL generation ([`Host::url_component`]). The structural check
    ///   accepts 1–4 hex-digit groups with at most one `::` compression;
    ///   IPv4-mapped tails (`::ffff:1.2.3.4`) are not supported — write them
    ///   as full hex groups instead.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, crate::error::DomainError> {
        let value = value.as_ref();

        let basic_valid = !value.is_empty()
            && value.len() <= 253
            && !value.contains(['/', '@', '?', '#', ' ', '\t', '\r', '\n'])
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'));
        if !basic_valid {
            return Err(invalid_host(value));
        }

        if value.contains(':') {
            validate_ipv6_literal(value)?;
        } else {
            validate_hostname(value)?;
        }

        Ok(Self(value.to_owned()))
    }

    /// Borrows the host string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host rendered for embedding in a URL authority: IPv6 literals
    /// come out bracketed (`[2001:db8::1]`), everything else unchanged.
    pub fn url_component(&self) -> String {
        if self.is_ipv6() {
            format!("[{}]", self.0)
        } else {
            self.0.clone()
        }
    }

    /// Whether this host is an IPv6 literal.
    pub fn is_ipv6(&self) -> bool {
        self.0.contains(':')
    }
}

fn invalid_host(value: &str) -> crate::error::DomainError {
    crate::error::DomainError::InvalidEndpoint {
        reason: format!("invalid camera host {value:?}"),
    }
}

/// Structural hostname checks beyond the shared character-set rules.
fn validate_hostname(value: &str) -> Result<(), crate::error::DomainError> {
    let invalid = || invalid_host(value);
    for label in value.split('.') {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Structural IPv6 check: groups of 1–4 hex digits, exactly 8 groups, or
/// fewer when a single `::` compression stands in for at least one group.
fn validate_ipv6_literal(value: &str) -> Result<(), crate::error::DomainError> {
    let invalid = || invalid_host(value);
    // ":::" and repeated "::" are never valid; match_indices is non-overlapping
    // so ":::" alone still needs its own check.
    if value.contains(":::") || value.match_indices("::").count() > 1 {
        return Err(invalid());
    }

    let (head, tail) = match value.split_once("::") {
        Some((head, tail)) => (head, Some(tail)),
        None => (value, None),
    };

    let count_groups = |side: &str| -> Result<usize, crate::error::DomainError> {
        if side.is_empty() {
            return Ok(0);
        }
        for group in side.split(':') {
            let valid_group =
                (1..=4).contains(&group.len()) && group.bytes().all(|b| b.is_ascii_hexdigit());
            if !valid_group {
                return Err(invalid());
            }
        }
        Ok(side.split(':').count())
    };

    let groups = count_groups(head)? + count_groups(tail.unwrap_or(""))?;
    let compressed = tail.is_some();
    if compressed && groups >= 8 {
        // "::" must stand in for at least one group.
        return Err(invalid());
    }
    if !compressed && groups != 8 {
        return Err(invalid());
    }
    Ok(())
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
    /// Port 0 has no meaning for an RTSP target and is rejected.
    pub fn new(
        host: Host,
        port: u16,
        path: impl AsRef<str>,
    ) -> Result<Self, crate::error::DomainError> {
        let path = path.as_ref();

        if port == 0 {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: "port must be between 1 and 65535".to_owned(),
            });
        }
        if path.len() > MAX_RTSP_PATH_LEN {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: format!("stream path must be at most {MAX_RTSP_PATH_LEN} bytes"),
            });
        }
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

    /// Credential-free URL, safe for logs and UI. IPv6 hosts are rendered
    /// bracketed (`rtsp://[2001:db8::1]:554/stream1`).
    pub fn display_url(&self) -> String {
        format!(
            "rtsp://{}:{}{}",
            self.host.url_component(),
            self.port,
            self.path
        )
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
                self.host.url_component(),
                self.port,
                self.path
            ),
        }
    }
}

/// Which audio streams are recorded alongside the primary video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioPolicy {
    /// Stream-copy every discovered audio stream.
    #[default]
    CopyAll,
    /// Record video only.
    Exclude,
}

impl AudioPolicy {
    /// Stable value persisted in settings and used by desktop DTOs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CopyAll => "copy_all",
            Self::Exclude => "exclude",
        }
    }

    /// Parses the stable persisted value.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "copy_all" => Some(Self::CopyAll),
            "exclude" => Some(Self::Exclude),
            _ => None,
        }
    }
}

/// Opaque reference to credentials stored outside ordinary application files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Validates a non-empty bounded credential reference.
    pub fn parse(value: impl Into<String>) -> Result<Self, crate::error::DomainError> {
        let value = value.into();
        if value.is_empty() || value.len() > 255 || value.bytes().any(|b| b.is_ascii_control()) {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: "invalid credential reference".to_owned(),
            });
        }
        Ok(Self(value))
    }

    /// Borrows the opaque reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Persisted non-secret camera source definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraSource {
    /// RTSP endpoint without userinfo/password material.
    Rtsp(CameraEndpoint),
}

/// Authoritative persisted camera configuration. Credentials are represented
/// only by an opaque reference into the native credential store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraConfig {
    camera_id: crate::CameraId,
    display_name: String,
    source: CameraSource,
    audio_policy: AudioPolicy,
    credential_ref: CredentialRef,
}

impl CameraConfig {
    /// Creates and validates a camera configuration.
    pub fn new(
        camera_id: crate::CameraId,
        display_name: impl Into<String>,
        source: CameraSource,
        audio_policy: AudioPolicy,
        credential_ref: CredentialRef,
    ) -> Result<Self, crate::error::DomainError> {
        let display_name = display_name.into();
        Self::validate_display_name(&display_name)?;
        Ok(Self {
            camera_id,
            display_name,
            source,
            audio_policy,
            credential_ref,
        })
    }

    /// Validates a user-facing display name without constructing a config.
    pub fn validate_display_name(display_name: &str) -> Result<(), crate::error::DomainError> {
        if display_name.trim().is_empty() || display_name.len() > MAX_DISPLAY_NAME_LEN {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: format!(
                    "display name must be non-empty and at most {MAX_DISPLAY_NAME_LEN} bytes"
                ),
            });
        }
        if display_name.bytes().any(|b| b.is_ascii_control()) {
            return Err(crate::error::DomainError::InvalidEndpoint {
                reason: "display name contains control characters".to_owned(),
            });
        }
        Ok(())
    }

    pub fn camera_id(&self) -> &crate::CameraId {
        &self.camera_id
    }
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
    pub fn source(&self) -> &CameraSource {
        &self.source
    }
    pub fn audio_policy(&self) -> AudioPolicy {
        self.audio_policy
    }
    pub fn credential_ref(&self) -> &CredentialRef {
        &self.credential_ref
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
        let oversized_path = format!("/{}", "a".repeat(MAX_RTSP_PATH_LEN));
        assert!(CameraEndpoint::rtsp(Host::parse("h1").unwrap(), oversized_path).is_err());
    }

    #[test]
    fn credential_lengths_are_bounded_well_below_ipc_limit() {
        assert!(Credentials::new("admin", "password").validate().is_ok());
        assert!(
            Credentials::new("u".repeat(MAX_CAMERA_USERNAME_LEN + 1), "password")
                .validate()
                .is_err()
        );
        assert!(
            Credentials::new("admin", "p".repeat(MAX_CAMERA_PASSWORD_LEN + 1))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn ipv6_hosts_render_bracketed_in_urls() {
        let endpoint = CameraEndpoint::rtsp(Host::parse("2001:db8::1").unwrap(), "/stream1")
            .expect("bracket-free IPv6 literal is accepted");
        assert_eq!(
            endpoint.display_url(),
            "rtsp://[2001:db8::1]:554/stream1",
            "URL generation must add the brackets"
        );

        let creds = Credentials::new("admin", "pw");
        assert_eq!(
            endpoint.url_with(Some(&creds)),
            "rtsp://admin:pw@[2001:db8::1]:554/stream1"
        );
        // The stored host itself stays bracket-free.
        assert_eq!(endpoint.host().as_str(), "2001:db8::1");
    }

    #[test]
    fn accepts_structurally_valid_ipv6_literals() {
        for valid in [
            "::",
            "::1",
            "2001:db8::1",
            "fe80:0:0:0:0:0:0:1",
            "1:2:3:4:5:6:7:8",
            "1::",
            "aa:bb:cc:dd:ee:ff:11:22",
        ] {
            assert!(Host::parse(valid).is_ok(), "expected acceptance of {valid}");
        }
    }

    #[test]
    fn rejects_malformed_ipv6_literals() {
        for invalid in [
            "::::",
            "1:::2",
            "1:2:3:4:5:6:7:8:9", // too many groups
            "12345::",           // group longer than 4 hex digits
            "1:2:3:4:5:6:7:8::", // compression with a full group count
            "1::2::3",           // two compressions
            "g::1",              // non-hex digit
        ] {
            assert!(
                Host::parse(invalid).is_err(),
                "expected rejection of {invalid}"
            );
        }
    }

    #[test]
    fn rejects_degenerate_hostnames_but_accepts_normal_ones() {
        for invalid in ["..", ".a", "a.", "a..b", "-lead", "trail-", "a-.b"] {
            assert!(
                Host::parse(invalid).is_err(),
                "expected rejection of {invalid}"
            );
        }
        for valid in ["cam.local", "192.168.1.42", "a-b.c-d", "h1"] {
            assert!(Host::parse(valid).is_ok(), "expected acceptance of {valid}");
        }
    }

    #[test]
    fn port_zero_is_rejected() {
        let host = Host::parse("192.168.1.42").unwrap();
        assert!(CameraEndpoint::new(host.clone(), 0, "/stream1").is_err());
        assert!(CameraEndpoint::new(host, 554, "/stream1").is_ok());
    }

    #[test]
    fn camera_state_names_are_stable() {
        assert_eq!(CameraState::default(), CameraState::Idle);
        assert_eq!(CameraState::Recording.to_string(), "recording");
    }
}
