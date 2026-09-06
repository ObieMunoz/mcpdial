//! The two MCP transports. There is no third.

pub mod http;
pub mod retry;
pub mod stdio;
pub mod trace;

use crate::protocol::{KnownVersion, Result};
use serde_json::Value;
pub(crate) use trace::silent;
pub use trace::{Logger, TraceEvent};

/// Something that can deliver one JSON-RPC message and hand back the reply.
///
/// `send` returns `Ok(None)` for notifications, which by definition get no answer.
pub trait Transport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>>;
    /// Told once, after `initialize`, which version the session settled on. HTTP
    /// puts it on every request from then on; stdio has nowhere to put it.
    fn negotiated(&mut self, _version: KnownVersion) {}
    fn close(&mut self) {}
}

impl Transport for Box<dyn Transport> {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        (**self).send(payload)
    }
    fn negotiated(&mut self, version: KnownVersion) {
        (**self).negotiated(version)
    }
    fn close(&mut self) {
        (**self).close()
    }
}
