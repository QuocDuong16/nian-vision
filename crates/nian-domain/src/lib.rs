//! Nian Vision domain model.
//!
//! This crate holds the vocabulary of the product: cameras, endpoints,
//! recordings, media stream descriptions and storage policy concepts.
//!
//! Rules enforced by construction:
//!
//! * no FFmpeg (or any other infrastructure) types leak into the domain;
//! * [`CameraId`] values are filesystem-safe so they can be used directly as
//!   recording directory names;
//! * credentials are wrapped in [`Secret`] and never rendered by `Debug` or
//!   `Display`, so a stray log line cannot leak a password;
//! * RTSP URL construction keeps credentials out of `Display`/`Debug` output;
//!   they are only present in the string returned by the explicit
//!   [`CameraEndpoint::url_with`].
//!
//! The crate forbids `unsafe`.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backoff;
pub mod camera;
pub mod error;
pub mod ids;
pub mod media;
pub mod ptz;
pub mod recording;
pub mod retention;
pub mod secret;

pub use backoff::ReconnectBackoff;
pub use camera::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraSource, CameraState, CredentialRef,
    Credentials, Host, MAX_CAMERA_PASSWORD_LEN, MAX_CAMERA_USERNAME_LEN, MAX_DISPLAY_NAME_LEN,
    MAX_RTSP_PATH_LEN,
};
pub use error::DomainError;
pub use ids::{CameraId, RecordingId};
pub use media::{MediaPacketMetadata, MediaProbeReport, MediaRational, MediaStreamInfo, MediaType};
pub use ptz::{
    MAX_ONVIF_DEVICE_PATH_LEN, MAX_ONVIF_ENDPOINT_REFERENCE_LEN, OnvifScheme, PtzBinding,
};
pub use recording::{FailureCategory, RecordingState};
pub use retention::{RetentionPolicy, StorageQuota};
pub use secret::Secret;
