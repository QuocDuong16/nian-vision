use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use md5::Md5;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::redirect::Policy;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use url::Url;
use uuid::Uuid;

use crate::authority::{
    parse_stream_uri as normalize_stream_uri, validate_event_xaddr, validate_service_xaddr,
};
use crate::types::{
    EventControl, MediaProfile, MediaServiceKind, MotionNotification, OnvifCredentials,
    OnvifInterrogation, PtzControl, PtzProfileAssociation, PullPointSubscription, ServiceEndpoint,
};
use crate::xml::{
    parse_device_information, parse_event_properties, parse_hostname, parse_motion_notifications,
    parse_profiles, parse_ptz_configuration_options, parse_ptz_profile_associations,
    parse_pullpoint_subscription, parse_renew_times, parse_services, parse_stream_uri,
};
use crate::{
    EVENT_INITIAL_SUBSCRIPTION_SECS, EVENT_PULL_MESSAGE_LIMIT, EVENT_PULL_TIMEOUT_MS,
    HTTP_TIMEOUT_MS, MAX_SOAP_RESPONSE_BYTES, OnvifError, PTZ_MOVE_TIMEOUT_MS, StreamEndpoint,
};

const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA1_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2_NS: &str = "http://www.onvif.org/ver20/media/wsdl";
const PTZ_NS: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const EVENT_NS: &str = "http://www.onvif.org/ver10/events/wsdl";

#[derive(Clone)]
pub struct OnvifClient {
    http: Client,
    username_token_authorities: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for OnvifClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnvifClient").finish_non_exhaustive()
    }
}

impl OnvifClient {
    pub fn new() -> Result<Self, OnvifError> {
        Self::with_timeout(Duration::from_millis(HTTP_TIMEOUT_MS))
    }

    fn with_timeout(timeout: Duration) -> Result<Self, OnvifError> {
        Ok(Self {
            http: build_http_client(timeout)?,
            username_token_authorities: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn interrogate(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<OnvifInterrogation, OnvifError> {
        let info_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetDeviceInformation"),
            "<tds:GetDeviceInformation/>",
        )?;
        let mut device = parse_device_information(&info_xml)?;

        if let Ok(hostname_xml) = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetHostname"),
            "<tds:GetHostname/>",
        ) {
            device.hostname = parse_hostname(&hostname_xml).ok().flatten();
        }

        let services_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetServices"),
            "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        )?;
        let services = parse_services(&services_xml)?;
        let (media2, rejected_media2) =
            validated_service_xaddrs(&services, "/ver20/media/wsdl", device_service);
        let (media1, rejected_media1) =
            validated_service_xaddrs(&services, "/ver10/media/wsdl", device_service);
        if media2.is_empty() && media1.is_empty() {
            return Err(if rejected_media2 || rejected_media1 {
                OnvifError::AuthorityRejected
            } else {
                OnvifError::Unsupported
            });
        }
        let (media_service, media_service_kind, mut profiles) =
            self.select_media_profiles(&media2, &media1, credentials)?;

        profiles.sort_by(|left, right| {
            right
                .is_h264_compatible()
                .cmp(&left.is_h264_compatible())
                .then_with(|| profile_pixels(right).cmp(&profile_pixels(left)))
                .then_with(|| left.token.cmp(&right.token))
        });
        if !profiles.iter().any(MediaProfile::is_h264_compatible) {
            return Err(OnvifError::NoCompatibleProfile);
        }

        Ok(OnvifInterrogation {
            device,
            media_service,
            media_service_kind,
            profiles,
        })
    }

    pub fn stream_endpoint(
        &self,
        device_service: &str,
        media_service: &str,
        credentials: &OnvifCredentials,
        profile: &MediaProfile,
    ) -> Result<StreamEndpoint, OnvifError> {
        if !profile.is_h264_compatible() {
            return Err(OnvifError::NoCompatibleProfile);
        }
        let token = xml_escape(&profile.token);
        let (action, body) = match profile.service_kind {
            MediaServiceKind::Media2 => (
                format!("{MEDIA2_NS}/GetStreamUri"),
                format!(
                    "<tr2:GetStreamUri><tr2:Protocol>RTSP</tr2:Protocol><tr2:ProfileToken>{token}</tr2:ProfileToken></tr2:GetStreamUri>"
                ),
            ),
            MediaServiceKind::LegacyMedia => (
                format!("{MEDIA1_NS}/GetStreamUri"),
                format!(
                    "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{token}</trt:ProfileToken></trt:GetStreamUri>"
                ),
            ),
        };
        let xml = self.soap(media_service, credentials, &action, &body)?;
        let raw = parse_stream_uri(&xml)?;
        normalize_stream_uri(&raw, device_service)
    }

    pub fn ptz_control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<PtzControl, OnvifError> {
        let services_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetServices"),
            "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        )?;
        let services = parse_services(&services_xml)?;
        let (ptz_services, rejected_ptz) =
            validated_service_xaddrs(&services, "/ver20/ptz/wsdl", device_service);
        if ptz_services.is_empty() {
            return Err(if rejected_ptz {
                OnvifError::AuthorityRejected
            } else {
                OnvifError::Unsupported
            });
        }
        let (media2, rejected_media2) =
            validated_service_xaddrs(&services, "/ver20/media/wsdl", device_service);
        let (media1, rejected_media1) =
            validated_service_xaddrs(&services, "/ver10/media/wsdl", device_service);
        if media2.is_empty() && media1.is_empty() {
            return Err(if rejected_media2 || rejected_media1 {
                OnvifError::AuthorityRejected
            } else {
                OnvifError::Unsupported
            });
        }

        let mut associations = Vec::new();
        let mut last_error = OnvifError::Unsupported;
        for (kind, candidates) in [
            (MediaServiceKind::Media2, &media2),
            (MediaServiceKind::LegacyMedia, &media1),
        ] {
            for media in candidates {
                match self.get_ptz_associations(media, credentials, kind) {
                    Ok(found) => associations.extend(found),
                    Err(OnvifError::AuthFailed) => return Err(OnvifError::AuthFailed),
                    Err(error) => last_error = error,
                }
            }
            if !associations.is_empty() {
                break;
            }
        }
        if associations.is_empty() {
            return Err(last_error);
        }

        for service in ptz_services {
            for association in &associations {
                let token = xml_escape(&association.configuration_token);
                let body = format!(
                    "<tptz:GetConfigurationOptions><tptz:ConfigurationToken>{token}</tptz:ConfigurationToken></tptz:GetConfigurationOptions>"
                );
                match self.soap(
                    &service,
                    credentials,
                    &format!("{PTZ_NS}/GetConfigurationOptions"),
                    &body,
                ) {
                    Ok(xml) => {
                        let options = parse_ptz_configuration_options(&xml)?;
                        if options.pan.is_some() && options.tilt.is_some() {
                            return Ok(PtzControl {
                                service,
                                profile_token: association.profile_token.clone(),
                                pan: options.pan,
                                tilt: options.tilt,
                                zoom: options.zoom,
                            });
                        }
                        last_error = OnvifError::Unsupported;
                    }
                    Err(OnvifError::AuthFailed) => return Err(OnvifError::AuthFailed),
                    Err(error) => last_error = error,
                }
            }
        }
        Err(last_error)
    }

    pub fn continuous_move(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: Option<(f64, f64)>,
        zoom: Option<f64>,
    ) -> Result<(), OnvifError> {
        if pan_tilt.is_none() && zoom.is_none() {
            return Err(OnvifError::Protocol);
        }
        let mut velocity = String::new();
        if let Some((pan, tilt)) = pan_tilt {
            let (pan_range, tilt_range) = control
                .pan
                .zip(control.tilt)
                .ok_or(OnvifError::Unsupported)?;
            let pan = pan_range.map_normalized(pan)?;
            let tilt = tilt_range.map_normalized(tilt)?;
            velocity.push_str(&format!("<tt:PanTilt x=\"{pan:.6}\" y=\"{tilt:.6}\"/>"));
        }
        if let Some(zoom) = zoom {
            let zoom_range = control.zoom.ok_or(OnvifError::Unsupported)?;
            let zoom = zoom_range.map_normalized(zoom)?;
            velocity.push_str(&format!("<tt:Zoom x=\"{zoom:.6}\"/>"));
        }
        let profile = xml_escape(&control.profile_token);
        let timeout_seconds = PTZ_MOVE_TIMEOUT_MS as f64 / 1000.0;
        let body = format!(
            "<tptz:ContinuousMove><tptz:ProfileToken>{profile}</tptz:ProfileToken><tptz:Velocity>{velocity}</tptz:Velocity><tptz:Timeout>PT{timeout_seconds:.3}S</tptz:Timeout></tptz:ContinuousMove>"
        );
        self.soap(
            &control.service,
            credentials,
            &format!("{PTZ_NS}/ContinuousMove"),
            &body,
        )?;
        Ok(())
    }

    pub fn stop(
        &self,
        control: &PtzControl,
        credentials: &OnvifCredentials,
        pan_tilt: bool,
        zoom: bool,
    ) -> Result<(), OnvifError> {
        if !pan_tilt && !zoom {
            return Ok(());
        }
        if pan_tilt && !control.pan_tilt_supported() {
            return Err(OnvifError::Unsupported);
        }
        if zoom && !control.zoom_supported() {
            return Err(OnvifError::Unsupported);
        }
        let profile = xml_escape(&control.profile_token);
        let body = format!(
            "<tptz:Stop><tptz:ProfileToken>{profile}</tptz:ProfileToken><tptz:PanTilt>{pan_tilt}</tptz:PanTilt><tptz:Zoom>{zoom}</tptz:Zoom></tptz:Stop>"
        );
        self.soap(
            &control.service,
            credentials,
            &format!("{PTZ_NS}/Stop"),
            &body,
        )?;
        Ok(())
    }

    pub fn event_control(
        &self,
        device_service: &str,
        credentials: &OnvifCredentials,
    ) -> Result<EventControl, OnvifError> {
        let services_xml = self.soap(
            device_service,
            credentials,
            &format!("{DEVICE_NS}/GetServices"),
            "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        )?;
        let services = parse_services(&services_xml)?;
        let (event_services, rejected_event) =
            validated_event_service_xaddrs(&services, device_service);
        if event_services.is_empty() {
            return Err(if rejected_event {
                OnvifError::AuthorityRejected
            } else {
                OnvifError::Unsupported
            });
        }

        let mut last_error = OnvifError::Unsupported;
        for service in event_services {
            match self.soap(
                &service,
                credentials,
                &format!("{EVENT_NS}/EventPortType/GetEventPropertiesRequest"),
                "<tev:GetEventProperties/>",
            ) {
                Ok(xml) => {
                    let properties = parse_event_properties(&xml)?;
                    if properties.motion_supported {
                        return Ok(EventControl {
                            device_service: device_service.to_owned(),
                            event_service: service,
                            properties,
                        });
                    }
                    last_error = OnvifError::Unsupported;
                }
                Err(OnvifError::AuthFailed) => return Err(OnvifError::AuthFailed),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    pub fn create_pullpoint_subscription(
        &self,
        control: &EventControl,
        credentials: &OnvifCredentials,
    ) -> Result<PullPointSubscription, OnvifError> {
        if !control.properties.motion_supported {
            return Err(OnvifError::Unsupported);
        }
        let body = format!(
            "<tev:CreatePullPointSubscription><tev:InitialTerminationTime>PT{}S</tev:InitialTerminationTime></tev:CreatePullPointSubscription>",
            EVENT_INITIAL_SUBSCRIPTION_SECS
        );
        let xml = self.soap(
            &control.event_service,
            credentials,
            &format!("{EVENT_NS}/EventPortType/CreatePullPointSubscriptionRequest"),
            &body,
        )?;
        let mut subscription = parse_pullpoint_subscription(&xml)?;
        subscription.endpoint =
            validate_event_xaddr(&subscription.endpoint, &control.device_service)?;
        if let (Some(current), Some(termination)) = (
            subscription.current_time_utc,
            subscription.termination_time_utc,
        ) && termination <= current
        {
            return Err(OnvifError::Protocol);
        }
        Ok(subscription)
    }

    pub fn set_synchronization_point(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        self.soap(
            &subscription.endpoint,
            credentials,
            &format!("{EVENT_NS}/PullPointSubscription/SetSynchronizationPointRequest"),
            "<tev:SetSynchronizationPoint/>",
        )?;
        Ok(())
    }

    pub fn pull_messages(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<Vec<MotionNotification>, OnvifError> {
        let timeout_seconds = EVENT_PULL_TIMEOUT_MS as f64 / 1000.0;
        let body = format!(
            "<tev:PullMessages><tev:Timeout>PT{timeout_seconds:.3}S</tev:Timeout><tev:MessageLimit>{EVENT_PULL_MESSAGE_LIMIT}</tev:MessageLimit></tev:PullMessages>"
        );
        let xml = self.soap(
            &subscription.endpoint,
            credentials,
            &format!("{EVENT_NS}/PullPointSubscription/PullMessagesRequest"),
            &body,
        )?;
        parse_motion_notifications(&xml)
    }

    pub fn renew_subscription(
        &self,
        subscription: &mut PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        let body = format!(
            "<wsnt:Renew><wsnt:TerminationTime>PT{}S</wsnt:TerminationTime></wsnt:Renew>",
            EVENT_INITIAL_SUBSCRIPTION_SECS
        );
        let xml = self.soap(
            &subscription.endpoint,
            credentials,
            &format!("{EVENT_NS}/SubscriptionManager/RenewRequest"),
            &body,
        )?;
        let (current, termination) = parse_renew_times(&xml)?;
        if let (Some(current), Some(termination)) = (current, termination)
            && termination <= current
        {
            return Err(OnvifError::Protocol);
        }
        subscription.current_time_utc = current;
        subscription.termination_time_utc = termination;
        Ok(())
    }

    pub fn unsubscribe(
        &self,
        subscription: &PullPointSubscription,
        credentials: &OnvifCredentials,
    ) -> Result<(), OnvifError> {
        self.soap(
            &subscription.endpoint,
            credentials,
            &format!("{EVENT_NS}/SubscriptionManager/UnsubscribeRequest"),
            "<wsnt:Unsubscribe/>",
        )?;
        Ok(())
    }
    fn get_ptz_associations(
        &self,
        service: &str,
        credentials: &OnvifCredentials,
        kind: MediaServiceKind,
    ) -> Result<Vec<PtzProfileAssociation>, OnvifError> {
        let (action, body) = match kind {
            MediaServiceKind::Media2 => (
                format!("{MEDIA2_NS}/GetProfiles"),
                "<tr2:GetProfiles><tr2:Type>All</tr2:Type></tr2:GetProfiles>",
            ),
            MediaServiceKind::LegacyMedia => {
                (format!("{MEDIA1_NS}/GetProfiles"), "<trt:GetProfiles/>")
            }
        };
        let xml = self.soap(service, credentials, &action, body)?;
        parse_ptz_profile_associations(&xml)
    }

    fn select_media_profiles(
        &self,
        media2: &[String],
        media1: &[String],
        credentials: &OnvifCredentials,
    ) -> Result<(String, MediaServiceKind, Vec<MediaProfile>), OnvifError> {
        let mut saw_profile_response = false;
        let mut last_retryable = None;
        for (kind, candidates) in [
            (MediaServiceKind::Media2, media2),
            (MediaServiceKind::LegacyMedia, media1),
        ] {
            for service in candidates {
                match self.get_profiles(service, credentials, kind) {
                    Ok(profiles) => {
                        saw_profile_response = true;
                        if profiles.iter().any(MediaProfile::is_h264_compatible) {
                            return Ok((service.clone(), kind, profiles));
                        }
                    }
                    Err(OnvifError::AuthFailed) => return Err(OnvifError::AuthFailed),
                    Err(
                        error @ (OnvifError::AuthorityRejected
                        | OnvifError::Timeout
                        | OnvifError::DeviceUnreachable
                        | OnvifError::Protocol
                        | OnvifError::Unsupported),
                    ) => {
                        last_retryable = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if saw_profile_response {
            Err(OnvifError::NoCompatibleProfile)
        } else {
            Err(last_retryable.unwrap_or(OnvifError::Unsupported))
        }
    }
    fn get_profiles(
        &self,
        service: &str,
        credentials: &OnvifCredentials,
        kind: MediaServiceKind,
    ) -> Result<Vec<MediaProfile>, OnvifError> {
        let (action, body) = match kind {
            MediaServiceKind::Media2 => (
                format!("{MEDIA2_NS}/GetProfiles"),
                "<tr2:GetProfiles><tr2:Type>All</tr2:Type></tr2:GetProfiles>",
            ),
            MediaServiceKind::LegacyMedia => {
                (format!("{MEDIA1_NS}/GetProfiles"), "<trt:GetProfiles/>")
            }
        };
        let xml = self.soap(service, credentials, &action, body)?;
        parse_profiles(&xml, kind)
    }

    fn send_soap_request(
        &self,
        url: &str,
        content_type: &str,
        envelope: String,
        authorization: Option<String>,
    ) -> Result<Response, OnvifError> {
        let mut request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, content_type)
            .body(envelope);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        request.send().map_err(map_http_error)
    }

    fn soap(
        &self,
        url: &str,
        credentials: &OnvifCredentials,
        action: &str,
        body: &str,
    ) -> Result<Vec<u8>, OnvifError> {
        let parsed = Url::parse(url).map_err(|_| OnvifError::Protocol)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.username() != ""
            || parsed.password().is_some()
        {
            return Err(OnvifError::AuthorityRejected);
        }

        let content_type = format!("application/soap+xml; charset=utf-8; action=\"{action}\"");
        let authority = auth_authority_key(&parsed)?;
        let prefer_username_token = self
            .username_token_authorities
            .lock()
            .map_err(|_| OnvifError::Internal)?
            .contains(&authority);

        if prefer_username_token {
            let response =
                self.send_soap_request(url, &content_type, soap_envelope(credentials, body), None)?;
            if response.status().as_u16() == 401
                && let Some(challenge) = digest_challenge_from_response(&response)
                && let Ok(authorization) = digest_authorization(credentials, &parsed, &challenge)
            {
                self.username_token_authorities
                    .lock()
                    .map_err(|_| OnvifError::Internal)?
                    .remove(&authority);
                let retry = self.send_soap_request(
                    url,
                    &content_type,
                    anonymous_soap_envelope(body),
                    Some(authorization),
                )?;
                return read_response(retry);
            }
            return read_response(response);
        }

        let anonymous = anonymous_soap_envelope(body);
        let response = self.send_soap_request(url, &content_type, anonymous.clone(), None)?;
        let status = response.status();
        if status.is_success() || status.is_redirection() || matches!(status.as_u16(), 404 | 405) {
            return read_response(response);
        }

        if status.as_u16() == 401
            && let Some(challenge) = digest_challenge_from_response(&response)
            && let Ok(authorization) = digest_authorization(credentials, &parsed, &challenge)
        {
            let retry =
                self.send_soap_request(url, &content_type, anonymous, Some(authorization))?;
            return read_response(retry);
        }

        if !matches!(status.as_u16(), 400 | 401 | 403 | 500) {
            return read_response(response);
        }

        let response =
            self.send_soap_request(url, &content_type, soap_envelope(credentials, body), None)?;
        let result = read_response(response);
        if result.is_ok() {
            self.username_token_authorities
                .lock()
                .map_err(|_| OnvifError::Internal)?
                .insert(authority);
        }
        result
    }
}

fn build_http_client(timeout: Duration) -> Result<Client, OnvifError> {
    Client::builder()
        .no_proxy()
        .timeout(timeout)
        .redirect(Policy::none())
        .build()
        .map_err(|_| OnvifError::Internal)
}

fn validated_service_xaddrs(
    services: &[ServiceEndpoint],
    namespace_fragment: &str,
    device_service: &str,
) -> (Vec<String>, bool) {
    let mut rejected = false;
    let mut candidates = Vec::new();
    for service in services
        .iter()
        .filter(|service| service.namespace.contains(namespace_fragment))
    {
        match validate_service_xaddr(&service.xaddr, device_service) {
            Ok(xaddr) if !candidates.contains(&xaddr) => candidates.push(xaddr),
            Ok(_) => {}
            Err(_) => rejected = true,
        }
    }
    candidates.sort_by(|left, right| {
        right
            .starts_with("https://")
            .cmp(&left.starts_with("https://"))
            .then_with(|| left.cmp(right))
    });
    (candidates, rejected)
}

fn validated_event_service_xaddrs(
    services: &[ServiceEndpoint],
    device_service: &str,
) -> (Vec<String>, bool) {
    let mut rejected = false;
    let mut candidates = Vec::new();
    for service in services
        .iter()
        .filter(|service| service.namespace.contains("/ver10/events/wsdl"))
    {
        match validate_event_xaddr(&service.xaddr, device_service) {
            Ok(xaddr) if !candidates.contains(&xaddr) => candidates.push(xaddr),
            Ok(_) => {}
            Err(_) => rejected = true,
        }
    }
    candidates.sort();
    (candidates, rejected)
}

fn auth_authority_key(url: &Url) -> Result<String, OnvifError> {
    let host = url.host_str().ok_or(OnvifError::AuthorityRejected)?;
    let port = url
        .port_or_known_default()
        .ok_or(OnvifError::AuthorityRejected)?;
    Ok(format!(
        "{}://{}:{port}",
        url.scheme(),
        host.to_ascii_lowercase()
    ))
}

fn digest_challenge_from_response(response: &Response) -> Option<DigestChallenge> {
    response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(parse_digest_challenge_value)
}

fn parse_digest_challenge_value(raw: &str) -> Option<DigestChallenge> {
    let start = raw.to_ascii_lowercase().find("digest ")?;
    parse_digest_challenge(&raw[start..])
}

fn profile_pixels(profile: &MediaProfile) -> u64 {
    u64::from(profile.width.unwrap_or(0)) * u64::from(profile.height.unwrap_or(0))
}

fn map_http_error(error: reqwest::Error) -> OnvifError {
    if error.is_timeout() {
        OnvifError::Timeout
    } else {
        OnvifError::DeviceUnreachable
    }
}

fn read_response(mut response: Response) -> Result<Vec<u8>, OnvifError> {
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(OnvifError::AuthFailed);
    }
    if status.is_redirection() {
        return Err(OnvifError::AuthorityRejected);
    }
    if !status.is_success() {
        return Err(if status.as_u16() == 404 || status.as_u16() == 405 {
            OnvifError::Unsupported
        } else {
            OnvifError::Protocol
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SOAP_RESPONSE_BYTES as u64)
    {
        return Err(OnvifError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((MAX_SOAP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| OnvifError::DeviceUnreachable)?;
    if bytes.len() > MAX_SOAP_RESPONSE_BYTES {
        return Err(OnvifError::ResponseTooLarge);
    }
    Ok(bytes)
}

fn anonymous_soap_envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:tt="http://www.onvif.org/ver10/schema">
<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn soap_envelope(credentials: &OnvifCredentials, body: &str) -> String {
    let nonce = Uuid::new_v4().as_bytes().to_vec();
    let created = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut digest_input = nonce.clone();
    digest_input.extend_from_slice(created.as_bytes());
    digest_input.extend_from_slice(credentials.password.as_bytes());
    let digest = Sha1::digest(&digest_input);
    let digest = base64::engine::general_purpose::STANDARD.encode(digest);
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce);
    let username = xml_escape(&credentials.username);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
<s:Header><wsse:Security s:mustUnderstand="1"><wsse:UsernameToken><wsse:Username>{username}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>
<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn xml_escape(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

#[derive(Debug)]
struct DigestChallenge {
    realm: String,
    nonce: String,
    qop: Option<String>,
    algorithm: String,
    opaque: Option<String>,
}

fn parse_digest_challenge(raw: &str) -> Option<DigestChallenge> {
    let raw = raw.trim();
    let rest = raw
        .strip_prefix("Digest ")
        .or_else(|| raw.strip_prefix("digest "))?;
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut algorithm = None;
    let mut opaque = None;
    for part in split_digest_parts(rest) {
        let (key, value) = part.split_once('=')?;
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(value),
            "nonce" => nonce = Some(value),
            "qop" => qop = Some(value),
            "algorithm" => algorithm = Some(value),
            "opaque" => opaque = Some(value),
            _ => {}
        }
    }
    Some(DigestChallenge {
        realm: realm?,
        nonce: nonce?,
        qop,
        algorithm: algorithm.unwrap_or_else(|| "MD5".to_owned()),
        opaque,
    })
}

fn split_digest_parts(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (index, ch) in raw.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch == ',' && !quoted {
            parts.push(raw[start..index].trim());
            start = index + 1;
        }
    }
    if start < raw.len() {
        parts.push(raw[start..].trim());
    }
    parts
}

fn digest_authorization(
    credentials: &OnvifCredentials,
    url: &Url,
    challenge: &DigestChallenge,
) -> Result<String, OnvifError> {
    let mut uri = url.path().to_owned();
    if uri.is_empty() {
        uri.push('/');
    }
    if let Some(query) = url.query() {
        uri.push('?');
        uri.push_str(query);
    }
    let qop = challenge.qop.as_deref().and_then(|qops| {
        qops.split(',')
            .map(str::trim)
            .find(|candidate| candidate.eq_ignore_ascii_case("auth"))
    });
    if challenge.qop.is_some() && qop.is_none() {
        return Err(OnvifError::Unsupported);
    }
    let cnonce = Uuid::new_v4().simple().to_string();
    let nc = "00000001";
    let ha1 = digest_hex(
        &challenge.algorithm,
        &format!(
            "{}:{}:{}",
            credentials.username, challenge.realm, credentials.password
        ),
    )?;
    let ha2 = digest_hex(&challenge.algorithm, &format!("POST:{uri}"))?;
    let response = if let Some(qop) = qop {
        digest_hex(
            &challenge.algorithm,
            &format!("{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}", challenge.nonce),
        )?
    } else {
        digest_hex(
            &challenge.algorithm,
            &format!("{ha1}:{}:{ha2}", challenge.nonce),
        )?
    };
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm={}",
        credentials.username, challenge.realm, challenge.nonce, uri, response, challenge.algorithm
    );
    if let Some(qop) = qop {
        header.push_str(&format!(", qop={qop}, nc={nc}, cnonce=\"{cnonce}\""));
    }
    if let Some(opaque) = &challenge.opaque {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    Ok(header)
}

fn digest_hex(algorithm: &str, input: &str) -> Result<String, OnvifError> {
    let bytes = match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => Md5::digest(input.as_bytes()).to_vec(),
        "SHA-256" => Sha256::digest(input.as_bytes()).to_vec(),
        _ => return Err(OnvifError::Unsupported),
    };
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").map_err(|_| OnvifError::Internal)?;
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_endpoint_selection_prefers_https_and_skips_invalid_candidates() {
        let services = vec![
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "ftp://192.168.1.8/media2".into(),
            },
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "http://192.168.1.8/media2".into(),
            },
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: "https://192.168.1.8/media2-secure".into(),
            },
        ];
        let (selected, rejected) =
            validated_service_xaddrs(&services, "/ver20/media/wsdl", "http://192.168.1.8/device");
        assert!(rejected);
        assert_eq!(
            selected,
            vec![
                "https://192.168.1.8/media2-secure".to_owned(),
                "http://192.168.1.8/media2".to_owned(),
            ]
        );
    }

    #[test]
    fn ranking_prefers_h264_then_resolution_then_token() {
        let mut profiles = [
            MediaProfile {
                token: "hevc".into(),
                name: None,
                video_codec: Some("H265".into()),
                width: Some(3840),
                height: Some(2160),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
            MediaProfile {
                token: "b".into(),
                name: None,
                video_codec: Some("H264".into()),
                width: Some(1280),
                height: Some(720),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
            MediaProfile {
                token: "a".into(),
                name: None,
                video_codec: Some("H264".into()),
                width: Some(1920),
                height: Some(1080),
                framerate: None,
                bitrate_kbps: None,
                audio_codec: None,
                service_kind: MediaServiceKind::Media2,
            },
        ];
        profiles.sort_by(|left, right| {
            right
                .is_h264_compatible()
                .cmp(&left.is_h264_compatible())
                .then_with(|| profile_pixels(right).cmp(&profile_pixels(left)))
                .then_with(|| left.token.cmp(&right.token))
        });
        assert_eq!(profiles[0].token, "a");
        assert_eq!(profiles[1].token, "b");
    }

    #[test]
    fn wsse_envelope_never_contains_plaintext_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        let body = soap_envelope(&credentials, "<tds:GetDeviceInformation/>");
        assert!(!body.contains("SENTINEL-password"));
        assert!(body.contains("PasswordDigest"));
    }

    #[test]
    fn digest_authorization_does_not_echo_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        let challenge = parse_digest_challenge(
            "Digest realm=\"cam\", nonce=\"abc\", qop=\"auth\", algorithm=MD5",
        )
        .unwrap();
        let header = digest_authorization(
            &credentials,
            &Url::parse("http://127.0.0.1/onvif").unwrap(),
            &challenge,
        )
        .unwrap();
        assert!(header.starts_with("Digest "));
        assert!(!header.contains("SENTINEL-password"));
    }

    #[test]
    fn digest_parser_finds_digest_when_basic_is_advertised_first() {
        let challenge = parse_digest_challenge_value(
            "Basic realm=\"camera\", Digest realm=\"camera\", nonce=\"abc\", qop=\"auth\", algorithm=SHA-256",
        )
        .unwrap();
        assert_eq!(challenge.realm, "camera");
        assert_eq!(challenge.nonce, "abc");
        assert_eq!(challenge.algorithm, "SHA-256");
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-password".into(),
        };
        assert!(!format!("{credentials:?}").contains("SENTINEL-password"));
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 2048];
        let mut header_end = None;
        let mut content_length = 0usize;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if header_end.is_none()
                && let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let end = position + 4;
                let headers = String::from_utf8_lossy(&bytes[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                header_end = Some(end);
            }
            if let Some(end) = header_end
                && bytes.len() >= end + content_length
            {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn write_http_response(
        stream: &mut std::net::TcpStream,
        status: &str,
        headers: &[(&str, String)],
        body: &str,
    ) {
        use std::io::Write as _;
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        response.push_str(body);
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn local_fixture_interrogates_media2_and_sanitizes_stream_userinfo() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let body = if request.contains("GetDeviceInformation") {
                    "<Envelope><Body><GetDeviceInformationResponse><Manufacturer>Fixture Corp</Manufacturer><Model>Fixture Cam</Model><FirmwareVersion>1.0</FirmwareVersion><SerialNumber>abc</SerialNumber><HardwareId>hw</HardwareId></GetDeviceInformationResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetHostname") {
                    "<Envelope><Body><GetHostnameResponse><HostnameInformation><Name>front-fixture</Name></HostnameInformation></GetHostnameResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetServices") {
                    format!(
                        "<Envelope><Body><GetServicesResponse><Service><Namespace>{MEDIA2_NS}</Namespace><XAddr>http://{address}/media2</XAddr></Service></GetServicesResponse></Body></Envelope>"
                    )
                } else if request.contains("GetProfiles") {
                    "<Envelope><Body><GetProfilesResponse><Profiles token=\"main\"><Name>Main</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1920</Width><Height>1080</Height></Resolution><RateControl><FrameRateLimit>25</FrameRateLimit><BitrateLimit>4096</BitrateLimit></RateControl></VideoEncoderConfiguration><AudioEncoderConfiguration><Encoding>AAC</Encoding></AudioEncoderConfiguration></Profiles><Profiles token=\"hevc\"><Name>HEVC</Name><VideoEncoderConfiguration><Encoding>H265</Encoding><Resolution><Width>3840</Width><Height>2160</Height></Resolution></VideoEncoderConfiguration></Profiles></GetProfilesResponse></Body></Envelope>".to_owned()
                } else if request.contains("GetStreamUri") {
                    "<Envelope><Body><GetStreamUriResponse><Uri>rtsp://admin:SENTINEL-fixture-password@127.0.0.1:8554/live/main</Uri></GetStreamUriResponse></Body></Envelope>".to_owned()
                } else {
                    panic!("unexpected fixture request")
                };
                write_http_response(&mut stream, "200 OK", &[], &body);
                requests.push(request);
            }
            requests
        });

        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-fixture-password".into(),
        };
        let client = OnvifClient::new().unwrap();
        let device_service = format!("http://{address}/onvif/device_service");
        let interrogation = client.interrogate(&device_service, &credentials).unwrap();
        assert_eq!(interrogation.device.model.as_deref(), Some("Fixture Cam"));
        assert_eq!(
            interrogation.device.hostname.as_deref(),
            Some("front-fixture")
        );
        assert_eq!(interrogation.media_service_kind, MediaServiceKind::Media2);
        assert_eq!(interrogation.profiles.len(), 2);
        assert!(interrogation.profiles[0].is_h264_compatible());
        let endpoint = client
            .stream_endpoint(
                &device_service,
                &interrogation.media_service,
                &credentials,
                &interrogation.profiles[0],
            )
            .unwrap();
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8554);
        assert_eq!(endpoint.path, "/live/main");
        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("SENTINEL-fixture-password"))
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("PasswordDigest"))
        );
    }

    #[test]
    fn stream_uri_query_is_rejected_before_any_endpoint_can_cross_the_boundary() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Uri>rtsp://127.0.0.1/live?opaque=SENTINEL-query-secret</Uri></Envelope>",
            );
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let profile = MediaProfile {
            token: "main".into(),
            name: None,
            video_codec: Some("H264".into()),
            width: None,
            height: None,
            framerate: None,
            bitrate_kbps: None,
            audio_codec: None,
            service_kind: MediaServiceKind::Media2,
        };
        let result = client.stream_endpoint(
            &format!("http://{address}/device"),
            &format!("http://{address}/media2"),
            &credentials,
            &profile,
        );
        assert_eq!(result, Err(OnvifError::InvalidStreamUri));
        assert!(!format!("{result:?}").contains("SENTINEL-query-secret"));
        server.join().unwrap();
    }

    #[test]
    fn media2_protocol_failure_falls_back_to_legacy_media_profiles() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                if request.contains("GetDeviceInformation") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Manufacturer>Fixture</Manufacturer><Model>Cam</Model></Envelope>",
                    );
                } else if request.contains("GetHostname") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Name>fixture</Name></Envelope>",
                    );
                } else if request.contains("GetServices") {
                    let body = format!(
                        "<Envelope><Service><Namespace>{MEDIA2_NS}</Namespace><XAddr>http://{address}/media2</XAddr></Service><Service><Namespace>{MEDIA1_NS}</Namespace><XAddr>http://{address}/media1</XAddr></Service></Envelope>"
                    );
                    write_http_response(&mut stream, "200 OK", &[], &body);
                } else if request.contains("POST /media2") {
                    write_http_response(&mut stream, "500 Internal Server Error", &[], "");
                } else if request.contains("POST /media1") {
                    write_http_response(
                        &mut stream,
                        "200 OK",
                        &[],
                        "<Envelope><Profiles token=\"legacy-main\"><Name>Legacy Main</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1280</Width><Height>720</Height></Resolution></VideoEncoderConfiguration></Profiles></Envelope>",
                    );
                } else {
                    panic!("unexpected fallback fixture request: {request}");
                }
            }
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let interrogation = client
            .interrogate(&format!("http://{address}/device"), &credentials)
            .unwrap();
        assert_eq!(
            interrogation.media_service_kind,
            MediaServiceKind::LegacyMedia
        );
        assert_eq!(interrogation.profiles[0].token, "legacy-main");
        server.join().unwrap();
    }

    #[test]
    fn onvif_http_client_with_explicit_proxy_bypass_constructs() {
        build_http_client(Duration::from_millis(50)).unwrap();
    }

    #[test]
    fn first_media2_unreachable_then_second_media2_succeeds() {
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_address = unavailable.local_addr().unwrap();
        drop(unavailable);
        let good = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let good_address = good.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = good.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Profiles token=\"main\"><VideoEncoderConfiguration><Encoding>H264</Encoding></VideoEncoderConfiguration></Profiles></Envelope>",
            );
        });
        let client = OnvifClient::with_timeout(Duration::from_millis(100)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let media2 = vec![
            format!("http://{unavailable_address}/media2-a"),
            format!("http://{good_address}/media2-b"),
        ];
        let (selected, kind, profiles) = client
            .select_media_profiles(&media2, &[], &credentials)
            .unwrap();
        assert_eq!(selected, media2[1]);
        assert_eq!(kind, MediaServiceKind::Media2);
        assert!(profiles.iter().any(MediaProfile::is_h264_compatible));
        server.join().unwrap();
    }

    #[test]
    fn media2_timeout_then_media1_succeeds() {
        let slow = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let slow_address = slow.local_addr().unwrap();
        let good = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let good_address = good.local_addr().unwrap();
        let slow_server = std::thread::spawn(move || {
            let (mut stream, _) = slow.accept().unwrap();
            let _ = read_http_request(&mut stream);
            std::thread::sleep(Duration::from_millis(120));
        });
        let good_server = std::thread::spawn(move || {
            let (mut stream, _) = good.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Profiles token=\"legacy\"><VideoEncoderConfiguration><Encoding>H264</Encoding></VideoEncoderConfiguration></Profiles></Envelope>",
            );
        });
        let client = OnvifClient::with_timeout(Duration::from_millis(30)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let media2 = vec![format!("http://{slow_address}/media2")];
        let media1 = vec![format!("http://{good_address}/media1")];
        let (selected, kind, _) = client
            .select_media_profiles(&media2, &media1, &credentials)
            .unwrap();
        assert_eq!(selected, media1[0]);
        assert_eq!(kind, MediaServiceKind::LegacyMedia);
        slow_server.join().unwrap();
        good_server.join().unwrap();
    }

    #[test]
    fn auth_failed_stops_before_remaining_media_authorities() {
        let auth = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let auth_address = auth.local_addr().unwrap();
        let untouched = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        untouched.set_nonblocking(true).unwrap();
        let untouched_address = untouched.local_addr().unwrap();
        let auth_server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = auth.accept().unwrap();
                let _ = read_http_request(&mut stream);
                write_http_response(&mut stream, "401 Unauthorized", &[], "");
            }
        });
        let client = OnvifClient::with_timeout(Duration::from_millis(100)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let media2 = vec![
            format!("http://{auth_address}/media2-a"),
            format!("http://{untouched_address}/media2-b"),
        ];
        assert_eq!(
            client.select_media_profiles(&media2, &[], &credentials),
            Err(OnvifError::AuthFailed)
        );
        auth_server.join().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            matches!(untouched.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn unsafe_alternate_service_is_filtered_and_never_contacted() {
        let safe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let safe_address = safe.local_addr().unwrap();
        let unsafe_listener = std::net::TcpListener::bind("127.0.0.2:0").unwrap();
        unsafe_listener.set_nonblocking(true).unwrap();
        let unsafe_address = unsafe_listener.local_addr().unwrap();
        let services = vec![
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: format!("http://{unsafe_address}/media2-a"),
            },
            ServiceEndpoint {
                namespace: MEDIA2_NS.into(),
                xaddr: format!("http://{safe_address}/media2-b"),
            },
        ];
        let (candidates, rejected) = validated_service_xaddrs(
            &services,
            "/ver20/media/wsdl",
            &format!("http://127.0.0.1:{}/device", safe_address.port()),
        );
        assert!(rejected);
        assert_eq!(candidates, vec![format!("http://{safe_address}/media2-b")]);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = safe.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Profiles token=\"main\"><VideoEncoderConfiguration><Encoding>H264</Encoding></VideoEncoderConfiguration></Profiles></Envelope>",
            );
        });
        let client = OnvifClient::with_timeout(Duration::from_millis(100)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        client
            .select_media_profiles(&candidates, &[], &credentials)
            .unwrap();
        server.join().unwrap();
        assert!(
            matches!(unsafe_listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn all_validated_candidates_without_h264_return_no_compatible_profile() {
        let media2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let media2_address = media2.local_addr().unwrap();
        let media1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let media1_address = media1.local_addr().unwrap();
        let server2 = std::thread::spawn(move || {
            let (mut stream, _) = media2.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Profiles token=\"hevc2\"><VideoEncoderConfiguration><Encoding>H265</Encoding></VideoEncoderConfiguration></Profiles></Envelope>",
            );
        });
        let server1 = std::thread::spawn(move || {
            let (mut stream, _) = media1.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "200 OK",
                &[],
                "<Envelope><Profiles token=\"hevc1\"><VideoEncoderConfiguration><Encoding>H265</Encoding></VideoEncoderConfiguration></Profiles></Envelope>",
            );
        });
        let client = OnvifClient::with_timeout(Duration::from_millis(100)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        assert_eq!(
            client.select_media_profiles(
                &[format!("http://{media2_address}/media2")],
                &[format!("http://{media1_address}/media1")],
                &credentials,
            ),
            Err(OnvifError::NoCompatibleProfile)
        );
        server2.join().unwrap();
        server1.join().unwrap();
    }

    #[test]
    fn digest_challenge_retries_once_without_plaintext_password() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_http_request(&mut first_stream);
            write_http_response(
                &mut first_stream,
                "401 Unauthorized",
                &[(
                    WWW_AUTHENTICATE.as_str(),
                    "Digest realm=\"fixture\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5"
                        .to_owned(),
                )],
                "",
            );
            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_http_request(&mut second_stream);
            write_http_response(
                &mut second_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );
            (first, second)
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-digest-password".into(),
        };
        let response = client
            .soap(
                &format!("http://{address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();
        assert!(!response.is_empty());
        let (first, second) = server.join().unwrap();
        assert!(!first.contains("SENTINEL-digest-password"));
        assert!(!second.contains("SENTINEL-digest-password"));
        assert!(!first.contains("PasswordDigest"));
        assert!(!second.contains("PasswordDigest"));
        assert!(!first.to_ascii_lowercase().contains("authorization: digest"));
        assert!(
            second
                .to_ascii_lowercase()
                .contains("authorization: digest")
        );
    }

    #[test]
    fn username_token_is_legacy_fallback_and_cached_without_storing_the_secret() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_http_request(&mut first_stream);
            write_http_response(&mut first_stream, "401 Unauthorized", &[], "");

            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_http_request(&mut second_stream);
            write_http_response(
                &mut second_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );

            let (mut third_stream, _) = listener.accept().unwrap();
            let third = read_http_request(&mut third_stream);
            write_http_response(
                &mut third_stream,
                "200 OK",
                &[],
                "<Envelope><Model>Fixture</Model></Envelope>",
            );
            (first, second, third)
        });

        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-username-token-password".into(),
        };
        let url = format!("http://{address}/device");
        client
            .soap(
                &url,
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();
        client
            .soap(
                &url,
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            )
            .unwrap();

        let (first, second, third) = server.join().unwrap();
        assert!(!first.contains("PasswordDigest"));
        assert!(second.contains("PasswordDigest"));
        assert!(third.contains("PasswordDigest"));
        for request in [&first, &second, &third] {
            assert!(!request.contains("SENTINEL-username-token-password"));
        }
    }

    #[test]
    fn bare_unauthorized_response_maps_to_auth_failed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_http_request(&mut stream);
                write_http_response(&mut stream, "401 Unauthorized", &[], "");
            }
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        assert_eq!(
            client.soap(
                &format!("http://{address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            ),
            Err(OnvifError::AuthFailed)
        );
        server.join().unwrap();
    }

    #[test]
    fn ptz_control_discovers_service_profile_association_and_zoom_capability() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let body = if request.contains("GetServices") {
                    format!(
                        "<Envelope><Service><Namespace>{MEDIA1_NS}</Namespace><XAddr>http://{address}/media</XAddr></Service><Service><Namespace>{PTZ_NS}</Namespace><XAddr>http://{address}/ptz</XAddr></Service></Envelope>"
                    )
                } else if request.contains("GetProfiles") {
                    "<Envelope><Profiles token=\"main\"><PTZConfiguration token=\"ptz-config\"/></Profiles></Envelope>".to_owned()
                } else if request.contains("GetConfigurationOptions") {
                    "<Envelope><Spaces><ContinuousPanTiltVelocitySpace><XRange><Min>-1</Min><Max>1</Max></XRange><YRange><Min>-1</Min><Max>1</Max></YRange></ContinuousPanTiltVelocitySpace><ContinuousZoomVelocitySpace><XRange><Min>-0.5</Min><Max>0.5</Max></XRange></ContinuousZoomVelocitySpace></Spaces></Envelope>".to_owned()
                } else {
                    panic!("unexpected PTZ fixture request: {request}")
                };
                write_http_response(&mut stream, "200 OK", &[], &body);
                requests.push(request);
            }
            requests
        });
        let client = OnvifClient::with_timeout(Duration::from_secs(1)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        let control = client
            .ptz_control(&format!("http://{address}/device"), &credentials)
            .unwrap();
        assert!(control.pan_tilt_supported());
        assert!(control.zoom_supported());
        assert_eq!(control.service, format!("http://{address}/ptz"));
        assert_eq!(control.profile_token, "main");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| !request.contains("secret")));
    }

    #[test]
    fn continuous_move_and_stop_are_bounded_and_axis_specific() {
        use crate::PtzVelocityRange;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                write_http_response(&mut stream, "200 OK", &[], "<Envelope/>");
                requests.push(request);
            }
            requests
        });
        let unit = PtzVelocityRange {
            min: -1.0,
            max: 1.0,
        };
        let control = PtzControl {
            service: format!("http://{address}/ptz"),
            profile_token: "main".to_owned(),
            pan: Some(unit),
            tilt: Some(unit),
            zoom: Some(unit),
        };
        let client = OnvifClient::with_timeout(Duration::from_secs(1)).unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        client
            .continuous_move(&control, &credentials, Some((-0.5, 0.25)), None)
            .unwrap();
        client.stop(&control, &credentials, true, false).unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].contains("ContinuousMove"));
        assert!(requests[0].contains("x=\"-0.500000\""));
        assert!(requests[0].contains("y=\"0.250000\""));
        assert!(requests[0].contains("PT1.000S"));
        assert!(requests[1].contains("<tptz:PanTilt>true</tptz:PanTilt>"));
        assert!(requests[1].contains("<tptz:Zoom>false</tptz:Zoom>"));
    }

    #[test]
    fn ptz_service_authority_mismatch_is_rejected_before_credentials_are_sent() {
        let services = vec![ServiceEndpoint {
            namespace: PTZ_NS.to_owned(),
            xaddr: "http://127.0.0.2/ptz".to_owned(),
        }];
        let (candidates, rejected) =
            validated_service_xaddrs(&services, "/ver20/ptz/wsdl", "http://127.0.0.1/device");
        assert!(candidates.is_empty());
        assert!(rejected);
    }

    #[test]
    fn local_event_fixture_runs_pullpoint_sync_pull_renew_and_unsubscribe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..7 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let body = if request.contains("GetServices") {
                    format!(
                        "<Envelope><Body><GetServicesResponse><Service><Namespace>{EVENT_NS}</Namespace><XAddr>http://{address}/events</XAddr></Service></GetServicesResponse></Body></Envelope>"
                    )
                } else if request.contains("GetEventProperties") {
                    r#"<Envelope xmlns:tns1="http://www.onvif.org/ver10/topics"><Body><GetEventPropertiesResponse><TopicSet><tns1:RuleEngine><tns1:CellMotionDetector><tns1:Motion/></tns1:CellMotionDetector></tns1:RuleEngine></TopicSet></GetEventPropertiesResponse></Body></Envelope>"#.to_owned()
                } else if request.contains("CreatePullPointSubscription") {
                    format!(
                        "<Envelope><Body><CreatePullPointSubscriptionResponse><SubscriptionReference><Address>http://{address}/pullpoint</Address></SubscriptionReference><CurrentTime>2026-09-05T03:00:00Z</CurrentTime><TerminationTime>2026-09-05T03:01:00Z</TerminationTime></CreatePullPointSubscriptionResponse></Body></Envelope>"
                    )
                } else if request.contains("SetSynchronizationPoint") {
                    "<Envelope><Body><SetSynchronizationPointResponse/></Body></Envelope>"
                        .to_owned()
                } else if request.contains("PullMessages") {
                    r#"<Envelope xmlns:tns1="http://www.onvif.org/ver10/topics"><Body><PullMessagesResponse><NotificationMessage><Topic>tns1:RuleEngine/CellMotionDetector/Motion</Topic><Message UtcTime="2026-09-05T03:00:01Z"><Source><SimpleItem Name="VideoSourceConfigurationToken" Value="RAW-SOURCE-TOKEN"/></Source><Data><SimpleItem Name="IsMotion" Value="true"/></Data></Message></NotificationMessage></PullMessagesResponse></Body></Envelope>"#.to_owned()
                } else if request.contains("<wsnt:Renew>") {
                    "<Envelope><Body><RenewResponse><CurrentTime>2026-09-05T03:00:20Z</CurrentTime><TerminationTime>2026-09-05T03:01:20Z</TerminationTime></RenewResponse></Body></Envelope>".to_owned()
                } else if request.contains("Unsubscribe") {
                    "<Envelope><Body><UnsubscribeResponse/></Body></Envelope>".to_owned()
                } else {
                    panic!("unexpected Event fixture request: {request}");
                };
                write_http_response(&mut stream, "200 OK", &[], &body);
                requests.push(request);
            }
            requests
        });

        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-event-password".into(),
        };
        let device_service = format!("http://{address}/onvif/device_service");
        let control = client.event_control(&device_service, &credentials).unwrap();
        assert!(control.properties().motion_supported);
        let mut subscription = client
            .create_pullpoint_subscription(&control, &credentials)
            .unwrap();
        client
            .set_synchronization_point(&subscription, &credentials)
            .unwrap();
        let notifications = client.pull_messages(&subscription, &credentials).unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].active);
        assert_eq!(
            notifications[0].source_key.as_ref().map(String::len),
            Some(64)
        );
        assert_ne!(
            notifications[0].source_key.as_deref(),
            Some("RAW-SOURCE-TOKEN")
        );
        client
            .renew_subscription(&mut subscription, &credentials)
            .unwrap();
        client.unsubscribe(&subscription, &credentials).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        assert!(requests.iter().any(|request| request.contains("PT4.000S")));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("MessageLimit>32"))
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("SENTINEL-event-password"))
        );
    }

    #[test]
    fn cross_host_event_service_is_rejected_before_followup_authenticated_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body = format!(
                "<Envelope><Body><GetServicesResponse><Service><Namespace>{EVENT_NS}</Namespace><XAddr>http://127.0.0.2:6553/events</XAddr></Service></GetServicesResponse></Body></Envelope>"
            );
            write_http_response(&mut stream, "200 OK", &[], &body);
            request
        });

        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "SENTINEL-cross-host-password".into(),
        };
        let device_service = format!("http://{address}/onvif/device_service");
        assert_eq!(
            client.event_control(&device_service, &credentials),
            Err(OnvifError::AuthorityRejected)
        );
        let request = server.join().unwrap();
        assert!(request.contains("GetServices"));
        assert!(!request.contains("SENTINEL-cross-host-password"));
    }

    #[test]
    fn redirects_are_not_followed_with_authenticated_requests() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_address = target.local_addr().unwrap();
        let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                "302 Found",
                &[("Location", format!("http://{target_address}/capture"))],
                "",
            );
        });
        let client = OnvifClient::new().unwrap();
        let credentials = OnvifCredentials {
            username: "admin".into(),
            password: "secret".into(),
        };
        assert_eq!(
            client.soap(
                &format!("http://{source_address}/device"),
                &credentials,
                "fixture-action",
                "<tds:GetDeviceInformation/>",
            ),
            Err(OnvifError::AuthorityRejected)
        );
        server.join().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            matches!(target.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
