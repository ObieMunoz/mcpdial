//! Request id allocation, the initialize handshake, and the three methods that matter.

use crate::protocol::{check, notification, request, Result, CLIENT_NAME, CLIENT_VERSION, PROTOCOL_VERSION};
use crate::transport::Transport;
use serde_json::{json, Value};

pub struct Session<T: Transport> {
    pub transport: T,
    next_id: u64,
    /// The `result` of `initialize`, once it has run.
    pub server_info: Value,
}

impl<T: Transport> Session<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, next_id: 0, server_info: Value::Null }
    }

    /// Send a request and return its `result`. JSON-RPC errors become [`crate::Error::Rpc`].
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.next_id += 1;
        let msg = check(self.transport.send(&request(method, self.next_id, params))?)?;
        Ok(msg
            .and_then(|mut m| m.get_mut("result").map(Value::take))
            .unwrap_or_else(|| json!({})))
    }

    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        self.transport.send(&notification(method, params))?;
        Ok(())
    }

    /// The handshake. Stateless servers ignore `notifications/initialized`;
    /// stateful ones refuse everything that follows if it is missing.
    pub fn initialize(&mut self) -> Result<&Value> {
        self.server_info = self.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
            })),
        )?;
        self.notify("notifications/initialized", None)?;
        Ok(&self.server_info)
    }

    pub fn list_tools(&mut self) -> Result<Vec<Value>> {
        let mut res = self.request("tools/list", None)?;
        Ok(res
            .get_mut("tools")
            .and_then(|t| t.as_array_mut())
            .map(std::mem::take)
            .unwrap_or_default())
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", Some(json!({ "name": name, "arguments": arguments })))
    }

    pub fn close(&mut self) {
        self.transport.close();
    }
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        self.transport.close();
    }
}

/// Flatten a `tools/call` result to text, one content block per line.
/// Non-text blocks (images, resources) are emitted as their JSON.
pub fn render_content(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| match (b.get("type").and_then(Value::as_str), b.get("text")) {
                    (Some("text"), Some(Value::String(t))) => t.clone(),
                    _ => b.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Error;
    use std::collections::VecDeque;

    /// A transport that replays canned replies and records what was sent.
    struct Fake {
        sent: Vec<Value>,
        replies: VecDeque<Option<Value>>,
    }

    impl Transport for Fake {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            self.sent.push(payload.clone());
            Ok(self.replies.pop_front().unwrap_or(None))
        }
    }

    fn fake(replies: Vec<Option<Value>>) -> Session<Fake> {
        Session::new(Fake { sent: Vec::new(), replies: replies.into() })
    }

    #[test]
    fn initialize_sends_the_mandatory_notification() {
        let mut s = fake(vec![Some(json!({"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"x"}}}))]);
        let info = s.initialize().unwrap().clone();
        assert_eq!(info["serverInfo"]["name"], "x");
        let sent = &s.transport.sent;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["id"], 1);
        assert_eq!(sent[1]["method"], "notifications/initialized");
        assert!(sent[1].get("id").is_none(), "a notification must not carry an id");
    }

    #[test]
    fn ids_increase_and_results_are_unwrapped() {
        let mut s = fake(vec![
            Some(json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}]}})),
            Some(json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hi"}]}})),
        ]);
        assert_eq!(s.list_tools().unwrap()[0]["name"], "a");
        let r = s.call_tool("a", json!({})).unwrap();
        assert_eq!(render_content(&r), "hi");
        assert_eq!(s.transport.sent[1]["id"], 2);
        assert_eq!(s.transport.sent[1]["params"]["name"], "a");
    }

    #[test]
    fn rpc_errors_surface() {
        let mut s = fake(vec![Some(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}))]);
        assert!(matches!(s.request("x", None), Err(Error::Rpc { code: -32601, .. })));
    }

    #[test]
    fn render_content_falls_back_to_json_for_non_text() {
        let r = json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"xx","mimeType":"image/png"}]});
        let out = render_content(&r);
        assert!(out.starts_with("a\n{"));
        assert!(out.contains("\"type\":\"image\""));
        assert_eq!(render_content(&json!({})), "");
    }
}
