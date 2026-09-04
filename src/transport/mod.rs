//! The two MCP transports. There is no third.

pub mod http;
pub mod stdio;

use crate::protocol::Result;
use serde_json::Value;

/// Something that can deliver one JSON-RPC message and hand back the reply.
///
/// `send` returns `Ok(None)` for notifications, which by definition get no answer.
pub trait Transport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>>;
    fn close(&mut self) {}
}

/// Where trace output goes when `--verbose` is on.
pub type Logger = Box<dyn FnMut(&str)>;

pub(crate) fn silent() -> Logger {
    Box::new(|_| {})
}

impl Transport for Box<dyn Transport> {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        (**self).send(payload)
    }
    fn close(&mut self) {
        (**self).close()
    }
}
