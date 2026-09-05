//! Request id allocation, the initialize handshake, and the methods that matter.

use crate::protocol::{
    check, negotiate, notification, request, Error, KnownVersion, Result, CLIENT_NAME,
    CLIENT_VERSION,
};
use crate::transport::Transport;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Backstop for a server that mints a fresh cursor forever, which the
/// repeated-cursor check below cannot catch.
const MAX_PAGES: usize = 1000;

pub struct Session<T: Transport> {
    pub transport: T,
    next_id: u64,
    /// The `result` of `initialize`, once it has run.
    pub server_info: Value,
    /// The version agreed at `initialize`; until then, the one it will offer.
    version: KnownVersion,
}

impl<T: Transport> Session<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 0,
            server_info: Value::Null,
            version: KnownVersion::LATEST,
        }
    }

    /// Offer `version` at `initialize` instead of the newest one, for a server
    /// that misbehaves when offered something it has never heard of.
    pub fn offering(mut self, version: KnownVersion) -> Self {
        self.version = version;
        self
    }

    /// The protocol version this session runs on, for gating what is asked of the
    /// server: `session.version() >= KnownVersion::V2025_11_25`.
    pub fn version(&self) -> KnownVersion {
        self.version
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
    ///
    /// A server that answers with a version we do not speak is an error here, and
    /// the notification is withheld: the spec has the client disconnect instead.
    pub fn initialize(&mut self) -> Result<&Value> {
        let offered = self.version;
        let info = self.request(
            "initialize",
            Some(json!({
                "protocolVersion": offered.as_str(),
                "capabilities": {},
                "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
            })),
        )?;
        self.version = negotiate(offered, &info["protocolVersion"])?;
        self.server_info = info;
        self.transport.negotiated(self.version);
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

    pub fn list_resources(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("resources/list", "resources")
    }

    /// Templates are RFC 6570 URI templates rather than URIs, and arrive from their
    /// own method: expanding one is the caller's job before [`Self::read_resource`].
    pub fn list_resource_templates(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("resources/templates/list", "resourceTemplates")
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.request("resources/read", Some(json!({ "uri": uri })))
    }

    pub fn list_prompts(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("prompts/list", "prompts")
    }

    pub fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request(
            "prompts/get",
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

/// The bytes behind an `image` or `audio` block, an embedded `resource` block with
/// a `blob`, or a `resources/read` entry with one: decoded, with what the server
/// called them.
#[derive(Debug)]
pub struct Media {
    /// The block's `type`; a `resources/read` entry counts as a `resource`.
    pub kind: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Where a media block's bytes go. `Some(path)` means the sink put them there and
/// the path is what gets named; `None` leaves a placeholder in the block's place.
pub type MediaSink<'a> = dyn FnMut(&Media) -> Result<Option<PathBuf>> + 'a;

/// Flatten a `tools/call` result to text, one content block per line. A media
/// block becomes the line [`describe`] makes of it; any other non-text block is
/// emitted as its JSON.
///
/// A tool that declares an `outputSchema` may answer with `structuredContent` and no
/// content blocks at all (2025-06-18). Without the fallback such a result prints as
/// the empty string, indistinguishable from a tool that legitimately returned nothing.
pub fn render_content(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let blocks = match result.get("content").and_then(Value::as_array) {
        Some(blocks) => render_blocks(blocks, sink)?,
        None => String::new(),
    };
    Ok(match result.get("structuredContent") {
        Some(structured) if blocks.is_empty() => serde_json::to_string_pretty(structured).unwrap(),
        _ => blocks,
    })
}

/// Flatten a `prompts/get` result to one `role: text` line per message. Content
/// that is not text is treated as in [`render_content`].
pub fn render_messages(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let Some(messages) = result.get("messages").and_then(Value::as_array) else {
        return Ok(String::new());
    };
    let lines = messages
        .iter()
        .map(|m| {
            Ok(format!(
                "{}: {}",
                m["role"].as_str().unwrap_or("?"),
                render_blocks(std::slice::from_ref(&m["content"]), sink)?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(lines.join("\n"))
}

/// The `contents[]` of a `resources/read` result for a reader that will not take
/// bytes: text as it is, and each blob as the line [`describe`] makes of it.
pub fn render_resource(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let mut out = String::new();
    for entry in result["contents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if let Value::String(text) = &entry["text"] {
            out.push_str(text);
        } else if let Some(found) = media(entry)? {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&describe(&found, sink(&found)?.as_deref()));
            out.push('\n');
        }
    }
    Ok(out)
}

/// One entry of a `resources/read` result.
#[derive(Debug)]
pub enum ResourceBody {
    Text(String),
    Bytes(Vec<u8>),
}

/// The `contents[]` of a `resources/read` result, with every base64 `blob` decoded
/// to the bytes the server actually holds. An entry carrying neither `text` nor
/// `blob` has nothing to hand back and is dropped.
pub fn resource_bodies(result: &Value) -> Result<Vec<ResourceBody>> {
    result["contents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| match (&entry["text"], &entry["blob"]) {
            (Value::String(text), _) => Some(Ok(ResourceBody::Text(text.clone()))),
            (_, Value::String(blob)) => Some(
                decode_blob(blob, entry["uri"].as_str().unwrap_or("resource"))
                    .map(ResourceBody::Bytes),
            ),
            _ => None,
        })
        .collect()
}

fn decode_blob(blob: &str, what: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(blob)
        .map_err(|e| Error::transport(format!("{what}: blob is not valid base64 ({e})")))
}

/// Which object holds a block's base64, and under which key.
fn payload_holder(block: &Value) -> Option<(Option<&'static str>, &'static str)> {
    match block["type"].as_str() {
        Some("image") | Some("audio") => Some((None, "data")),
        Some("resource") => Some((Some("resource"), "blob")),
        // A `resources/read` entry has no type and keeps its blob at the top.
        None => Some((None, "blob")),
        _ => None,
    }
}

/// The media in a block, if it is one. A text block, a resource link, or a block
/// with no payload is `None`.
pub fn media(block: &Value) -> Result<Option<Media>> {
    let Some((inner, key)) = payload_holder(block) else {
        return Ok(None);
    };
    let holder = inner.map_or(block, |k| &block[k]);
    let Value::String(encoded) = &holder[key] else {
        return Ok(None);
    };
    let kind = block["type"].as_str().unwrap_or("resource");
    let bytes = decode_blob(encoded, holder["uri"].as_str().unwrap_or(kind))?;
    Ok(Some(Media {
        kind: kind.to_string(),
        mime_type: holder["mimeType"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string(),
        bytes,
    }))
}

/// Hand every media block of a `tools/call`, `prompts/get` or `resources/read`
/// result to `sink`, and where the bytes land in a file, put `path` and `bytes`
/// in the block in place of the base64. Everything else stays the server's.
pub fn save_media(result: &mut Value, sink: &mut MediaSink) -> Result<()> {
    for block in media_blocks_mut(result) {
        let Some(found) = media(block)? else {
            continue;
        };
        let Some(path) = sink(&found)? else {
            continue;
        };
        let (inner, key) = payload_holder(block).expect("a media block has a payload");
        let holder = match inner {
            Some(k) => &mut block[k],
            None => block,
        };
        if let Some(fields) = holder.as_object_mut() {
            fields.remove(key);
            fields.insert("path".into(), json!(path.to_string_lossy()));
            fields.insert("bytes".into(), json!(found.bytes.len()));
        }
    }
    Ok(())
}

/// Every block a result can carry media in: `content[]` of `tools/call`,
/// `messages[].content` of `prompts/get`, and `contents[]` of `resources/read`.
fn media_blocks_mut(result: &mut Value) -> Vec<&mut Value> {
    let Some(fields) = result.as_object_mut() else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for (key, value) in fields.iter_mut() {
        match (key.as_str(), value) {
            ("content" | "contents", Value::Array(items)) => blocks.extend(items.iter_mut()),
            ("messages", Value::Array(items)) => {
                blocks.extend(items.iter_mut().filter_map(|m| m.get_mut("content")))
            }
            _ => {}
        }
    }
    blocks
}

/// The one line that stands in for a media block on stdout.
pub fn describe(media: &Media, saved_to: Option<&Path>) -> String {
    let size = human_size(media.bytes.len());
    match saved_to {
        Some(path) => format!("[{} saved to {}, {size}]", media.kind, path.display()),
        None => format!("[{} {}, {size}]", media.kind, media.mime_type),
    }
}

fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / KB / KB)
    }
}

/// A file extension for a mime type: the usual one where the subtype is not it
/// already, the subtype where that is a plain word, and `bin` otherwise.
pub fn extension_for(mime_type: &str) -> &str {
    let essence = mime_type.split(';').next().unwrap_or("").trim();
    match essence {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" => "m4a",
        "text/plain" => "txt",
        "application/octet-stream" => "bin",
        _ => match essence.split_once('/') {
            Some((_, sub)) if !sub.is_empty() && sub.bytes().all(|c| c.is_ascii_alphanumeric()) => {
                sub
            }
            _ => "bin",
        },
    }
}

fn render_blocks(blocks: &[Value], sink: &mut MediaSink) -> Result<String> {
    let lines = blocks
        .iter()
        .map(|b| match (b["type"].as_str(), b.get("text")) {
            (Some("text"), Some(Value::String(t))) => Ok(t.clone()),
            _ => match media(b)? {
                Some(found) => Ok(describe(&found, sink(&found)?.as_deref())),
                None => Ok(b.to_string()),
            },
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(lines.join("\n"))
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
        told: Option<KnownVersion>,
    }

    impl Transport for Fake {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            self.sent.push(payload.clone());
            Ok(self.replies.pop_front().unwrap_or(None))
        }
        fn negotiated(&mut self, version: KnownVersion) {
            self.told = Some(version);
        }
    }

    fn fake(replies: Vec<Option<Value>>) -> Session<Fake> {
        Session::new(Fake {
            sent: Vec::new(),
            replies: replies.into(),
            told: None,
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
    fn initialize_offers_the_newest_version_and_keeps_what_the_server_answers() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}),
        )]);
        assert_eq!(
            s.version(),
            KnownVersion::V2025_11_25,
            "the offer, until then"
        );
        s.initialize().unwrap();
        assert_eq!(
            s.transport.sent[0]["params"]["protocolVersion"],
            "2025-11-25"
        );
        assert_eq!(s.version(), KnownVersion::V2025_06_18);
        assert_eq!(
            s.transport.told,
            Some(KnownVersion::V2025_06_18),
            "the transport hears the agreed version, not the offer"
        );
        assert!(
            s.version() < KnownVersion::V2025_11_25,
            "so gating reads naturally"
        );
    }

    #[test]
    fn a_pinned_version_is_the_one_offered() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}),
        )])
        .offering(KnownVersion::V2025_03_26);
        s.initialize().unwrap();
        assert_eq!(
            s.transport.sent[0]["params"]["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(s.version(), KnownVersion::V2025_03_26);
    }

    #[test]
    fn a_version_we_do_not_speak_ends_the_handshake() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1999-01-01"}}),
        )]);
        let e = s.initialize().unwrap_err();
        assert!(matches!(e, Error::Transport(_)), "{e:?}");
        let text = e.to_string();
        assert!(
            text.contains("1999-01-01") && text.contains("2025-11-25"),
            "{text}"
        );
        assert!(text.contains("--protocol-version"), "{text}");
        assert_eq!(
            s.transport.sent.len(),
            1,
            "no notifications/initialized: the spec says disconnect"
        );
        assert_eq!(s.transport.told, None);
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
        assert_eq!(render_content(&r, &mut placeholders).unwrap(), "hi");
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

    /// The sink of a reader with nowhere to put bytes.
    fn placeholders(_: &Media) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// A sink that records what it was handed and claims to have filed it.
    fn filing(
        seen: &mut Vec<(String, String, usize)>,
    ) -> impl FnMut(&Media) -> Result<Option<PathBuf>> + '_ {
        move |m| {
            seen.push((m.kind.clone(), m.mime_type.clone(), m.bytes.len()));
            Ok(Some(PathBuf::from(format!(
                "out/{}.{}",
                seen.len(),
                extension_for(&m.mime_type)
            ))))
        }
    }

    const PNG: &str = "iVBORw0KGgo=";

    #[test]
    fn render_content_stands_a_placeholder_in_for_an_image() {
        let r = json!({"content":[
            {"type":"text","text":"a"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
            {"type":"audio","data":PNG,"mimeType":"audio/wav"},
            {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","blob":PNG}},
        ]});
        let out = render_content(&r, &mut placeholders).unwrap();
        assert_eq!(
            out,
            "a\n[image image/png, 8 B]\n[audio audio/wav, 8 B]\n[resource application/pdf, 8 B]"
        );
        assert!(!out.contains(PNG), "no base64 on stdout");
        assert_eq!(render_content(&json!({}), &mut placeholders).unwrap(), "");
    }

    #[test]
    fn render_content_names_the_file_the_sink_wrote() {
        let r = json!({"content":[
            {"type":"text","text":"done"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
        ]});
        let mut seen = Vec::new();
        let out = render_content(&r, &mut filing(&mut seen)).unwrap();
        assert_eq!(out, "done\n[image saved to out/1.png, 8 B]");
        assert_eq!(seen, [("image".to_string(), "image/png".to_string(), 8)]);
    }

    #[test]
    fn render_content_still_emits_other_blocks_as_json() {
        let r = json!({"content":[
            {"type":"resource_link","uri":"file:///x","name":"x"},
            {"type":"resource","resource":{"uri":"file:///t.txt","text":"inline"}},
            {"type":"image","mimeType":"image/png"},
        ]});
        let out = render_content(&r, &mut placeholders).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"resource_link\""), "{out}");
        assert!(lines[1].contains("\"inline\""), "{out}");
        assert!(
            lines[2].contains("\"image\""),
            "a block with no data is not media"
        );

        let mangled =
            json!({"content":[{"type":"image","data":"not base64!","mimeType":"image/png"}]});
        let e = render_content(&mangled, &mut placeholders)
            .unwrap_err()
            .to_string();
        assert!(e.contains("image") && e.contains("base64"), "{e}");
    }

    #[test]
    fn render_content_prints_structured_content_when_no_block_renders() {
        let only = json!({"structuredContent":{"temp":20},"isError":false});
        assert_eq!(
            render_content(&only, &mut placeholders).unwrap(),
            "{\n  \"temp\": 20\n}"
        );

        let empty_array = json!({"content":[],"structuredContent":{"temp":20}});
        assert_eq!(
            render_content(&empty_array, &mut placeholders).unwrap(),
            "{\n  \"temp\": 20\n}"
        );

        assert_eq!(
            render_content(&json!({"content":[]}), &mut placeholders).unwrap(),
            ""
        );
    }

    #[test]
    fn render_content_leaves_a_result_with_blocks_untouched() {
        let both = json!({
            "content":[{"type":"text","text":"20 degrees"}],
            "structuredContent":{"temp":20},
        });
        assert_eq!(
            render_content(&both, &mut placeholders).unwrap(),
            "20 degrees"
        );
    }

    #[test]
    fn render_messages_prefixes_each_message_with_its_role() {
        let got = render_messages(
            &json!({"messages":[
                {"role":"user","content":{"type":"text","text":"summarize this"}},
                {"role":"assistant","content":{"type":"text","text":"sure"}},
            ]}),
            &mut placeholders,
        )
        .unwrap();
        assert_eq!(got, "user: summarize this\nassistant: sure");

        let image = render_messages(
            &json!({"messages":[
                {"role":"user","content":{"type":"image","data":PNG,"mimeType":"image/png"}},
            ]}),
            &mut placeholders,
        )
        .unwrap();
        assert_eq!(image, "user: [image image/png, 8 B]");

        assert_eq!(render_messages(&json!({}), &mut placeholders).unwrap(), "");
    }

    #[test]
    fn render_resource_keeps_text_and_files_blobs() {
        let read = json!({"contents":[
            {"uri":"file:///a.txt","text":"hello"},
            {"uri":"file:///b.png","mimeType":"image/png","blob":PNG},
            {"uri":"file:///c.empty"},
        ]});
        let mut seen = Vec::new();
        let out = render_resource(&read, &mut filing(&mut seen)).unwrap();
        assert_eq!(out, "hello\n[resource saved to out/1.png, 8 B]\n");
        assert_eq!(seen[0].0, "resource");
        assert_eq!(
            render_resource(&read, &mut placeholders).unwrap(),
            "hello\n[resource image/png, 8 B]\n"
        );
    }

    #[test]
    fn save_media_swaps_the_base64_for_the_path_and_size() {
        let mut call = json!({"content":[
            {"type":"text","text":"done"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
            {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","blob":PNG}},
        ],"isError":false});
        let mut seen = Vec::new();
        save_media(&mut call, &mut filing(&mut seen)).unwrap();
        assert_eq!(
            call,
            json!({"content":[
                {"type":"text","text":"done"},
                {"type":"image","path":"out/1.png","bytes":8,"mimeType":"image/png"},
                {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","path":"out/2.pdf","bytes":8}},
            ],"isError":false})
        );

        let mut prompt = json!({"messages":[
            {"role":"user","content":{"type":"image","data":PNG,"mimeType":"image/png"}},
            {"role":"assistant","content":{"type":"text","text":"sure"}},
        ]});
        save_media(&mut prompt, &mut filing(&mut Vec::new())).unwrap();
        assert_eq!(prompt["messages"][0]["content"]["path"], "out/1.png");
        assert!(prompt["messages"][0]["content"].get("data").is_none());
        assert_eq!(prompt["messages"][1]["content"]["text"], "sure");

        let mut read =
            json!({"contents":[{"uri":"file:///b.png","mimeType":"image/png","blob":PNG}]});
        save_media(&mut read, &mut filing(&mut Vec::new())).unwrap();
        assert_eq!(
            read,
            json!({"contents":[{"uri":"file:///b.png","mimeType":"image/png","path":"out/1.png","bytes":8}]})
        );

        let mut untouched = json!({"content":[{"type":"image","data":PNG,"mimeType":"image/png"}]});
        let before = untouched.clone();
        save_media(&mut untouched, &mut placeholders).unwrap();
        assert_eq!(
            untouched, before,
            "a sink that files nothing changes nothing"
        );
    }

    #[test]
    fn describe_sizes_like_a_person_would() {
        let media = |n: usize| Media {
            kind: "image".into(),
            mime_type: "image/png".into(),
            bytes: vec![0; n],
        };
        assert_eq!(describe(&media(4096), None), "[image image/png, 4 KB]");
        assert_eq!(describe(&media(1023), None), "[image image/png, 1023 B]");
        assert_eq!(describe(&media(1536), None), "[image image/png, 2 KB]");
        assert_eq!(
            describe(&media(3 * 1024 * 1024 / 2), None),
            "[image image/png, 1.5 MB]"
        );
        assert_eq!(
            describe(&media(4096), Some(Path::new("shots/take_screenshot-1.png"))),
            "[image saved to shots/take_screenshot-1.png, 4 KB]"
        );
    }

    #[test]
    fn extensions_follow_the_mime_type() {
        assert_eq!(extension_for("image/png"), "png");
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/svg+xml"), "svg");
        assert_eq!(extension_for("audio/mpeg"), "mp3");
        assert_eq!(extension_for("audio/x-wav"), "wav");
        assert_eq!(extension_for("application/pdf"), "pdf");
        assert_eq!(extension_for("text/plain; charset=utf-8"), "txt");
        assert_eq!(extension_for("text/html"), "html");
        assert_eq!(extension_for("application/octet-stream"), "bin");
        assert_eq!(extension_for("application/x-tar"), "bin");
        assert_eq!(extension_for(""), "bin");
        assert_eq!(extension_for("nonsense"), "bin");
    }

    #[test]
    fn resource_bodies_pass_text_through_and_decode_blobs() {
        let read = json!({"contents":[
            {"uri":"file:///a.txt","text":"hello\n"},
            {"uri":"file:///b.png","blob":"iVBORw0KGgo="},
            {"uri":"file:///c.empty"},
        ]});
        let bodies = resource_bodies(&read).unwrap();
        assert_eq!(bodies.len(), 2, "an entry with no payload is dropped");
        assert!(matches!(&bodies[0], ResourceBody::Text(t) if t == "hello\n"));
        assert!(matches!(&bodies[1], ResourceBody::Bytes(b) if b == b"\x89PNG\r\n\x1a\n"));

        let mangled = json!({"contents":[{"uri":"file:///b.png","blob":"not base64!"}]});
        let e = resource_bodies(&mangled).unwrap_err().to_string();
        assert!(e.contains("file:///b.png") && e.contains("base64"), "{e}");
    }
}
