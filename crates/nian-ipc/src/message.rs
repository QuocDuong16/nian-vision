//! Protocol envelopes and known methods.

use serde::{Deserialize, Serialize};

use crate::error::IpcError;

/// Current wire protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Correlation id of a request/response pair.
pub type RequestId = u64;

/// Method names understood by the media worker's control loop.
pub mod method {
    /// Liveness check; must answer promptly.
    pub const PING: &str = "ping";
    /// Reports worker identity and capability versions.
    pub const DESCRIBE: &str = "describe";
    /// Asks the worker to exit its serve loop cleanly.
    pub const SHUTDOWN: &str = "shutdown";
}

/// Event names emitted by the worker without a preceding request.
pub mod event {
    /// Emitted once after startup with worker metadata.
    pub const HELLO: &str = "hello";
}

/// One message on the wire.
///
/// Requests correlate to responses via [`RequestId`]; events have no id and
/// are fire-and-forget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    /// A call from host to worker.
    Request {
        /// Protocol version marker.
        v: u32,
        /// Correlation id echoed back in the response.
        id: RequestId,
        /// Method name, see [`method`].
        method: String,
        /// Free-form parameters for the method.
        params: serde_json::Value,
    },
    /// An answer from worker to host.
    Response {
        /// Protocol version marker.
        v: u32,
        /// Id of the originating request.
        id: RequestId,
        /// Whether the call succeeded.
        ok: bool,
        /// Success payload (`null` when absent).
        #[serde(default)]
        result: serde_json::Value,
        /// Machine-readable error code on failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
    },
    /// Unsolicited notification from worker to host.
    Event {
        /// Protocol version marker.
        v: u32,
        /// Event name, see [`event`].
        name: String,
        /// Event payload.
        data: serde_json::Value,
    },
}

impl Envelope {
    /// Builds a `ping`-style request with empty parameters.
    pub fn request(id: RequestId, method: impl Into<String>) -> Self {
        Self::Request {
            v: PROTOCOL_VERSION,
            id,
            method: method.into(),
            params: serde_json::Value::Null,
        }
    }

    /// Builds a successful response.
    pub fn success(id: RequestId, result: serde_json::Value) -> Self {
        Self::Response {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            result,
            error_code: None,
        }
    }

    /// Builds an error response.
    pub fn failure(id: RequestId, error_code: impl Into<String>) -> Self {
        Self::Response {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            result: serde_json::Value::Null,
            error_code: Some(error_code.into()),
        }
    }

    /// Builds an event notification.
    pub fn event(name: impl Into<String>, data: serde_json::Value) -> Self {
        Self::Event {
            v: PROTOCOL_VERSION,
            name: name.into(),
            data,
        }
    }

    /// Validates the protocol version marker before any other use.
    pub fn validate_version(&self) -> Result<(), IpcError> {
        let found = match self {
            Self::Request { v, .. } | Self::Response { v, .. } | Self::Event { v, .. } => *v,
        };
        if found == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(IpcError::UnsupportedProtocolVersion {
                found,
                expected: PROTOCOL_VERSION,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrips_all_envelope_kinds() {
        let messages = [
            Envelope::request(7, method::PING),
            Envelope::success(7, json!({"answer": 42})),
            Envelope::failure(7, "method_not_found"),
            Envelope::event(event::HELLO, json!({"pid": 123})),
        ];
        for original in messages {
            let text = serde_json::to_string(&original).unwrap();
            let decoded: Envelope = serde_json::from_str(&text).unwrap();
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn version_validation_accepts_current_and_rejects_other() {
        assert!(
            Envelope::request(1, method::PING)
                .validate_version()
                .is_ok()
        );

        let future = json!({
            "type": "request",
            "v": PROTOCOL_VERSION + 1,
            "id": 1,
            "method": "ping",
            "params": null
        });
        let decoded: Envelope = serde_json::from_value(future).unwrap();
        assert!(matches!(
            decoded.validate_version(),
            Err(IpcError::UnsupportedProtocolVersion { .. })
        ));
    }

    #[test]
    fn responses_omit_absent_error_code() {
        let text = serde_json::to_string(&Envelope::success(3, json!(null))).unwrap();
        assert!(!text.contains("error_code"), "unexpected field in {text}");
    }
}
