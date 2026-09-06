//! What a transport tells whoever is watching. `-v` renders it on stderr as
//! prose; `--trace FILE` appends it as JSON Lines, one object per message or
//! transport event, for a bug report or a script.

use super::retry::Failure;
use crate::config::open_private_append;
use crate::protocol::{Error, Result};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `$MCPDIAL_TRACE`: the file `--trace` would name.
pub const ENV_TRACE: &str = "MCPDIAL_TRACE";

/// Which wire an event happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire<'a> {
    /// JSON-RPC posted to this URL.
    Http(&'a str),
    /// An OAuth request to this URL: not JSON-RPC, and full of secrets.
    OAuth(&'a str),
    /// A local process's pipes: written on its stdin, read on its stdout.
    Stdio,
    /// A daemon's socket, both ways.
    Socket,
}

impl Wire<'_> {
    /// The name in the trace file.
    fn transport(self) -> &'static str {
        match self {
            Wire::Http(_) | Wire::OAuth(_) => "http",
            Wire::Stdio => "stdio",
            Wire::Socket => "socket",
        }
    }

    /// What `-v` calls a message going out.
    fn outbound(self) -> String {
        match self {
            Wire::Http(url) | Wire::OAuth(url) => format!("POST {url}"),
            Wire::Stdio => "stdin".into(),
            Wire::Socket => "socket".into(),
        }
    }

    /// What `-v` calls a message coming in.
    fn inbound(self) -> &'static str {
        match self {
            Wire::Http(_) | Wire::OAuth(_) => "HTTP",
            Wire::Stdio => "stdout",
            Wire::Socket => "socket",
        }
    }
}

/// What the reader made of a message that arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The answer to what we sent.
    Reply,
    /// A request the server made of us.
    ServerRequest,
    /// A notification, or a reply to nothing we asked.
    Other,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Reply => "",
            Kind::ServerRequest => " (server request)",
            Kind::Other => " (other message)",
        }
    }
}

/// One thing that happened on a transport.
#[derive(Debug)]
pub enum TraceEvent<'a> {
    Sent {
        wire: Wire<'a>,
        message: &'a Value,
    },
    Received {
        wire: Wire<'a>,
        message: &'a Value,
        kind: Kind,
    },
    /// A line that arrived on a stream and was not JSON.
    NotJson {
        wire: Wire<'a>,
        line: &'a str,
    },
    /// A request with no message in it is going out: a probe, a session DELETE.
    HttpRequest {
        wire: Wire<'a>,
        method: &'static str,
        purpose: String,
    },
    /// What one HTTP request got back, before any decoding.
    HttpReply {
        wire: Wire<'a>,
        method: &'static str,
        status: u16,
        content_type: Option<String>,
        /// The raw body, where the framing rather than the message is what
        /// `-v` should show. Never given for an OAuth reply: its secrets are
        /// redacted from the decoded message, not from the bytes.
        body: Option<&'a str>,
        elapsed: Duration,
    },
    /// One HTTP request that got no reply at all.
    HttpFailed {
        wire: Wire<'a>,
        method: &'static str,
        error: String,
        elapsed: Duration,
    },
    /// The one more attempt a transient failure is allowed.
    Retrying {
        wire: Wire<'a>,
        after: &'a Failure,
    },
    /// A stdio server's process has started.
    Spawned {
        pid: u32,
        command: &'a [String],
    },
    /// A stdio server's process has ended: its exit status (`None` for a
    /// signal) and the last lines it wrote to stderr.
    Exited {
        status: Option<i32>,
        stderr_tail: Vec<String>,
    },
}

/// Where trace output goes when `--verbose` or `--trace` is on.
pub type Logger = Box<dyn Fn(&TraceEvent<'_>)>;

pub(crate) fn silent() -> Logger {
    Box::new(|_| {})
}

/// The fields of an OAuth exchange that are secrets, or good as one.
const SECRET_FIELDS: [&str; 4] = ["access_token", "refresh_token", "client_secret", "code"];

/// `value` with every secret field, at any depth, replaced by `"****"`.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, v)| {
                    let shown = if SECRET_FIELDS.contains(&key.as_str()) {
                        json!("****")
                    } else {
                        redact(v)
                    };
                    (key.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// The message as either sink may show it: an OAuth exchange with its secrets
/// redacted, anything else as it was.
fn shown<'a>(wire: Wire<'_>, message: &'a Value) -> Cow<'a, Value> {
    match wire {
        Wire::OAuth(_) => Cow::Owned(redact(message)),
        _ => Cow::Borrowed(message),
    }
}

// -- the stderr sink ----------------------------------------------------------------

/// The `-v` line for `event`, or `None` where there is nothing to add.
pub fn describe(event: &TraceEvent<'_>) -> Option<String> {
    Some(match event {
        TraceEvent::Sent { wire, message } => {
            format!("-> {}\n   {}", wire.outbound(), shown(*wire, message))
        }
        // The reply that carried it was shown in full, framing and all.
        TraceEvent::Received {
            wire: Wire::Http(_),
            ..
        } => return None,
        TraceEvent::Received {
            wire: wire @ Wire::OAuth(_),
            message,
            ..
        } => format!("   {}", shown(*wire, message)),
        TraceEvent::Received {
            wire,
            message,
            kind,
        } => format!("<- {}{}\n   {}", wire.inbound(), kind.label(), message),
        TraceEvent::NotJson { wire, line } => {
            format!("<- {} (ignored, not JSON)\n   {line}", wire.inbound())
        }
        TraceEvent::HttpRequest {
            wire,
            method,
            purpose,
        } => format!("-> {method} {} ({purpose})", url_of(*wire)),
        TraceEvent::HttpReply {
            status,
            content_type,
            body,
            ..
        } => {
            let mut line = format!("<- HTTP {status}");
            if let Some(ct) = content_type {
                line.push(' ');
                line.push_str(ct);
            }
            if let Some(text) = body {
                line.push_str("\n   ");
                line.push_str(text.trim());
            }
            line
        }
        TraceEvent::HttpFailed {
            wire,
            method,
            error,
            ..
        } => format!("<- {method} {} failed: {error}", url_of(*wire)),
        TraceEvent::Retrying { after, .. } => format!("retrying after {after} (1 of 1)"),
        TraceEvent::Spawned { pid, command } => {
            format!("spawned {} (pid {pid})", command.join(" "))
        }
        TraceEvent::Exited {
            status: Some(code), ..
        } => format!("server exited with status {code}"),
        TraceEvent::Exited { status: None, .. } => "server was killed by a signal".into(),
    })
}

fn url_of(wire: Wire<'_>) -> &str {
    match wire {
        Wire::Http(url) | Wire::OAuth(url) => url,
        Wire::Stdio | Wire::Socket => "",
    }
}

// -- the file sink ------------------------------------------------------------------

/// `--trace FILE`: an append-only, owner-only file shared by every transport
/// this process opens.
#[derive(Debug, Clone)]
pub struct Trace {
    file: Arc<Mutex<File>>,
}

impl Trace {
    /// The file `--trace` names, else the one `$MCPDIAL_TRACE` names, else none.
    pub fn from_flag_or_env(flag: Option<&Path>) -> Result<Option<Self>> {
        let path = match flag {
            Some(path) => path.to_path_buf(),
            None => match std::env::var_os(ENV_TRACE).filter(|v| !v.is_empty()) {
                Some(path) => PathBuf::from(path),
                None => return Ok(None),
            },
        };
        Self::open(&path).map(Some)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = open_private_append(path)
            .map_err(|e| Error::config(format!("trace file {}: {e}", path.display())))?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn record(&self, target: &str, event: &TraceEvent<'_>) {
        let Some(line) = line(target, SystemTime::now(), event) else {
            return;
        };
        // A trace that cannot be written is a missing line, not a failed call.
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = file.write_all(line.as_bytes());
    }
}

/// The JSON Lines record of `event`, newline included, or `None` where the file
/// has nothing to say (a request that is about to go out is recorded once it
/// has an outcome).
fn line(target: &str, at: SystemTime, event: &TraceEvent<'_>) -> Option<String> {
    let http = |wire: Wire<'_>, method: &str, mut detail: Value| {
        detail["method"] = json!(method);
        detail["url"] = json!(url_of(wire));
        detail
    };
    let (wire, dir, rest): (Wire<'_>, &str, Vec<(&str, Value)>) = match event {
        TraceEvent::Sent { wire, message } => (
            *wire,
            "send",
            vec![("message", shown(*wire, message).into_owned())],
        ),
        TraceEvent::Received { wire, message, .. } => (
            *wire,
            "recv",
            vec![("message", shown(*wire, message).into_owned())],
        ),
        TraceEvent::NotJson { wire, line } => (
            *wire,
            "event",
            vec![
                ("event", json!("not_json")),
                ("detail", json!({ "line": line })),
            ],
        ),
        TraceEvent::HttpRequest { .. } => return None,
        TraceEvent::HttpReply {
            wire,
            method,
            status,
            content_type,
            elapsed,
            ..
        } => (
            *wire,
            "event",
            vec![
                ("event", json!("http")),
                (
                    "detail",
                    http(
                        *wire,
                        method,
                        json!({
                            "status": status,
                            "content_type": content_type.as_deref().unwrap_or(""),
                            "elapsed_ms": elapsed.as_millis() as u64,
                        }),
                    ),
                ),
            ],
        ),
        TraceEvent::HttpFailed {
            wire,
            method,
            error,
            elapsed,
        } => (
            *wire,
            "event",
            vec![
                ("event", json!("http")),
                (
                    "detail",
                    http(
                        *wire,
                        method,
                        json!({ "error": error, "elapsed_ms": elapsed.as_millis() as u64 }),
                    ),
                ),
            ],
        ),
        TraceEvent::Retrying { wire, after } => (
            *wire,
            "event",
            vec![
                ("event", json!("retry")),
                ("detail", json!({ "after": after.to_string() })),
            ],
        ),
        TraceEvent::Spawned { pid, command } => (
            Wire::Stdio,
            "event",
            vec![
                ("event", json!("spawn")),
                ("detail", json!({ "pid": pid, "command": command })),
            ],
        ),
        TraceEvent::Exited {
            status,
            stderr_tail,
        } => (
            Wire::Stdio,
            "event",
            vec![
                ("event", json!("exit")),
                (
                    "detail",
                    json!({ "status": status, "stderr_tail": stderr_tail }),
                ),
            ],
        ),
    };
    // Written by hand so the keys come out in the documented order; every key is
    // a literal that needs no escaping, and every value is serialized JSON.
    let mut fields = vec![
        ("t", json!(iso8601(at))),
        ("dir", json!(dir)),
        ("transport", json!(wire.transport())),
        ("target", json!(target)),
    ];
    fields.extend(rest);
    let body = fields
        .iter()
        .map(|(key, value)| format!("\"{key}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("{{{body}}}\n"))
}

/// `at` as `2026-09-05T10:11:12.345Z`.
fn iso8601(at: SystemTime) -> String {
    let since_epoch = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let (hour, minute, second) = (secs % 86_400 / 3600, secs % 3600 / 60, secs % 60);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        since_epoch.subsec_millis()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, millis: u32) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(u64::from(millis))
    }

    fn parsed(line: &str) -> Value {
        assert!(line.ends_with('\n'), "one line, terminated: {line:?}");
        assert_eq!(line.matches('\n').count(), 1, "one line only: {line:?}");
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    #[test]
    fn timestamps_are_utc_to_the_millisecond() {
        assert_eq!(iso8601(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(at(951_782_400, 7)), "2000-02-29T00:00:00.007Z");
        assert_eq!(iso8601(at(1_788_013_872, 345)), "2026-08-29T14:31:12.345Z");
        assert_eq!(iso8601(at(4_102_444_800, 999)), "2100-01-01T00:00:00.999Z");
    }

    #[test]
    fn every_secret_field_is_masked_at_any_depth_and_nothing_else_is() {
        let exchange = json!({
            "grant_type": "authorization_code",
            "code": "one-time-code",
            "client_secret": "hunter2",
            "nested": {"access_token": "at", "refresh_token": "rt", "expires_in": 3600},
            "list": [{"code": "x"}, "code", 7],
            "response_types": ["code"],
        });
        let masked = redact(&exchange);
        assert_eq!(masked["code"], "****");
        assert_eq!(masked["client_secret"], "****");
        assert_eq!(masked["nested"]["access_token"], "****");
        assert_eq!(masked["nested"]["refresh_token"], "****");
        assert_eq!(masked["list"][0]["code"], "****");
        assert_eq!(masked["grant_type"], "authorization_code");
        assert_eq!(masked["nested"]["expires_in"], 3600);
        assert_eq!(masked["list"][1], "code", "a value is not a field");
        assert_eq!(masked["response_types"][0], "code");
        assert!(!masked.to_string().contains("hunter2"));
    }

    #[test]
    fn an_oauth_exchange_is_redacted_by_both_sinks_and_a_tool_call_is_not() {
        let token_request = json!({"grant_type": "refresh_token", "refresh_token": "rt-secret"});
        let token_reply = json!({"access_token": "at-secret", "token_type": "Bearer"});
        let sent = TraceEvent::Sent {
            wire: Wire::OAuth("https://as/token"),
            message: &token_request,
        };
        let received = TraceEvent::Received {
            wire: Wire::OAuth("https://as/token"),
            message: &token_reply,
            kind: Kind::Reply,
        };
        for event in [&sent, &received] {
            let prose = describe(event).unwrap();
            assert!(prose.contains("****"), "{prose}");
            assert!(!prose.contains("secret"), "{prose}");
            let file = line("work", at(0, 0), event).unwrap();
            assert!(file.contains("****"), "{file}");
            assert!(!file.contains("secret"), "{file}");
        }
        assert_eq!(
            describe(&sent).unwrap(),
            "-> POST https://as/token\n   {\"grant_type\":\"refresh_token\",\"refresh_token\":\"****\"}"
        );

        // A tool that takes source code is traced as it was sent.
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"code": "print(1)"}}});
        let event = TraceEvent::Sent {
            wire: Wire::Http("https://x/mcp"),
            message: &call,
        };
        let record = parsed(&line("x", at(0, 0), &event).unwrap());
        assert_eq!(record["message"]["params"]["arguments"]["code"], "print(1)");
        assert!(describe(&event).unwrap().contains("print(1)"));
    }

    #[test]
    fn a_message_line_has_the_documented_shape_and_key_order() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let text = line(
            "wiki",
            at(1_788_013_872, 345),
            &TraceEvent::Sent {
                wire: Wire::Http("https://x/mcp"),
                message: &message,
            },
        )
        .unwrap();
        assert!(
            text.starts_with(
                "{\"t\":\"2026-08-29T14:31:12.345Z\",\"dir\":\"send\",\"transport\":\"http\",\"target\":\"wiki\",\"message\":{"
            ),
            "{text}"
        );
        let record = parsed(&text);
        assert_eq!(record["message"], message);

        let received = line(
            "stdio:./srv",
            at(0, 0),
            &TraceEvent::Received {
                wire: Wire::Stdio,
                message: &message,
                kind: Kind::ServerRequest,
            },
        )
        .unwrap();
        let record = parsed(&received);
        assert_eq!(record["dir"], "recv");
        assert_eq!(record["transport"], "stdio");
        assert_eq!(record["target"], "stdio:./srv");
        assert!(record.get("event").is_none());
    }

    #[test]
    fn events_carry_their_detail() {
        let reply = TraceEvent::HttpReply {
            wire: Wire::Http("https://x/mcp"),
            method: "POST",
            status: 200,
            content_type: Some("text/event-stream".into()),
            body: Some("event: message\ndata: {}\n\n"),
            elapsed: Duration::from_millis(123),
        };
        let record = parsed(&line("x", at(0, 0), &reply).unwrap());
        assert_eq!(record["dir"], "event");
        assert_eq!(record["event"], "http");
        assert_eq!(
            record["detail"],
            json!({"status": 200, "content_type": "text/event-stream", "elapsed_ms": 123,
                   "method": "POST", "url": "https://x/mcp"})
        );
        assert!(record.get("message").is_none());
        assert_eq!(
            describe(&reply).unwrap(),
            "<- HTTP 200 text/event-stream\n   event: message\ndata: {}"
        );

        let failed = TraceEvent::HttpFailed {
            wire: Wire::Http("https://x/mcp"),
            method: "POST",
            error: "connection refused".into(),
            elapsed: Duration::from_millis(5),
        };
        let record = parsed(&line("x", at(0, 0), &failed).unwrap());
        assert_eq!(record["detail"]["error"], "connection refused");
        assert!(record["detail"].get("status").is_none());

        let exited = TraceEvent::Exited {
            status: Some(9),
            stderr_tail: vec!["boom".into()],
        };
        let record = parsed(&line("x", at(0, 0), &exited).unwrap());
        assert_eq!(record["transport"], "stdio");
        assert_eq!(record["event"], "exit");
        assert_eq!(
            record["detail"],
            json!({"status": 9, "stderr_tail": ["boom"]})
        );
        assert_eq!(describe(&exited).unwrap(), "server exited with status 9");

        let command = vec!["npx".to_string(), "-y".to_string(), "srv".to_string()];
        let spawned = TraceEvent::Spawned {
            pid: 42,
            command: &command,
        };
        let record = parsed(&line("x", at(0, 0), &spawned).unwrap());
        assert_eq!(
            record["detail"],
            json!({"pid": 42, "command": ["npx", "-y", "srv"]})
        );

        let retry = TraceEvent::Retrying {
            wire: Wire::Http("https://x/mcp"),
            after: &Failure::Status {
                status: 503,
                retry_after: None,
            },
        };
        assert_eq!(
            describe(&retry).unwrap(),
            "retrying after HTTP 503 (1 of 1)"
        );
        let record = parsed(&line("x", at(0, 0), &retry).unwrap());
        assert_eq!(record["event"], "retry");
        assert_eq!(record["detail"]["after"], "HTTP 503");
    }

    #[test]
    fn the_verbose_prose_is_what_it_always_was() {
        let message = json!({"id": 1, "jsonrpc": "2.0", "method": "ping"});
        let prose = |event: &TraceEvent<'_>| describe(event);
        assert_eq!(
            prose(&TraceEvent::Sent {
                wire: Wire::Http("https://x/mcp"),
                message: &message
            })
            .unwrap(),
            format!("-> POST https://x/mcp\n   {message}")
        );
        assert_eq!(
            prose(&TraceEvent::Sent {
                wire: Wire::Stdio,
                message: &message
            })
            .unwrap(),
            format!("-> stdin\n   {message}")
        );
        assert_eq!(
            prose(&TraceEvent::Received {
                wire: Wire::Socket,
                message: &message,
                kind: Kind::Other
            })
            .unwrap(),
            format!("<- socket (other message)\n   {message}")
        );
        assert_eq!(
            prose(&TraceEvent::Received {
                wire: Wire::Http("https://x/mcp"),
                message: &message,
                kind: Kind::Reply
            }),
            None,
            "the reply body was already shown"
        );
        assert_eq!(
            prose(&TraceEvent::NotJson {
                wire: Wire::Stdio,
                line: "ready"
            })
            .unwrap(),
            "<- stdout (ignored, not JSON)\n   ready"
        );
        assert_eq!(
            prose(&TraceEvent::HttpRequest {
                wire: Wire::Http("https://x/mcp"),
                method: "DELETE",
                purpose: "session abc".into()
            })
            .unwrap(),
            "-> DELETE https://x/mcp (session abc)"
        );
        assert_eq!(
            prose(&TraceEvent::HttpReply {
                wire: Wire::Http("https://x/mcp"),
                method: "DELETE",
                status: 405,
                content_type: None,
                body: None,
                elapsed: Duration::ZERO
            })
            .unwrap(),
            "<- HTTP 405"
        );
        assert_eq!(
            line(
                "x",
                at(0, 0),
                &TraceEvent::HttpRequest {
                    wire: Wire::Http("https://x/mcp"),
                    method: "GET",
                    purpose: "legacy transport probe".into()
                }
            ),
            None,
            "the file records the outcome, not the intent"
        );
    }

    #[test]
    fn the_file_is_appended_to_and_shared_between_clones() {
        let dir = std::env::temp_dir().join(format!(
            "mcpdial-trace-{}-{}",
            std::process::id(),
            at(0, 0).elapsed().unwrap_or_default().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        {
            let trace = Trace::open(&path).unwrap();
            let other = trace.clone();
            trace.record(
                "a",
                &TraceEvent::Sent {
                    wire: Wire::Stdio,
                    message: &message,
                },
            );
            other.record(
                "b",
                &TraceEvent::HttpRequest {
                    wire: Wire::Http("https://x/mcp"),
                    method: "GET",
                    purpose: "probe".into(),
                },
            );
        }
        Trace::open(&path).unwrap().record(
            "c",
            &TraceEvent::Exited {
                status: Some(0),
                stderr_tail: Vec::new(),
            },
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let targets: Vec<String> = text
            .lines()
            .map(|l| {
                parsed(&format!("{l}\n"))["target"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(targets, ["a", "c"], "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a trace can hold private tool output");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_path_is_a_config_error() {
        let missing = Path::new("/nonexistent-dir-for-mcpdial/trace.jsonl");
        let err = Trace::open(missing).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(
            err.to_string().contains("nonexistent-dir-for-mcpdial"),
            "{err}"
        );
        assert!(Trace::from_flag_or_env(Some(missing)).is_err());
    }
}
