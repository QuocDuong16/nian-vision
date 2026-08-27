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
///
/// The handler receives `&mut FramedWriter` so long-running methods can push
/// EVENT envelopes on the same protocol channel while (or before) their
/// reply is written — this is how `recording.start` streams supervisor
/// events without a second connection. Events must remain well-formed
/// envelopes; interleaving them with a response is safe because each frame
/// is one newline-terminated line read by an NDP-JSON parser on the host
/// side that demultiplexes responses and events by envelope type.
pub trait Handler<W> {
    /// Handles one request. Unknown methods should map to
    /// [`RpcFailure::new`] with code `"method_not_found"`.
    fn handle(
        &mut self,
        method_name: &str,
        params: &serde_json::Value,
        writer: &mut FramedWriter<W>,
    ) -> Dispatch
    where
        W: std::io::Write;
}

/// Reads requests until shutdown or EOF, writing exactly one response each.
///
/// Events are emitted only through the writer handed to
/// [`Handler::handle`]; nothing is pushed between requests because the loop
/// blocks on input — which matches the worker's job model (one recording
/// process does its reporting while its start request is in flight).
pub fn serve<R, W, H>(reader: R, writer: W, handler: &mut H) -> Result<(), IpcError>
where
    R: BufRead,
    W: std::io::Write,
    H: Handler<W>,
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
        match handler.handle(&name, &params, &mut writer) {
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

    struct TestHandler;

    impl<W: std::io::Write> Handler<W> for TestHandler {
        fn handle(
            &mut self,
            method_name: &str,
            _params: &Value,
            _writer: &mut FramedWriter<W>,
        ) -> Dispatch {
            match method_name {
                method::PING => Dispatch::Reply(Ok(json!("pong"))),
                method::SHUTDOWN => Dispatch::ShutdownReply(Ok(json!({"bye": true}))),
                _ => Dispatch::Reply(Err(RpcFailure::new("method_not_found"))),
            }
        }
    }

    /// Proves the trait contract that handlers MAY emit events through the
    /// provided writer before replying.
    struct EventfulHandler;

    impl<W: std::io::Write> Handler<W> for EventfulHandler {
        fn handle(
            &mut self,
            method_name: &str,
            params: &Value,
            writer: &mut FramedWriter<W>,
        ) -> Dispatch {
            if method_name == method::DESCRIBE {
                let _ = writer.send(&Envelope::event(
                    "recording.status",
                    json!({"state": "connecting"}),
                ));
                return Dispatch::Reply(Ok(json!({"described": true})));
            }
            TestHandler.handle(method_name, params, writer)
        }
    }

    fn request_line(id: u64, name: &str) -> String {
        format!(
            "{{\"type\":\"request\",\"v\":{PROTOCOL_VERSION},\"id\":{id},\"method\":\"{name}\",\"params\":null}}\n"
        )
    }

    #[test]
    fn answers_ping_then_shuts_down_on_request() {
        let input = format!(
            "{}{}",
            request_line(1, method::PING),
            request_line(2, method::SHUTDOWN)
        );
        let mut output = Vec::new();
        serve(
            Cursor::new(input.into_bytes()),
            &mut output,
            &mut TestHandler,
        )
        .unwrap();

        let responses = decode_all(output);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["ok"], true);
        assert_eq!(responses[0]["result"], "pong");
        assert_eq!(responses[1]["id"], 2);
    }

    #[test]
    fn handler_can_emit_events_through_the_writer_before_replying() {
        let input = request_line(5, method::DESCRIBE);
        let mut output = Vec::new();
        serve(
            Cursor::new(input.into_bytes()),
            &mut output,
            &mut EventfulHandler,
        )
        .unwrap();

        let messages = decode_all(output);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["type"], "event");
        assert_eq!(messages[0]["name"], "recording.status");
        assert_eq!(messages[1]["type"], "response");
        assert_eq!(messages[1]["id"], 5);
    }

    #[test]
    fn unknown_methods_fail_with_stable_code() {
        let mut output = Vec::new();
        serve(
            Cursor::new(request_line(3, "teleport").into_bytes()),
            &mut output,
            &mut TestHandler,
        )
        .unwrap();
        let responses = decode_all(output);
        assert_eq!(responses[0]["ok"], false);
        assert_eq!(responses[0]["error_code"], "method_not_found");
    }

    #[test]
    fn events_are_ignored_without_breaking_the_loop() {
        let input = format!(
            "{{\"type\":\"event\",\"v\":{PROTOCOL_VERSION},\"name\":\"hello\",\"data\":null}}\n{}",
            request_line(4, method::PING)
        );
        let mut output = Vec::new();
        serve(
            Cursor::new(input.into_bytes()),
            &mut output,
            &mut TestHandler,
        )
        .unwrap();
        let responses = decode_all(output);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 4);
    }

    #[test]
    fn eof_ends_the_loop_cleanly() {
        let mut output = Vec::new();
        serve(
            Cursor::new(String::new().into_bytes()),
            &mut output,
            &mut TestHandler,
        )
        .unwrap();
        assert!(decode_all(output).is_empty());
    }

    fn decode_all(bytes: Vec<u8>) -> Vec<Value> {
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
