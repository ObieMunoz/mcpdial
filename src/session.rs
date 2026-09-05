//! Request id allocation, the initialize handshake, and the three methods that matter.

use crate::protocol::{
    check, notification, request, Result, CLIENT_NAME, CLIENT_VERSION, PROTOCOL_VERSION,
};
use crate::transport::Transport;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Backstop for a server that mints a fresh cursor forever, which the
/// repeated-cursor check below cannot catch.
const MAX_PAGES: usize = 1000;

pub struct Session<T: Transport> {
    pub transport: T,
    next_id: u64,
    /// The `result` of `initialize`, once it has run.
    pub server_info: Value,
}

impl<T: Transport> Session<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 0,
            server_info: Value::Null,
        }
    }

    /// Send a request and return its `result`. JSON-RPC errors become [`crate::Error::Rpc`].
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.next_id += 1;
        let msg = check(
            self.transport
                .send(&request(method, self.next_id, params))?,
        )?;
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

    /// Every `*/list` method answers with items under `key` and an optional
    /// `nextCursor`, handed back as `{"cursor": "..."}`. Stopping at the first page
    /// loses the rest with no error: a tool that is really there reads as unknown.
    pub fn list_paginated(&mut self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut items: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let params = cursor.take().map(|c| json!({ "cursor": c }));
            let mut res = self.request(method, params)?;
            if let Some(page) = res.get_mut(key).and_then(Value::as_array_mut) {
                items.append(page);
            }
            let cursor_to_a_further_page = res
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty());
            match cursor_to_a_further_page {
                Some(c) if seen.insert(c.to_string()) => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(items)
    }

    pub fn list_tools(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("tools/list", "tools")
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
        )
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
///
/// A tool that declares an `outputSchema` may answer with `structuredContent` and no
/// content blocks at all (2025-06-18). Without the fallback such a result prints as
/// the empty string, indistinguishable from a tool that legitimately returned nothing.
pub fn render_content(result: &Value) -> String {
    let blocks = match result.get("content").and_then(Value::as_array) {
        Some(blocks) => render_blocks(blocks),
        None => String::new(),
    };
    match result.get("structuredContent") {
        Some(structured) if blocks.is_empty() => serde_json::to_string_pretty(structured).unwrap(),
        _ => blocks,
    }
}

fn render_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .map(
            |b| match (b.get("type").and_then(Value::as_str), b.get("text")) {
                (Some("text"), Some(Value::String(t))) => t.clone(),
                _ => b.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
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
        Session::new(Fake {
            sent: Vec::new(),
            replies: replies.into(),
        })
    }

    #[test]
    fn initialize_sends_the_mandatory_notification() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"x"}}}),
        )]);
        let info = s.initialize().unwrap().clone();
        assert_eq!(info["serverInfo"]["name"], "x");
        let sent = &s.transport.sent;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["id"], 1);
        assert_eq!(sent[1]["method"], "notifications/initialized");
        assert!(
            sent[1].get("id").is_none(),
            "a notification must not carry an id"
        );
    }

    #[test]
    fn ids_increase_and_results_are_unwrapped() {
        let mut s = fake(vec![
            Some(json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}]}})),
            Some(
                json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hi"}]}}),
            ),
        ]);
        assert_eq!(s.list_tools().unwrap()[0]["name"], "a");
        let r = s.call_tool("a", json!({})).unwrap();
        assert_eq!(render_content(&r), "hi");
        assert_eq!(s.transport.sent[1]["id"], 2);
        assert_eq!(s.transport.sent[1]["params"]["name"], "a");
    }

    #[test]
    fn a_paginated_list_is_read_to_the_end() {
        let mut s = fake(vec![
            Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":"c1"}}),
            ),
            Some(
                json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"b"}],"nextCursor":"c2"}}),
            ),
            Some(json!({"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"c"}]}})),
        ]);
        let names: Vec<String> = s
            .list_tools()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a", "b", "c"], "pages arrive in order");

        let sent = &s.transport.sent;
        assert_eq!(sent.len(), 3);
        assert!(
            sent[0].get("params").is_none(),
            "no cursor on the first page"
        );
        assert_eq!(sent[1]["params"]["cursor"], "c1");
        assert_eq!(sent[2]["params"]["cursor"], "c2");
    }

    #[test]
    fn the_helper_works_for_any_list_method() {
        let mut s = fake(vec![
            Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"prompts":[{"name":"a"}],"nextCursor":"c1"}}),
            ),
            Some(json!({"jsonrpc":"2.0","id":2,"result":{"prompts":[{"name":"b"}]}})),
        ]);
        let got = s.list_paginated("prompts/list", "prompts").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(s.transport.sent[1]["method"], "prompts/list");
    }

    #[test]
    fn an_empty_or_null_cursor_ends_the_list() {
        for last in [json!(""), Value::Null] {
            let mut s = fake(vec![Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":last}}),
            )]);
            assert_eq!(s.list_tools().unwrap().len(), 1);
            assert_eq!(s.transport.sent.len(), 1, "asked once and stopped");
        }
    }

    #[test]
    fn a_repeated_cursor_stops_the_loop() {
        let stuck =
            json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":"same"}});
        let mut s = fake(vec![Some(stuck); 10]);
        assert_eq!(s.list_tools().unwrap().len(), 2);
        assert_eq!(
            s.transport.sent.len(),
            2,
            "the second page repeats a cursor we have already followed"
        );
    }

    #[test]
    fn endless_fresh_cursors_stop_at_the_page_cap() {
        struct Endless(u64);
        impl Transport for Endless {
            fn send(&mut self, _payload: &Value) -> Result<Option<Value>> {
                self.0 += 1;
                Ok(Some(json!({"jsonrpc":"2.0","id":self.0,"result":{
                    "tools":[{"name":format!("t{}", self.0)}],
                    "nextCursor":format!("c{}", self.0)}})))
            }
        }
        let mut s = Session::new(Endless(0));
        assert_eq!(s.list_tools().unwrap().len(), MAX_PAGES);
    }

    #[test]
    fn rpc_errors_surface() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}),
        )]);
        assert!(matches!(
            s.request("x", None),
            Err(Error::Rpc { code: -32601, .. })
        ));
    }

    #[test]
    fn render_content_falls_back_to_json_for_non_text() {
        let r = json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"xx","mimeType":"image/png"}]});
        let out = render_content(&r);
        assert!(out.starts_with("a\n{"));
        assert!(out.contains("\"type\":\"image\""));
        assert_eq!(render_content(&json!({})), "");
    }

    #[test]
    fn render_content_prints_structured_content_when_no_block_renders() {
        let only = json!({"structuredContent":{"temp":20},"isError":false});
        assert_eq!(render_content(&only), "{\n  \"temp\": 20\n}");

        let empty_array = json!({"content":[],"structuredContent":{"temp":20}});
        assert_eq!(render_content(&empty_array), "{\n  \"temp\": 20\n}");

        assert_eq!(render_content(&json!({"content":[]})), "");
    }

    #[test]
    fn render_content_leaves_a_result_with_blocks_untouched() {
        let both = json!({
            "content":[{"type":"text","text":"20 degrees"}],
            "structuredContent":{"temp":20},
        });
        assert_eq!(render_content(&both), "20 degrees");
    }
}
