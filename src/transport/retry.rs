//! One more attempt, and only where it is provably safe.
//!
//! A request is sent again in two cases, and at most once: when it is idempotent,
//! so a second delivery changes nothing; or when it failed in a way that proves
//! the server never processed it. Everything else fails on the first attempt,
//! because a retried tool call that did go through would run twice.

use crate::protocol::Error;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::io::ErrorKind;
use std::time::Duration;

/// The pause before the second attempt when the server named none.
pub const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Requests whose second delivery changes nothing, whatever the server did with
/// the first one.
const IDEMPOTENT_METHODS: &[&str] = &[
    "server/discover",
    "initialize",
    "tools/list",
    "resources/list",
    "resources/templates/list",
    "prompts/list",
    "ping",
];

/// How an HTTP request failed, as far as retrying is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Not a byte came back: refused, reset, or no address to connect to. The
    /// request cannot have been processed. Carries the reason, for the trace.
    Unreached(&'static str),
    /// The server answered with this status, and perhaps said how long to wait.
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// The reply broke off after it had begun, so the request was processed.
    Interrupted,
    /// Retrying would not help: a timeout, a redirect, a malformed reply.
    Final,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Unreached(reason) => f.write_str(reason),
            Failure::Status { status, .. } => write!(f, "HTTP {status}"),
            Failure::Interrupted => f.write_str("an interrupted reply"),
            Failure::Final => f.write_str("a failure"),
        }
    }
}

/// One failed attempt: the error the caller gets if there is no second one, and
/// what the failure was for deciding that.
#[derive(Debug)]
pub struct Failed {
    pub error: Error,
    pub failure: Failure,
}

impl Failed {
    pub fn final_(error: Error) -> Self {
        Self {
            error,
            failure: Failure::Final,
        }
    }
}

/// Whether a second attempt is allowed at all, and the longest it may wait.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub enabled: bool,
    pub timeout: Duration,
}

impl Retry {
    /// How long to wait before the one retry, or `None` to fail now.
    ///
    /// `session_started` is whether the server has issued a session id: a
    /// gateway's 502/503/504 proves nothing about the server behind it once
    /// there is state there to lose.
    pub fn delay(
        &self,
        idempotent: bool,
        session_started: bool,
        failure: &Failure,
    ) -> Option<Duration> {
        if !self.enabled || !worth_retrying(idempotent, session_started, failure) {
            return None;
        }
        Some(match failure {
            Failure::Status {
                retry_after: Some(wait),
                ..
            } => (*wait).min(self.timeout),
            _ => RETRY_DELAY,
        })
    }
}

fn worth_retrying(idempotent: bool, session_started: bool, failure: &Failure) -> bool {
    match failure {
        Failure::Unreached(_) => true,
        Failure::Status { status: 429, .. } => true,
        Failure::Status {
            status: 502..=504, ..
        } => idempotent || !session_started,
        Failure::Status { .. } | Failure::Final => false,
        Failure::Interrupted => idempotent,
    }
}

/// Classify a request that got no response object at all.
pub fn before_any_reply(e: &ureq::Error) -> Failure {
    match e {
        ureq::Error::HostNotFound => Failure::Unreached("DNS failure"),
        ureq::Error::ConnectionFailed => Failure::Unreached("connection failure"),
        ureq::Error::Io(io) => Failure::Unreached(match io.kind() {
            ErrorKind::ConnectionRefused => "connection refused",
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => "connection reset",
            ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof => "connection closed",
            _ => "connection error",
        }),
        _ => Failure::Final,
    }
}

/// The `Retry-After` header as a wait, when it is one in seconds. The header's
/// other form, an HTTP date, is left to the default delay.
pub fn retry_after(header: Option<&str>) -> Option<Duration> {
    header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Whether delivering `payload` twice is the same as delivering it once.
pub fn is_idempotent(payload: &Value, idempotent_tools: &HashSet<String>) -> bool {
    match payload["method"].as_str() {
        Some(method) if IDEMPOTENT_METHODS.contains(&method) => true,
        Some("tools/call") => payload["params"]["name"]
            .as_str()
            .is_some_and(|name| idempotent_tools.contains(name)),
        _ => false,
    }
}

/// Learn from a `tools/list` page which tools declare `idempotentHint`. A tool
/// listed again without it is forgotten, so the set follows the server.
pub fn note_tools(idempotent_tools: &mut HashSet<String>, reply: &Value) {
    let Some(tools) = reply["result"]["tools"].as_array() else {
        return;
    };
    for tool in tools {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        if tool["annotations"]["idempotentHint"] == true {
            idempotent_tools.insert(name.to_string());
        } else {
            idempotent_tools.remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ON: Retry = Retry {
        enabled: true,
        timeout: Duration::from_secs(60),
    };

    fn status(status: u16) -> Failure {
        Failure::Status {
            status,
            retry_after: None,
        }
    }

    #[test]
    fn a_request_the_server_never_saw_is_always_retried() {
        let refused = Failure::Unreached("connection refused");
        assert_eq!(ON.delay(false, true, &refused), Some(RETRY_DELAY));
        assert_eq!(ON.delay(false, false, &refused), Some(RETRY_DELAY));
        assert_eq!(ON.delay(true, true, &refused), Some(RETRY_DELAY));
    }

    #[test]
    fn a_gateway_error_is_retried_until_there_is_a_session_to_lose() {
        for code in [502, 503, 504] {
            assert!(ON.delay(false, false, &status(code)).is_some(), "{code}");
            assert!(ON.delay(false, true, &status(code)).is_none(), "{code}");
            assert!(
                ON.delay(true, true, &status(code)).is_some(),
                "{code}: idempotent, so the session is not at risk"
            );
        }
    }

    #[test]
    fn a_rate_limit_is_retried_after_what_it_asks_for_capped_at_the_timeout() {
        let asks_for = |secs| Failure::Status {
            status: 429,
            retry_after: Some(Duration::from_secs(secs)),
        };
        assert_eq!(
            ON.delay(false, true, &asks_for(2)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            ON.delay(false, true, &asks_for(3600)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(ON.delay(false, true, &status(429)), Some(RETRY_DELAY));
    }

    #[test]
    fn what_is_never_retried() {
        for code in [400, 401, 403, 404, 500] {
            assert!(ON.delay(true, false, &status(code)).is_none(), "{code}");
        }
        assert!(ON.delay(true, false, &Failure::Final).is_none());
        assert!(ON.delay(false, false, &Failure::Interrupted).is_none());
        let off = Retry {
            enabled: false,
            ..ON
        };
        assert!(off
            .delay(true, false, &Failure::Unreached("connection refused"))
            .is_none());
    }

    #[test]
    fn an_interrupted_reply_is_retried_only_when_the_request_is_idempotent() {
        assert_eq!(
            ON.delay(true, true, &Failure::Interrupted),
            Some(RETRY_DELAY)
        );
        assert!(ON.delay(false, false, &Failure::Interrupted).is_none());
    }

    #[test]
    fn ureq_errors_before_a_reply_are_named_for_the_trace() {
        let io = |kind| ureq::Error::Io(std::io::Error::from(kind));
        assert_eq!(
            before_any_reply(&io(ErrorKind::ConnectionRefused)),
            Failure::Unreached("connection refused")
        );
        assert_eq!(
            before_any_reply(&io(ErrorKind::ConnectionReset)),
            Failure::Unreached("connection reset")
        );
        assert_eq!(
            before_any_reply(&io(ErrorKind::UnexpectedEof)),
            Failure::Unreached("connection closed")
        );
        assert_eq!(
            before_any_reply(&ureq::Error::HostNotFound),
            Failure::Unreached("DNS failure")
        );
        assert_eq!(
            before_any_reply(&ureq::Error::Timeout(ureq::Timeout::Global)),
            Failure::Final
        );
        assert_eq!(
            before_any_reply(&ureq::Error::Tls("bad certificate")),
            Failure::Final
        );
        assert_eq!(
            Failure::Unreached("connection reset").to_string(),
            "connection reset"
        );
        assert_eq!(status(503).to_string(), "HTTP 503");
    }

    #[test]
    fn retry_after_is_read_in_seconds_only() {
        assert_eq!(retry_after(Some("3")), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(Some(" 0 ")), Some(Duration::ZERO));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn idempotent_requests_are_the_listed_methods_and_tools_that_say_so() {
        let mut tools = HashSet::new();
        let call = |name: &str| json!({"method": "tools/call", "params": {"name": name}});
        for method in IDEMPOTENT_METHODS {
            assert!(
                is_idempotent(&json!({"method": method}), &tools),
                "{method}"
            );
        }
        assert!(!is_idempotent(&json!({"method": "resources/read"}), &tools));
        assert!(!is_idempotent(
            &json!({"method": "notifications/initialized"}),
            &tools
        ));
        assert!(!is_idempotent(&call("lookup"), &tools));

        note_tools(
            &mut tools,
            &json!({"result": {"tools": [
                {"name": "lookup", "annotations": {"idempotentHint": true}},
                {"name": "send", "annotations": {"idempotentHint": false}},
                {"name": "unmarked"},
            ]}}),
        );
        assert!(is_idempotent(&call("lookup"), &tools));
        assert!(!is_idempotent(&call("send"), &tools));
        assert!(!is_idempotent(&call("unmarked"), &tools));

        note_tools(
            &mut tools,
            &json!({"result": {"tools": [{"name": "lookup"}]}}),
        );
        assert!(
            !is_idempotent(&call("lookup"), &tools),
            "a tool that dropped the hint is no longer trusted"
        );
        note_tools(&mut tools, &json!({"result": {}}));
    }
}
