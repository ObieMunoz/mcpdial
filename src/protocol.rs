//! JSON-RPC 2.0 framing as MCP uses it, and nothing else.
//!
//! Three facts are the whole protocol:
//!
//! 1. Every message is a JSON-RPC 2.0 object.
//! 2. Transport is HTTP POST to one endpoint, or newline-delimited JSON over stdio.
//! 3. The methods you actually need are `initialize`, `tools/list` and `tools/call`.

use serde_json::{json, Value};
use std::fmt;
use std::str::FromStr;

/// What `initialize` offers unless a version is pinned: the newest one we speak.
pub const PROTOCOL_VERSION: &str = KnownVersion::LATEST.as_str();
pub const CLIENT_NAME: &str = "mcpdial";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC's "method not found", the honest answer to a request we do not serve.
pub const METHOD_NOT_FOUND: i64 = -32601;

pub type Result<T> = std::result::Result<T, Error>;

/// The protocol revisions mcpdial speaks, newest first.
///
/// Ordered by date, so a feature gate reads `session.version() >= V2025_11_25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownVersion {
    V2025_11_25,
    V2025_06_18,
    V2025_03_26,
}

impl KnownVersion {
    pub const ALL: [KnownVersion; 3] = [
        KnownVersion::V2025_11_25,
        KnownVersion::V2025_06_18,
        KnownVersion::V2025_03_26,
    ];
    pub const LATEST: KnownVersion = KnownVersion::ALL[0];

    pub const fn as_str(self) -> &'static str {
        match self {
            KnownVersion::V2025_11_25 => "2025-11-25",
            KnownVersion::V2025_06_18 => "2025-06-18",
            KnownVersion::V2025_03_26 => "2025-03-26",
        }
    }

    pub fn parse(s: &str) -> Option<KnownVersion> {
        KnownVersion::ALL.into_iter().find(|v| v.as_str() == s)
    }

    fn listed() -> String {
        KnownVersion::ALL
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl PartialOrd for KnownVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KnownVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Display for KnownVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for KnownVersion {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        KnownVersion::parse(s).ok_or_else(|| {
            format!(
                "{s:?} is not a protocol version mcpdial speaks (one of {})",
                KnownVersion::listed()
            )
        })
    }
}

/// The version a session runs on, given what `initialize` offered and the
/// `protocolVersion` the server answered with.
///
/// A server that supports the offer echoes it; one that does not names another
/// version it supports, and the spec has a client that does not speak that one
/// disconnect. A server that names none is taken to have accepted the offer.
pub fn negotiate(offered: KnownVersion, answered: &Value) -> Result<KnownVersion> {
    let answered = match answered {
        Value::Null => return Ok(offered),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    KnownVersion::parse(&answered).ok_or_else(|| {
        Error::transport(format!(
            "the server answered initialize with protocol version {answered}, which mcpdial \
             does not speak (offered {offered}; known: {}).\n\
             Hint: offer one the server accepts with --protocol-version VERSION.",
            KnownVersion::listed()
        ))
    })
}

/// Everything that can go wrong, sorted by who is to blame.
#[derive(Debug)]
pub enum Error {
    /// The server answered with a JSON-RPC error object.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The server answered with a non-2xx HTTP status.
    Http {
        status: u16,
        body: String,
        www_authenticate: Option<String>,
    },
    /// The transport itself failed: socket, process, timeout, malformed frame.
    Transport(String),
    /// The OAuth dance failed somewhere between discovery and token exchange.
    Auth(String),
    /// The config or credential store could not be read or written.
    Config(String),
    /// The caller handed us something we cannot act on.
    Usage(String),
}

impl Error {
    pub fn transport(msg: impl Into<String>) -> Self {
        Error::Transport(msg.into())
    }
    pub fn auth(msg: impl Into<String>) -> Self {
        Error::Auth(msg.into())
    }
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }
    pub fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    /// True when the server asked for a credential (any status with a challenge).
    pub fn is_auth_challenge(&self) -> bool {
        matches!(
            self,
            Error::Http {
                www_authenticate: Some(_),
                ..
            } | Error::Http { status: 401, .. }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Rpc { code, message, .. } => write!(f, "MCP error {code}: {message}"),
            Error::Http {
                status,
                body,
                www_authenticate,
            } => {
                write!(f, "HTTP {status}")?;
                let body = body.trim();
                if !body.is_empty() {
                    let shown: String = body.chars().take(400).collect();
                    write!(f, "\n{shown}")?;
                }
                write!(f, "{}", http_hint(*status, www_authenticate.as_deref()))
            }
            Error::Transport(m) | Error::Auth(m) | Error::Config(m) | Error::Usage(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Transport(format!("malformed JSON from server: {e}"))
    }
}

/// Distinguish "nobody asked for a token" from "your token was rejected".
///
/// A 403 with **no** `WWW-Authenticate` header is a WAF, IP allowlist, or geo
/// block. Adding a credential cannot fix it, and telling the user to check their
/// token scope wastes real debugging time. A challenge header, on any status, means
/// the server wants a credential and (per RFC 9728) says where to get one.
pub fn http_hint(status: u16, www_authenticate: Option<&str>) -> String {
    match (status, www_authenticate) {
        (_, Some(www)) => format!(
            "\nHint: the server challenged for a credential. Run `mcpdial login <server>` \
             or check the token's scope against the metadata below.\nWWW-Authenticate: {www}"
        ),
        (401, None) => "\nHint: 401 without a challenge header. The server wants a credential \
                        but did not say how to get one; try `mcpdial login <server>`."
            .to_string(),
        (403, None) => "\nHint: 403 with no WWW-Authenticate is a policy/edge block, not an \
                        auth failure - a token will not help. Common causes: WAF bot \
                        fingerprinting, IP allowlist, or geo restriction."
            .to_string(),
        _ => String::new(),
    }
}

/// Return the JSON-RPC message carried in an HTTP response body.
///
/// Streamable HTTP lets a server answer a POST either as plain `application/json`
/// or as a one-shot `text/event-stream`. The SSE form wraps the payload in
/// `event:` / `data:` lines, so that framing is stripped before parsing. A bare
/// `| jq` fails on it, which is the main reason a naive curl attempt does not work.
///
/// Notifications are answered with an empty 202, which is why `None` is a valid
/// return value.
pub fn decode_body(body: &str, content_type: &str) -> Result<Option<Value>> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(None);
    }

    let looks_like_sse = content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
        || body.starts_with("event:")
        || body.starts_with("data:");

    if looks_like_sse {
        return decode_sse(body);
    }

    Ok(Some(serde_json::from_str(body)?))
}

/// Return the first response carried in an SSE body.
///
/// A server may put a log notification or a `ping` of its own on the response
/// stream ahead of the answer, so each event is parsed alone: joining the whole
/// body concatenates two JSON objects and blames the server for a reply that was
/// well formed.
fn decode_sse(body: &str) -> Result<Option<Value>> {
    let mut unparseable = None;

    for event in sse_events(body) {
        match serde_json::from_str::<Value>(&event) {
            Ok(msg) if matches!(classify(&msg, None), Incoming::Response) => return Ok(Some(msg)),
            Ok(_the_server_talking_to_us) => continue,
            Err(e) => unparseable = unparseable.or(Some(e)),
        }
    }

    match unparseable {
        Some(e) => Err(e.into()),
        None => Ok(None),
    }
}

/// Split an SSE body into the joined `data:` payload of each event.
fn sse_events(body: &str) -> Vec<String> {
    let end_of_the_last_event = std::iter::once("");
    let mut events = Vec::new();
    let mut data = String::new();

    for line in body.lines().chain(end_of_the_last_event) {
        let blank_line_ends_the_event = line.trim().is_empty();
        if blank_line_ends_the_event {
            if !data.is_empty() {
                events.push(std::mem::take(&mut data));
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim());
        }
    }

    events
}

/// What a message from the server is, to a client waiting for one particular id.
///
/// The pair that matters is `id` **and** `method`: that combination is a request
/// addressed to us, and dropping it leaves the server waiting for an answer that
/// never comes.
#[derive(Debug)]
pub enum Incoming<'a> {
    /// The answer we are waiting for.
    Response,
    /// The server is asking us something and may block until we reply to `id`.
    ServerRequest { id: &'a Value, method: &'a str },
    /// One-way traffic - progress, logging. Nothing is owed.
    Notification { method: &'a str },
    /// An answer to somebody else's id, or a frame we cannot place.
    Foreign,
}

/// Sort one incoming message, the rule [`StdioTransport`] applies to stdout.
///
/// `awaiting` is the id we are blocked on, or `None` to accept any response: an
/// HTTP body carries the answer to the POST it came back from, so there is no
/// other id it could be. `error` stands in for `id` in that case, because a server
/// that could not read the id we sent has nothing to echo back.
///
/// [`StdioTransport`]: crate::StdioTransport
pub fn classify<'a>(msg: &'a Value, awaiting: Option<&Value>) -> Incoming<'a> {
    // A null id is not an id: JSON-RPC uses it for a request the server could not
    // parse, and answering it would address a reply to nobody.
    let id = msg.get("id").filter(|id| !id.is_null());
    match (msg.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Incoming::ServerRequest { id, method },
        (Some(method), None) => Incoming::Notification { method },
        (None, id) => {
            let answers_us = match awaiting {
                Some(ours) => id == Some(ours),
                None => id.is_some() || msg.get("error").is_some(),
            };
            if answers_us {
                Incoming::Response
            } else {
                Incoming::Foreign
            }
        }
    }
}

/// Raise a JSON-RPC `error` member as [`Error::Rpc`], otherwise pass the message on.
pub fn check(msg: Option<Value>) -> Result<Option<Value>> {
    if let Some(err) = msg.as_ref().and_then(|m| m.get("error")) {
        return Err(Error::Rpc {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(-1),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
            data: err.get("data").cloned(),
        });
    }
    Ok(msg)
}

pub fn request(method: &str, id: u64, params: Option<Value>) -> Value {
    let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if let Some(p) = params {
        msg["params"] = p;
    }
    msg
}

pub fn notification(method: &str, params: Option<Value>) -> Value {
    let mut msg = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(p) = params {
        msg["params"] = p;
    }
    msg
}

/// The reply owed to a server request, given its id and method.
///
/// `ping` is the one request every MCP client must answer whatever it declared. A
/// server that pings during a long call and hears nothing back either drops the
/// connection or waits for a peer that is never coming, and the stall gets
/// reported as the server's fault. Everything else - `sampling/createMessage`,
/// `roots/list`, `elicitation/create` - we turned down by sending an empty
/// `capabilities` at initialize, so a refusal is both truthful and something the
/// server can act on at once; silence it can only time out.
pub fn answer(id: &Value, method: &str) -> Value {
    match method {
        "ping" => reply(id, json!({})),
        other => reply_error(
            id,
            METHOD_NOT_FOUND,
            &format!("{CLIENT_NAME} does not implement {other}"),
        ),
    }
}

pub fn reply(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn reply_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json_passes_through() {
        let v = decode_body(
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            "application/json",
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn sse_framing_is_stripped() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn sse_detected_from_body_when_content_type_is_missing() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}";
        let v = decode_body(body, "").unwrap().unwrap();
        assert_eq!(v["id"], 9);
    }

    #[test]
    fn multiline_data_is_joined() {
        let body = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":4,\"result\":{}}";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 4);
    }

    #[test]
    fn notification_before_the_result_is_skipped() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\"}}\n\n\
                    event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["content"][0]["text"], "ok");
    }

    #[test]
    fn notification_after_the_result_is_skipped() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn server_ping_is_not_mistaken_for_the_result() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-1\",\"method\":\"ping\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"ok\":true}}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 5);
    }

    #[test]
    fn an_error_response_is_a_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":6,\"error\":{\"code\":-32602,\"message\":\"nope\"}}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn comments_and_other_fields_are_ignored() {
        let body = ": keep-alive\n\nevent: message\nid: 42\nretry: 3000\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":8,\"result\":{}}\n\n: keep-alive\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 8);
    }

    #[test]
    fn multiline_data_survives_a_neighbouring_event() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":4,\"result\":{}}\n\n";
        let v = decode_body(body, "text/event-stream").unwrap().unwrap();
        assert_eq!(v["id"], 4);
    }

    #[test]
    fn a_body_of_only_notifications_is_none() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        assert!(decode_body(body, "text/event-stream").unwrap().is_none());
    }

    #[test]
    fn malformed_events_only_fail_when_nothing_else_answers() {
        let body =
            "data: <html>nope</html>\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert_eq!(
            decode_body(body, "text/event-stream").unwrap().unwrap()["id"],
            1
        );
        assert!(matches!(
            decode_body("data: <html>nope</html>\n\n", "text/event-stream"),
            Err(Error::Transport(_))
        ));
    }

    #[test]
    fn empty_body_is_none() {
        assert!(decode_body("", "application/json").unwrap().is_none());
        assert!(decode_body("   \n", "text/event-stream").unwrap().is_none());
        assert!(decode_body("event: message\n\n", "text/event-stream")
            .unwrap()
            .is_none());
    }

    #[test]
    fn garbage_is_a_transport_error() {
        assert!(matches!(
            decode_body("<html>nope</html>", "text/html"),
            Err(Error::Transport(_))
        ));
    }

    #[test]
    fn rpc_error_is_raised() {
        let msg =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Tool nope not found"}});
        match check(Some(msg)) {
            Err(Error::Rpc { code, message, .. }) => {
                assert_eq!(code, -32602);
                assert_eq!(message, "Tool nope not found");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[test]
    fn hints_key_off_the_challenge_header_not_the_status() {
        assert!(http_hint(403, None).contains("policy/edge block"));
        assert!(http_hint(403, Some("Bearer realm=x")).contains("challenged"));
        assert!(http_hint(401, Some("Bearer realm=x")).contains("challenged"));
        assert!(http_hint(500, None).is_empty());
    }

    #[test]
    fn notifications_carry_no_id() {
        let n = notification("notifications/initialized", None);
        assert!(n.get("id").is_none());
        let r = request("tools/list", 7, None);
        assert_eq!(r["id"], 7);
        assert!(r.get("params").is_none());
    }

    #[test]
    fn a_method_beside_an_id_is_a_request_to_us() {
        let ours = json!(2);
        let ping = json!({"jsonrpc":"2.0","id":"srv-1","method":"ping"});
        match classify(&ping, Some(&ours)) {
            Incoming::ServerRequest { id, method } => {
                assert_eq!(id, "srv-1");
                assert_eq!(method, "ping");
            }
            other => panic!("expected a server request, got {other:?}"),
        }
    }

    #[test]
    fn a_method_alone_is_a_notification() {
        let ours = json!(2);
        let note = json!({"jsonrpc":"2.0","method":"notifications/message"});
        assert!(matches!(
            classify(&note, Some(&ours)),
            Incoming::Notification {
                method: "notifications/message"
            }
        ));
        // A null id is the same thing: there is nobody to address a reply to.
        let null_id = json!({"jsonrpc":"2.0","id":null,"method":"notifications/progress"});
        assert!(matches!(
            classify(&null_id, Some(&ours)),
            Incoming::Notification { .. }
        ));
    }

    #[test]
    fn only_our_own_id_answers_us() {
        let ours = json!(2);
        let mine = json!({"jsonrpc":"2.0","id":2,"result":{}});
        let theirs = json!({"jsonrpc":"2.0","id":99,"result":{}});
        assert!(matches!(classify(&mine, Some(&ours)), Incoming::Response));
        assert!(matches!(classify(&theirs, Some(&ours)), Incoming::Foreign));
        // With no id to wait for, any response will do, error or not.
        assert!(matches!(classify(&theirs, None), Incoming::Response));
        let no_id = json!({"jsonrpc":"2.0","error":{"code":-32700,"message":"parse error"}});
        assert!(matches!(classify(&no_id, None), Incoming::Response));
        assert!(matches!(classify(&no_id, Some(&ours)), Incoming::Foreign));
    }

    #[test]
    fn ping_is_answered_and_the_rest_refused() {
        let id = json!("srv-1");
        let pong = answer(&id, "ping");
        assert_eq!(pong["id"], "srv-1");
        assert_eq!(pong["result"], json!({}));
        assert!(pong.get("error").is_none());

        let refusal = answer(&id, "sampling/createMessage");
        assert_eq!(refusal["error"]["code"], METHOD_NOT_FOUND);
        assert!(refusal["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sampling/createMessage"));
        assert!(refusal.get("result").is_none());
    }

    #[test]
    fn known_versions_order_by_date_and_the_newest_is_offered() {
        use KnownVersion::*;
        assert!(V2025_11_25 > V2025_06_18 && V2025_06_18 > V2025_03_26);
        assert_eq!(KnownVersion::LATEST, V2025_11_25);
        assert_eq!(PROTOCOL_VERSION, "2025-11-25");
        assert_eq!("2025-03-26".parse::<KnownVersion>(), Ok(V2025_03_26));
        let e = "2024-11-05".parse::<KnownVersion>().unwrap_err();
        assert!(e.contains("2024-11-05") && e.contains("2025-11-25"), "{e}");
        assert_eq!(V2025_06_18.to_string(), "2025-06-18");
    }

    #[test]
    fn a_known_answer_is_agreed_to_and_none_means_the_offer_stood() {
        use KnownVersion::*;
        assert_eq!(
            negotiate(V2025_11_25, &json!("2025-11-25")).unwrap(),
            V2025_11_25
        );
        assert_eq!(
            negotiate(V2025_11_25, &json!("2025-06-18")).unwrap(),
            V2025_06_18
        );
        assert_eq!(negotiate(V2025_06_18, &Value::Null).unwrap(), V2025_06_18);
    }

    #[test]
    fn an_unknown_answer_is_a_transport_error_naming_both_versions() {
        let e = negotiate(KnownVersion::V2025_11_25, &json!("1999-01-01")).unwrap_err();
        assert!(matches!(e, Error::Transport(_)), "{e:?}");
        let text = e.to_string();
        assert!(
            text.contains("1999-01-01") && text.contains("2025-11-25"),
            "{text}"
        );
        assert!(text.contains("--protocol-version"), "{text}");
        let not_a_string = negotiate(KnownVersion::V2025_11_25, &json!(3)).unwrap_err();
        assert!(
            not_a_string.to_string().contains("version 3"),
            "{not_a_string}"
        );
    }
}
