use url::Url;

use crate::{DeviceInformation, PtzVelocityRange};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceAdapter {
    Generic,
    TapoC200,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EventCompatibility {
    #[default]
    Standard,
    TapoC200,
}

impl DeviceAdapter {
    pub(crate) fn detect(device: &DeviceInformation) -> Self {
        let manufacturer = device
            .manufacturer
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let model = device
            .model
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        let tapo_vendor = manufacturer.contains("tp-link")
            || manufacturer.contains("tplink")
            || manufacturer.contains("tapo")
            || model.contains("tapo");
        let c200 = model
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|part| part.eq_ignore_ascii_case("c200"));

        if tapo_vendor && c200 {
            Self::TapoC200
        } else {
            Self::Generic
        }
    }

    pub(crate) fn event_compatibility(self) -> EventCompatibility {
        match self {
            Self::Generic => EventCompatibility::Standard,
            Self::TapoC200 => EventCompatibility::TapoC200,
        }
    }

    pub(crate) fn allows_unadvertised_motion_probe(self) -> bool {
        matches!(self, Self::TapoC200)
    }

    pub(crate) fn ptz_pan_tilt_fallback(self) -> Option<(PtzVelocityRange, PtzVelocityRange)> {
        if !matches!(self, Self::TapoC200) {
            return None;
        }
        let unit = PtzVelocityRange {
            min: -1.0,
            max: 1.0,
        };
        Some((unit, unit))
    }

    pub(crate) fn known_tapo_service_candidate(self, device_service: &str) -> Option<String> {
        if !matches!(self, Self::TapoC200) {
            return None;
        }

        let mut url = Url::parse(device_service).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || url.username() != ""
            || url.password().is_some()
        {
            return None;
        }
        url.set_port(Some(2020)).ok()?;
        url.set_path("/onvif/service");
        url.set_query(None);
        url.set_fragment(None);
        Some(url.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tapo_c200_detection_accepts_realistic_device_metadata() {
        for (manufacturer, model) in [
            ("TP-Link", "Tapo C200"),
            ("TP-LINK", "C200"),
            ("Tapo", "C200 V5"),
        ] {
            let device = DeviceInformation {
                manufacturer: Some(manufacturer.to_owned()),
                model: Some(model.to_owned()),
                hardware_id: Some("5.0".to_owned()),
                ..DeviceInformation::default()
            };
            assert_eq!(DeviceAdapter::detect(&device), DeviceAdapter::TapoC200);
        }
    }

    #[test]
    fn tapo_adapter_never_claims_an_unrelated_c200() {
        let device = DeviceInformation {
            manufacturer: Some("Other Vendor".to_owned()),
            model: Some("C200".to_owned()),
            ..DeviceInformation::default()
        };
        assert_eq!(DeviceAdapter::detect(&device), DeviceAdapter::Generic);
    }

    #[test]
    fn tapo_ptz_fallback_is_scoped_to_c200_pan_tilt_only() {
        let (pan, tilt) = DeviceAdapter::TapoC200.ptz_pan_tilt_fallback().unwrap();
        assert_eq!(
            pan,
            PtzVelocityRange {
                min: -1.0,
                max: 1.0
            }
        );
        assert_eq!(
            tilt,
            PtzVelocityRange {
                min: -1.0,
                max: 1.0
            }
        );
        assert_eq!(DeviceAdapter::Generic.ptz_pan_tilt_fallback(), None);
    }

    #[test]
    fn tapo_service_candidate_stays_on_same_host_and_fixed_onvif_port() {
        let candidate = DeviceAdapter::TapoC200
            .known_tapo_service_candidate("http://192.0.2.10:1234/onvif/device_service")
            .unwrap();
        assert_eq!(candidate, "http://192.0.2.10:2020/onvif/service");
    }
}
