//! Shared worker HELLO compatibility contract.
//!
//! Packaged builds must never silently pair a desktop executable with a worker
//! from a different application release. Debug builds deliberately tolerate
//! legacy test/development workers that predate the `application_version`
//! field, while still rejecting an explicit mismatch.

use serde_json::Value;
use thiserror::Error;

use crate::PROTOCOL_VERSION;

/// Product version compiled into every workspace crate from the authoritative
/// `[workspace.package]` version.
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkerHelloError {
    #[error("worker HELLO is missing protocol version")]
    MissingProtocol,
    #[error("worker IPC protocol {worker} is incompatible with desktop protocol {desktop}")]
    ProtocolMismatch { worker: u64, desktop: u32 },
    #[error("worker HELLO is missing application_version in a packaged build")]
    MissingApplicationVersion,
    #[error("worker application version {worker} is incompatible with desktop version {desktop}")]
    ApplicationVersionMismatch {
        worker: String,
        desktop: &'static str,
    },
}

/// Validates the metadata carried by the worker's HELLO event.
///
/// The IPC protocol version is always mandatory. `application_version` is
/// mandatory in non-debug builds. Debug builds may omit it so lightweight
/// protocol stubs remain useful, but an explicitly supplied version must still
/// match exactly.
pub fn validate_worker_hello(data: &Value) -> Result<(), WorkerHelloError> {
    let protocol = data
        .get("protocol")
        .and_then(Value::as_u64)
        .ok_or(WorkerHelloError::MissingProtocol)?;
    if protocol != u64::from(PROTOCOL_VERSION) {
        return Err(WorkerHelloError::ProtocolMismatch {
            worker: protocol,
            desktop: PROTOCOL_VERSION,
        });
    }

    match data.get("application_version").and_then(Value::as_str) {
        Some(version) if version == APPLICATION_VERSION => Ok(()),
        Some(version) => Err(WorkerHelloError::ApplicationVersionMismatch {
            worker: version.to_owned(),
            desktop: APPLICATION_VERSION,
        }),
        None if cfg!(debug_assertions) => Ok(()),
        None => Err(WorkerHelloError::MissingApplicationVersion),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_matching_application_and_protocol_versions() {
        validate_worker_hello(&json!({
            "protocol": PROTOCOL_VERSION,
            "application_version": APPLICATION_VERSION,
        }))
        .unwrap();
    }

    #[test]
    fn rejects_explicit_application_version_mismatch() {
        let error = validate_worker_hello(&json!({
            "protocol": PROTOCOL_VERSION,
            "application_version": "999.999.999",
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            WorkerHelloError::ApplicationVersionMismatch { .. }
        ));
    }

    #[test]
    fn rejects_protocol_mismatch_before_application_version() {
        let error = validate_worker_hello(&json!({
            "protocol": u64::from(PROTOCOL_VERSION) + 1,
            "application_version": APPLICATION_VERSION,
        }))
        .unwrap_err();
        assert!(matches!(error, WorkerHelloError::ProtocolMismatch { .. }));
    }
}
