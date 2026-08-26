//! Generic request dispatch loop used by the media worker.

use std::io::BufRead;

use tracing::{debug, warn};

use crate::error::IpcError;
use crate::framing::{FramedReader, FramedWriter};
use crate::message::{Envelope, RequestId};

/// Outcome produced by a [`Handler`] for one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Reply to the caller with this result.
    Reply(Result<serde_json::Value, RpcFailure>),
    /// Reply, then leave the serve loop cleanly.
    ShutdownReply(Result<serde_json::Value, RpcFailure>),
}

/// Machine-readable failure carried in an error response.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcFailure {
    /// Stable error code string, e.g. `method_not_found`.
    pub code: String,
}

impl RpcFailure {
    /// Builds a failure with the given code.
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}

/// Application logic behind the protocol methods.
pub trait Handler {
    /// Handles one request. Unknown methods should map to
    /// [`RpcFailure::new`] with code `"method_not_found"`.
    fn handle(&mut self, method_name: &str, params: &serde_json::Value) -> Dispatch;
}

/// Reads requests until shutdown or EOF, writing exactly one response each.
///
/// Events are not produced here; workers push them through their own
/// [`FramedWriter`] when needed.
pub fn serve<R, W, H>(reader: R, writer: W, handler: &mut H) -> Result<(), IpcError>
where
    R: BufRead,
    W: std::io::Write,
    H: Handler,
{
    let mut reader = FramedReader::new(reader);
    let mut writer = FramedWriter::new(writer);

    loop {
        let Some(envelope) = reader.next_message()? else {
            debug!("ipc peer closed the stream");
            return Ok(());
        };

        let Envelope::Request {
            id,
            method: name,
            params,
            ..
        } = envelope
        else {
            warn!(
                message_kind = "non_request",
                "ignoring unexpected non-request envelope"
            );
            continue;
        };

        debug!(%id, %name, "handling request");
        match handler.handle(&name, &params) {
            Dispatch::Reply(outcome) => {
                writer.send(&response(id, outcome))?;
            }
            Dispatch::ShutdownReply(outcome) => {
                writer.send(&response(id, outcome))?;
                debug!("shutdown requested; leaving serve loop");
                return Ok(());
            }
        }
    }
}

fn response(id: RequestId, outcome: Result<serde_json::Value, RpcFailure>) -> Envelope {
    match outcome {
        Ok(value) => Envelope::success(id, value),
        Err(failure) => Envelope::failure(id, failure.code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{PROTOCOL_VERSION, method};
    use serde_json::Value;
    use serde_json::json;
    use std::io::Cursor;

    struct TestHandler {
        served_requests: usize,
    }

    impl Handler for TestHandler {
        fn handle(&mut self, method_name: &str, _params: &Value) -> Dispatch {
            self.served_requests += 1;
            match method_name {
                method::PING => Dispatch::Reply(Ok(json!("pong"))),
                method::SHUTDOWN => Dispatch::ShutdownReply(Ok(json!({"bye": true}))),
                _ => Dispatch::Reply(Err(RpcFailure::new("method_not_found"))),
            }
        }
    }

    fn request_line(id: u64, name: &str) -> String {
        format!(
            "{{\"type\":\"request\",\"v\":{PROTOCOL_VERSION},\"id\":{id},\"method\":\"{name}\",\"params\":null}}\n"
        )
    }

    fn run(input: String) -> Vec<Value> {
        let mut handler = TestHandler { served_requests: 0 };
        let mut output = Vec::new();
        serve(Cursor::new(input.into_bytes()), &mut output, &mut handler).unwrap();

        let text = String::from_utf8(output).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn answers_ping_then_shuts_down_on_request() {
        let input = format!(
            "{}{}",
            request_line(1, method::PING),
            request_line(2, method::SHUTDOWN)
        );
        let responses = run(input);

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["ok"], true);
        assert_eq!(responses[0]["result"], "pong");
        assert_eq!(responses[1]["id"], 2);
    }

    #[test]
    fn unknown_methods_fail_with_stable_code() {
        let responses = run(request_line(3, "teleport"));
        assert_eq!(responses[0]["ok"], false);
        assert_eq!(responses[0]["error_code"], "method_not_found");
    }

    #[test]
    fn events_are_ignored_without_breaking_the_loop() {
        let input = format!(
            "{{\"type\":\"event\",\"v\":{PROTOCOL_VERSION},\"name\":\"hello\",\"data\":null}}\n{}",
            request_line(4, method::PING)
        );
        let responses = run(input);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 4);
    }

    #[test]
    fn eof_ends_the_loop_cleanly() {
        let responses = run(String::new());
        assert!(responses.is_empty());
    }
}
