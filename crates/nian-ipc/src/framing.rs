//! NDJSON framing over any byte stream.

use std::io::{BufRead, Write};

use crate::error::IpcError;
use crate::message::Envelope;

/// Hard cap for one framed message, chosen well above any planned payload
/// (probe reports, metadata batches) while preventing a hostile peer from
/// forcing unbounded memory growth.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Reads [`Envelope`]s delimited by `\n`.
#[derive(Debug)]
pub struct FramedReader<R: BufRead> {
    inner: R,
    line: Vec<u8>,
}

impl<R: BufRead> FramedReader<R> {
    /// Wraps a buffered reader.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            line: Vec::with_capacity(4096),
        }
    }

    /// Blocks until the next complete message.
    ///
    /// Returns `Ok(None)` on clean EOF between messages. Blank lines are
    /// skipped; anything else that cannot be decoded is an error.
    pub fn next_message(&mut self) -> Result<Option<Envelope>, IpcError> {
        loop {
            match self.read_line()? {
                Line::Eof => return Ok(None),
                Line::Complete => {
                    if let Some(envelope) = self.decode_line()? {
                        return Ok(Some(envelope));
                    }
                    // Blank line: keep reading.
                }
            }
        }
    }

    fn read_line(&mut self) -> Result<Line, IpcError> {
        self.line.clear();
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                if self.line.is_empty() {
                    return Ok(Line::Eof);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "ipc message ended without newline",
                )
                .into());
            }

            match available.iter().position(|byte| *byte == b'\n') {
                Some(newline_at) => {
                    self.line.extend_from_slice(&available[..newline_at]);
                    self.inner.consume(newline_at + 1);
                    return Ok(Line::Complete);
                }
                None => {
                    self.line.extend_from_slice(available);
                    let consumed = available.len();
                    self.inner.consume(consumed);
                    if self.line.len() > MAX_MESSAGE_BYTES {
                        return Err(IpcError::MessageTooLarge {
                            limit: MAX_MESSAGE_BYTES,
                            actual: self.line.len(),
                        });
                    }
                }
            }
        }
    }

    fn decode_line(&self) -> Result<Option<Envelope>, IpcError> {
        let mut line = self.line.as_slice();
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return Ok(None);
        }
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(IpcError::MessageTooLarge {
                limit: MAX_MESSAGE_BYTES,
                actual: line.len(),
            });
        }
        let envelope: Envelope = serde_json::from_slice(line)?;
        envelope.validate_version()?;
        Ok(Some(envelope))
    }
}

enum Line {
    Eof,
    Complete,
}

/// Writes [`Envelope`]s as single newline-terminated JSON lines.
#[derive(Debug)]
pub struct FramedWriter<W: Write> {
    inner: W,
}

impl<W: Write> FramedWriter<W> {
    /// Wraps a writer.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Serializes, writes and flushes one message.
    ///
    /// Flushing per message keeps worker stdout latency predictable for the
    /// host's request/response correlation timeouts.
    pub fn send(&mut self, envelope: &Envelope) -> Result<(), IpcError> {
        envelope.validate_version()?;
        let mut line = serde_json::to_vec(envelope)?;
        line.push(b'\n');
        self.inner.write_all(&line)?;
        self.inner.flush()?;
        Ok(())
    }

    /// Unwraps the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{PROTOCOL_VERSION, RequestId, method};
    use std::io::Cursor;

    fn encode(messages: &[Envelope]) -> Cursor<Vec<u8>> {
        let mut bytes = Vec::new();
        for message in messages {
            let mut line = serde_json::to_vec(message).expect("test messages serialize");
            line.push(b'\n');
            bytes.extend_from_slice(&line);
        }
        Cursor::new(bytes)
    }

    #[test]
    fn roundtrips_messages_in_order() {
        let expected = vec![
            Envelope::request(1, method::PING),
            Envelope::success(1, serde_json::json!({"ok": true})),
            Envelope::event("hello", serde_json::json!({"pid": 5})),
        ];
        let mut reader = FramedReader::new(encode(&expected));
        for want in &expected {
            assert_eq!(&reader.next_message().unwrap().unwrap(), want);
        }
        assert!(reader.next_message().unwrap().is_none());
    }

    #[test]
    fn skips_blank_lines_and_tolerates_crlf() {
        let input = b"\r\n{\"type\":\"event\",\"v\":1,\"name\":\"hello\",\"data\":null}\r\n";
        let mut reader = FramedReader::new(Cursor::new(input.to_vec()));
        let Envelope::Event { name, .. } = reader.next_message().unwrap().unwrap() else {
            panic!("expected event");
        };
        assert_eq!(name, "hello");
        assert!(reader.next_message().unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_lines_without_unbounded_allocation() {
        let payload = "x".repeat(MAX_MESSAGE_BYTES + 1);
        let input = format!("{payload}\n");
        let mut reader = FramedReader::new(Cursor::new(input.into_bytes()));
        assert!(matches!(
            reader.next_message(),
            Err(IpcError::MessageTooLarge { .. })
        ));
    }

    #[test]
    fn errors_on_truncated_final_line() {
        let mut reader = FramedReader::new(Cursor::new(b"{\"type\":\"event\"".to_vec()));
        assert!(reader.next_message().is_err());
    }

    #[test]
    fn writer_flushes_each_message() {
        let mut writer = FramedWriter::new(Vec::<u8>::new());
        writer
            .send(&Envelope::Request {
                v: PROTOCOL_VERSION,
                id: 9,
                method: method::DESCRIBE.to_owned(),
                params: serde_json::Value::Null,
            })
            .unwrap();
        let buffer = writer.into_inner();
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.ends_with('\n'));
        let _id: RequestId = 9;
    }
}
