//! The two MCP transports. There is no third.

pub mod http;
pub mod retry;
pub mod stdio;
pub mod trace;

use crate::protocol::{KnownVersion, Responder, Result};
use serde_json::Value;
use std::time::Duration;
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

    /// Wait no longer than `within` for each exchange from here on, in place of
    /// the timeout this transport was built with, and hand back the wait that
    /// was in force so a caller can put it back. `None` clears it.
    ///
    /// What this exists for is a request sent inside a keystroke: Tab
    /// completion cannot spend the sixty seconds a tool call may. A transport
    /// that keeps no clock of its own ignores it, which is what the default
    /// does.
    fn wait_at_most(&mut self, within: Option<Duration>) -> Option<Duration> {
        let _ = within;
        None
    }

    /// Told once, after `initialize`, which version the session settled on. HTTP
    /// puts it on every request from then on; stdio has nowhere to put it.
    fn negotiated(&mut self, _version: KnownVersion) {}
    /// Install what serves the server requests this client would declare a
    /// capability for, and say whether it was taken.
    ///
    /// A transport with no way to deliver a reply drops the responder and goes
    /// on refusing, and answers `false` so that nothing is declared on its
    /// behalf: a capability with nobody behind it is a promise to a server that
    /// then blocks on an answer never coming.
    fn answer_requests(&mut self, _responder: Responder) -> bool {
        false
    }
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
    fn wait_at_most(&mut self, within: Option<Duration>) -> Option<Duration> {
        (**self).wait_at_most(within)
    }
    fn negotiated(&mut self, version: KnownVersion) {
        (**self).negotiated(version)
    }
    fn answer_requests(&mut self, responder: Responder) -> bool {
        (**self).answer_requests(responder)
    }
    fn close(&mut self) {
        (**self).close()
    }
}
