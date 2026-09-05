use crate::{
    CameraId, CredentialRef, DomainError, Host, MAX_ONVIF_DEVICE_PATH_LEN,
    MAX_ONVIF_ENDPOINT_REFERENCE_LEN, OnvifScheme,
};

/// Safe optional ONVIF Event-plane association for a configured RTSP camera.
///
/// Event-service XAddrs, PullPoint SubscriptionReferences, subscription IDs,
/// SOAP/XML payloads and credentials are intentionally absent. They are
/// rediscovered/revalidated by the backend each time monitoring starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBinding {
    camera_id: CameraId,
    scheme: OnvifScheme,
    host: Host,
    port: u16,
    device_path: String,
    endpoint_reference: String,
    credential_ref: CredentialRef,
    owns_credential: bool,
}

impl EventBinding {
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
                reason: "invalid ONVIF event device-service port".to_owned(),
            });
        }
        if !device_path.starts_with('/')
            || device_path.len() > MAX_ONVIF_DEVICE_PATH_LEN
            || device_path
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '@' || c == '?' || c == '#')
        {
            return Err(DomainError::InvalidEndpoint {
                reason: "invalid ONVIF event device-service path".to_owned(),
            });
        }
        let endpoint_reference = endpoint_reference.into();
        if endpoint_reference.trim().is_empty()
            || endpoint_reference.len() > MAX_ONVIF_ENDPOINT_REFERENCE_LEN
            || endpoint_reference.chars().any(char::is_control)
        {
            return Err(DomainError::InvalidEndpoint {
                reason: "invalid ONVIF event endpoint reference".to_owned(),
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
    fn binding_rejects_unsafe_device_service_metadata() {
        let camera_id = CameraId::parse("front-door").unwrap();
        let host = Host::parse("192.168.1.8").unwrap();
        let credentials = CredentialRef::parse("nian-vision/front-door/events/fixture").unwrap();
        assert!(
            EventBinding::new(
                camera_id.clone(),
                OnvifScheme::Http,
                host.clone(),
                80,
                "/onvif/device_service?token=secret",
                "urn:uuid:fixture",
                credentials.clone(),
                true,
            )
            .is_err()
        );
        assert!(
            EventBinding::new(
                camera_id,
                OnvifScheme::Http,
                host,
                80,
                "/onvif/device_service",
                "",
                credentials,
                true,
            )
            .is_err()
        );
    }
}
