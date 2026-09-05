//! Bounded ONVIF discovery and provisioning protocol support.
//!
//! This crate owns only network/protocol concerns. It deliberately has no
//! dependency on Tauri, settings persistence, keyring storage, FFmpeg, or the
//! recording controller.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod authority;
mod client;
mod discovery;
mod error;
mod types;
mod xml;

pub use client::OnvifClient;
pub use discovery::{DiscoveryConfig, DiscoveryScanner};
pub use error::OnvifError;
pub use types::{
    DeviceInformation, DiscoveredDevice, EventControl, EventProperties, MediaProfile,
    MediaServiceKind, MotionNotification, OnvifCredentials, OnvifInterrogation, PtzControl,
    PtzVelocityRange, PullPointSubscription, StreamEndpoint,
};

pub const DEFAULT_DISCOVERY_TIMEOUT_MS: u64 = 3_000;
pub const MAX_DISCOVERED_DEVICES: usize = 64;
pub const MAX_DISCOVERY_DATAGRAM_BYTES: usize = 64 * 1024;
pub const MAX_XADDRS_PER_DEVICE: usize = 4;
pub const MAX_SCOPES_PER_DEVICE: usize = 16;
pub const MAX_SCOPE_BYTES: usize = 512;
pub const MAX_ENDPOINT_REFERENCE_BYTES: usize = 512;
pub const MAX_URL_BYTES: usize = 2_048;
pub const MAX_SOAP_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_XML_DEPTH: usize = 64;
pub const MAX_XML_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_PROFILES: usize = 128;
pub const MAX_PROFILE_TOKEN_BYTES: usize = 256;
pub const MAX_PROFILE_NAME_BYTES: usize = 512;
pub const MAX_PTZ_CONFIGURATION_TOKEN_BYTES: usize = 256;
pub const PTZ_MOVE_TIMEOUT_MS: u64 = 1_000;
pub const HTTP_TIMEOUT_MS: u64 = 5_000;
pub const EVENT_PULL_TIMEOUT_MS: u64 = 4_000;
pub const EVENT_PULL_MESSAGE_LIMIT: usize = 32;
pub const EVENT_INITIAL_SUBSCRIPTION_SECS: u64 = 60;
pub const MIN_EVENT_SUBSCRIPTION_LIFETIME_SECS: u64 = 5;
pub const MAX_EVENT_SUBSCRIPTION_LIFETIME_SECS: u64 = 24 * 60 * 60;
pub const MAX_EVENT_TOPIC_BYTES: usize = 512;
pub const MAX_EVENT_SIMPLE_ITEMS: usize = 64;
pub const MAX_EVENT_SIMPLE_ITEM_NAME_BYTES: usize = 256;
pub const MAX_EVENT_SIMPLE_ITEM_VALUE_BYTES: usize = 512;
pub const MAX_EVENT_TIMESTAMP_BYTES: usize = 64;
pub const MAX_EVENT_TOPIC_SET_NODES: usize = 512;
