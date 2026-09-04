//! JSON-RPC 2.0 framing as MCP uses it, and nothing else.
//!
//! Three facts are the whole protocol:
//!
//! 1. Every message is a JSON-RPC 2.0 object.
//! 2. Transport is HTTP POST to one endpoint, or newline-delimited JSON over stdio.
//! 3. The methods you actually need are `initialize`, `tools/list` and `tools/call`.

use serde_json::{json, Value};
use std::fmt;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const CLIENT_NAME: &str = "mcpdial";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub type Result<T> = std::result::Result<T, Error>;

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
            }
        ) || matches!(self, Error::Http { status: 401, .. })
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
        // One message may be split across several consecutive data: lines.
        let payload: String = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect();
        if payload.is_empty() {
            return Ok(None);
        }
        return Ok(Some(serde_json::from_str(&payload)?));
    }

    Ok(Some(serde_json::from_str(body)?))
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
}
