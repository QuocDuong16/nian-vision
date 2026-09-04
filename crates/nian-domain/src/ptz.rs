use crate::{CameraId, CredentialRef, DomainError, Host};

pub const MAX_ONVIF_DEVICE_PATH_LEN: usize = 4096;
pub const MAX_ONVIF_ENDPOINT_REFERENCE_LEN: usize = 512;

/// Persisted device-service transport. The application reconstructs the URL
/// backend-side; raw authenticated/service URLs never cross the UI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnvifScheme {
    Http,
    Https,
}

impl OnvifScheme {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            _ => None,
        }
    }
}

/// Safe optional ONVIF control-plane association for a configured RTSP camera.
///
/// This intentionally contains no password, PTZ service XAddr, SOAP payload,
/// Digest challenge, or PTZ/profile/configuration token. Those are resolved
/// and revalidated by the backend when a control session is established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtzBinding {
    camera_id: CameraId,
    scheme: OnvifScheme,
    host: Host,
    port: u16,
    device_path: String,
    endpoint_reference: String,
    credential_ref: CredentialRef,
    owns_credential: bool,
}

impl PtzBinding {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        camera_id: CameraId,
        scheme: OnvifScheme,
        host: Host,
        port: u16,
        device_path: impl Into<String>,
        endpoint_reference: impl Into<String>,
        credential_ref: CredentialRef,
        owns_credential: bool,
    ) -> Result<Self, DomainError> {
        let device_path = device_path.into();
        if port == 0 {
            return Err(DomainError::InvalidEndpoint {
                reason: "invalid ONVIF device-service port".to_owned(),
            });
        }
        if !device_path.starts_with('/')
            || device_path.len() > MAX_ONVIF_DEVICE_PATH_LEN
            || device_path
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '@' || c == '?' || c == '#')
        {
            return Err(DomainError::InvalidEndpoint {
                reason: "invalid ONVIF device-service path".to_owned(),
            });
        }
        let endpoint_reference = endpoint_reference.into();
        if endpoint_reference.trim().is_empty()
            || endpoint_reference.len() > MAX_ONVIF_ENDPOINT_REFERENCE_LEN
            || endpoint_reference.chars().any(char::is_control)
        {
            return Err(DomainError::InvalidEndpoint {
                reason: "invalid ONVIF endpoint reference".to_owned(),
            });
        }
        Ok(Self {
            camera_id,
            scheme,
            host,
            port,
            device_path,
            endpoint_reference,
            credential_ref,
            owns_credential,
        })
    }

    pub fn camera_id(&self) -> &CameraId {
        &self.camera_id
    }
    pub const fn scheme(&self) -> OnvifScheme {
        self.scheme
    }
    pub fn host(&self) -> &Host {
        &self.host
    }
    pub const fn port(&self) -> u16 {
        self.port
    }
    pub fn device_path(&self) -> &str {
        &self.device_path
    }
    pub fn endpoint_reference(&self) -> &str {
        &self.endpoint_reference
    }
    pub fn credential_ref(&self) -> &CredentialRef {
        &self.credential_ref
    }
    pub const fn owns_credential(&self) -> bool {
        self.owns_credential
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_rejects_query_bearing_or_unbounded_device_paths() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let host = Host::parse("192.168.1.8").unwrap();
        let credentials = CredentialRef::parse("nian-vision/front-door/fixture").unwrap();
        assert!(
            PtzBinding::new(
                camera_id.clone(),
                OnvifScheme::Http,
                host.clone(),
                80,
                "/onvif/device_service?token=secret",
                "urn:uuid:fixture",
                credentials.clone(),
                false,
            )
            .is_err()
        );
        assert!(
            PtzBinding::new(
                camera_id,
                OnvifScheme::Http,
                host,
                80,
                format!("/{}", "x".repeat(MAX_ONVIF_DEVICE_PATH_LEN)),
                "urn:uuid:fixture",
                credentials,
                false,
            )
            .is_err()
        );
    }
}
