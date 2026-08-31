//! Host ⇄ media-worker IPC protocol.
//!
//! Transport: newline-delimited JSON (NDJSON) over the worker's stdin/stdout.
//! Rationale (master spec §4):
//!
//! * framing is trivial to implement correctly in any language;
//! * logs go to stderr, so stdout stays a clean protocol channel;
//! * secrets travel inside message payloads, never as command-line
//!   arguments, so they are invisible in process listings.
//!
//! Every envelope carries `v` (protocol version). The codec rejects unknown
//! versions loudly instead of guessing. The crate forbids `unsafe`.

#![forbid(unsafe_code)]
// Tests exercise failure paths directly; panicking asserts are idiomatic there.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod framing;
pub mod handshake;
pub mod message;
pub mod server;

pub use error::IpcError;
pub use framing::{FramedReader, FramedWriter, MAX_MESSAGE_BYTES};
pub use handshake::{APPLICATION_VERSION, WorkerHelloError, validate_worker_hello};
pub use message::{Envelope, PROTOCOL_VERSION, RequestId};
pub use server::{Dispatch, Handler, RpcFailure, serve};
