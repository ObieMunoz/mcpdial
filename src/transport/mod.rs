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

    /// [`Transport::send`], with every message the server sends on the way to
    /// its answer handed to `watch` as it arrives.
    ///
    /// "As it arrives" is the whole point: a progress notification that reaches
    /// the caller only once the call is over has reported nothing. A transport
    /// that cannot see those messages ignores `watch`, which is what the
    /// default does.
    fn send_watching(
        &mut self,
        payload: &Value,
        watch: &mut dyn FnMut(&Value),
    ) -> Result<Option<Value>> {
        let _ = watch;
        self.send(payload)
    }

    /// Told once, after `initialize`, which version the session settled on. HTTP
    /// puts it on every request from then on; stdio has nowhere to put it.
    fn negotiated(&mut self, _version: KnownVersion) {}
    fn close(&mut self) {}
}

impl Transport for Box<dyn Transport> {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        (**self).send(payload)
    }
    fn send_watching(
        &mut self,
        payload: &Value,
        watch: &mut dyn FnMut(&Value),
    ) -> Result<Option<Value>> {
        (**self).send_watching(payload, watch)
    }
    fn negotiated(&mut self, version: KnownVersion) {
        (**self).negotiated(version)
    }
    fn close(&mut self) {
        (**self).close()
    }
}
