//! What a server says on its own initiative: while a request of ours is still
//! in flight, or on a subscription stream we opened and left open.
//!
//! A long tool call is silent for as long as it runs unless the server is
//! allowed to speak on the way. Two notifications carry that: `notifications/
//! progress`, which a server only sends for a request that handed it a
//! `progressToken`, and `notifications/message`, the server's own log. Two more
//! say that what the server offers has moved on: a `list_changed` for each of
//! the three lists, and `notifications/resources/updated` for one resource
//! somebody asked to follow.
//!
//! Everything here is parsing and filtering. Where the result is shown - and
//! whether it is shown at all - belongs to whoever is watching, because that
//! decision is the difference between a person at a terminal and a pipe whose
//! bytes are frozen.

use serde_json::Value;
use std::fmt;
use std::str::FromStr;

pub const PROGRESS_METHOD: &str = "notifications/progress";
pub const MESSAGE_METHOD: &str = "notifications/message";
pub const RESOURCE_UPDATED_METHOD: &str = "notifications/resources/updated";
pub const ACKNOWLEDGED_METHOD: &str = "notifications/subscriptions/acknowledged";

/// Which of the three lists a server offers, for the notification that says one
/// of them has changed and for re-reading it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Listing {
    Tools,
    Resources,
    Prompts,
}

impl Listing {
    pub const ALL: [Listing; 3] = [Listing::Tools, Listing::Resources, Listing::Prompts];

    pub const fn as_str(self) -> &'static str {
        match self {
            Listing::Tools => "tools",
            Listing::Resources => "resources",
            Listing::Prompts => "prompts",
        }
    }

    /// The notification a server sends when this list is no longer what it was.
    pub const fn changed_method(self) -> &'static str {
        match self {
            Listing::Tools => "notifications/tools/list_changed",
            Listing::Resources => "notifications/resources/list_changed",
            Listing::Prompts => "notifications/prompts/list_changed",
        }
    }

    /// The key 2026-07-28's `subscriptions/listen` filter opts in under.
    pub const fn listen_key(self) -> &'static str {
        match self {
            Listing::Tools => "toolsListChanged",
            Listing::Resources => "resourcesListChanged",
            Listing::Prompts => "promptsListChanged",
        }
    }

    fn of_method(method: &str) -> Option<Listing> {
        Listing::ALL
            .into_iter()
            .find(|l| l.changed_method() == method)
    }
}

impl fmt::Display for Listing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The eight severities RFC 5424 names, which MCP adopted as they are.
///
/// Ordered least to most severe, so a threshold reads `level >= Level::Warning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

impl Level {
    pub const ALL: [Level; 8] = [
        Level::Debug,
        Level::Info,
        Level::Notice,
        Level::Warning,
        Level::Error,
        Level::Critical,
        Level::Alert,
        Level::Emergency,
    ];

    /// What is shown when nobody said otherwise: a server's warnings and worse
    /// are worth a person's attention, and its chatter is not.
    pub const DEFAULT: Level = Level::Warning;

    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Notice => "notice",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Critical => "critical",
            Level::Alert => "alert",
            Level::Emergency => "emergency",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let wanted = s.trim().to_ascii_lowercase();
        Level::ALL
            .into_iter()
            .find(|l| l.as_str() == wanted)
            .ok_or_else(|| {
                let names: Vec<&str> = Level::ALL.iter().map(|l| l.as_str()).collect();
                format!(
                    "{s:?} is not a log level; the eight are {}",
                    names.join(", ")
                )
            })
    }
}

/// What one notification says, once it is worth showing.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// `notifications/progress`: how far along, out of how much, and what the
    /// server is doing.
    Progress {
        done: f64,
        total: Option<f64>,
        message: Option<String>,
    },
    /// `notifications/message`: the server's own log line.
    Log {
        level: Level,
        logger: Option<String>,
        text: String,
    },
    /// A `list_changed`: what the server offers is no longer what it offered,
    /// so anything holding a copy of that list is holding a stale one.
    ListChanged(Listing),
    /// `notifications/resources/updated`: one resource somebody asked to follow
    /// has new contents. The notification carries no contents of its own - it
    /// is an invitation to read the resource again.
    ResourceUpdated { uri: String },
    /// `notifications/subscriptions/acknowledged`: the first message on a
    /// 2026-07-28 subscription stream, carrying the subset of the filter the
    /// server agreed to honour.
    Acknowledged { notifications: Value },
}

/// One notification from a server, parsed, with the message it arrived in kept
/// whole for a caller that prints JSON rather than prose.
#[derive(Debug, Clone)]
pub struct Notice<'a> {
    pub message: &'a Value,
    pub body: Body,
}

impl Notice<'_> {
    pub fn level(&self) -> Option<Level> {
        match &self.body {
            Body::Log { level, .. } => Some(*level),
            _not_a_log_line => None,
        }
    }

    /// The line a person reads. Every part of it came from the server, so it is
    /// one line and holds no control character: a `\r` in a server's status
    /// message would otherwise wind the cursor back over what came before it.
    pub fn summary(&self) -> String {
        match &self.body {
            Body::Progress {
                done,
                total,
                message,
            } => {
                let counted = match total {
                    Some(total) => format!("{}/{}", number(*done), number(*total)),
                    None => number(*done),
                };
                match message {
                    Some(m) => format!("{counted} {m}"),
                    None => counted,
                }
            }
            Body::Log {
                level,
                logger,
                text,
            } => match logger {
                Some(name) => format!("server [{level}] {name}: {text}"),
                None => format!("server [{level}] {text}"),
            },
            Body::ListChanged(what) => format!("{what} changed"),
            Body::ResourceUpdated { uri } => format!("updated {uri}"),
            Body::Acknowledged { .. } => "subscribed".to_string(),
        }
    }
}

/// The notification `msg` carries, if it is one worth showing.
///
/// `token` is the `progressToken` of the request in flight, or `None` when none
/// was sent. Progress under any other token belongs to a request that has
/// already finished or to nobody at all: the server is within its rights to
/// send it and we have nothing to do with it, so it is dropped rather than
/// raised as an error. Anything else the server says on the way - a `ping`, a
/// notification of a kind mcpdial does not render, a frame that is not even a
/// notification - is likewise not our business here.
pub fn read<'a>(msg: &'a Value, token: Option<&Value>) -> Option<Notice<'a>> {
    let params = &msg["params"];
    let body = match msg["method"].as_str()? {
        PROGRESS_METHOD => {
            if token.is_none_or(|ours| &params["progressToken"] != ours) {
                return None;
            }
            Body::Progress {
                done: params["progress"].as_f64()?,
                total: params["total"].as_f64(),
                message: params["message"].as_str().map(one_line),
            }
        }
        MESSAGE_METHOD => Body::Log {
            level: level_of(&params["level"]),
            logger: params["logger"].as_str().map(one_line),
            text: one_line(&text_of(&params["data"])),
        },
        RESOURCE_UPDATED_METHOD => Body::ResourceUpdated {
            uri: one_line(params["uri"].as_str()?),
        },
        ACKNOWLEDGED_METHOD => Body::Acknowledged {
            notifications: params["notifications"].clone(),
        },
        changed => Body::ListChanged(Listing::of_method(changed)?),
    };
    Some(Notice { message: msg, body })
}

/// A level name the spec does not define is a broken server rather than an
/// urgent one, so it reads as the quietest level that still says something.
fn level_of(level: &Value) -> Level {
    level
        .as_str()
        .and_then(|name| name.parse().ok())
        .unwrap_or(Level::Info)
}

/// `data` is whatever the server chose to attach. A string is the message
/// itself; anything else is shown as the JSON it is.
fn text_of(data: &Value) -> String {
    match data {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A server's own text, fit for one line of a terminal: no control character
/// survives, and a newline becomes a space rather than a second line that
/// nothing would have cleared.
pub fn one_line(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(' '),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => write!(out, "\\x{:02x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// A count as a person writes it: `3`, not `3.0`, and the fraction only when
/// the server sent one.
fn number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn progress(params: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": PROGRESS_METHOD, "params": params})
    }

    #[test]
    fn the_eight_level_names_round_trip_and_nothing_else_parses() {
        for level in Level::ALL {
            assert_eq!(level.as_str().parse::<Level>().unwrap(), level);
        }
        assert_eq!("WARNING".parse::<Level>().unwrap(), Level::Warning);
        assert!("chatty".parse::<Level>().is_err());
        assert!(Level::Warning >= Level::DEFAULT);
        assert!(Level::Info < Level::DEFAULT);
    }

    #[test]
    fn progress_is_read_only_under_the_token_we_sent() {
        let ours = json!(7);
        let msg = progress(json!({"progressToken": 7, "progress": 3, "total": 10,
                                  "message": "fetching page 3"}));
        let notice = read(&msg, Some(&ours)).unwrap();
        assert_eq!(notice.summary(), "3/10 fetching page 3");

        let stale = progress(json!({"progressToken": 6, "progress": 1}));
        assert!(
            read(&stale, Some(&ours)).is_none(),
            "another request's token"
        );
        assert!(read(&msg, None).is_none(), "we asked for no progress");
        let untokened = progress(json!({"progress": 1}));
        assert!(read(&untokened, Some(&ours)).is_none(), "no token at all");
    }

    #[test]
    fn a_count_reads_as_a_count_and_a_missing_total_is_left_out() {
        let ours = json!(1);
        let whole = progress(json!({"progressToken": 1, "progress": 4}));
        assert_eq!(read(&whole, Some(&ours)).unwrap().summary(), "4");
        let fraction = progress(json!({"progressToken": 1, "progress": 0.5, "total": 1}));
        assert_eq!(read(&fraction, Some(&ours)).unwrap().summary(), "0.5/1");
        let no_number = progress(json!({"progressToken": 1, "message": "hi"}));
        assert!(read(&no_number, Some(&ours)).is_none());
    }

    #[test]
    fn a_log_message_names_its_level_and_its_logger() {
        let msg = json!({"jsonrpc": "2.0", "method": MESSAGE_METHOD,
                         "params": {"level": "warning", "logger": "crawler", "data": "slow"}});
        let notice = read(&msg, None).unwrap();
        assert_eq!(notice.level(), Some(Level::Warning));
        assert_eq!(notice.summary(), "server [warning] crawler: slow");

        let structured = json!({"jsonrpc": "2.0", "method": MESSAGE_METHOD,
                                "params": {"level": "error", "data": {"code": 5}}});
        assert_eq!(
            read(&structured, None).unwrap().summary(),
            "server [error] {\"code\":5}"
        );

        let nonsense = json!({"jsonrpc": "2.0", "method": MESSAGE_METHOD,
                              "params": {"level": "shouty", "data": "x"}});
        assert_eq!(read(&nonsense, None).unwrap().level(), Some(Level::Info));
    }

    #[test]
    fn nothing_else_the_server_sends_is_a_notice() {
        let ours = json!(1);
        for msg in [
            json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
            json!({"jsonrpc": "2.0", "id": "srv", "method": "ping"}),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled"}),
            // The invitation to re-read a resource is the URI; without one
            // there is nothing to re-read.
            json!({"jsonrpc": "2.0", "method": RESOURCE_UPDATED_METHOD, "params": {}}),
        ] {
            assert!(read(&msg, Some(&ours)).is_none(), "{msg}");
        }
    }

    #[test]
    fn each_list_has_its_own_notification_and_its_own_listen_key() {
        for what in Listing::ALL {
            let msg = json!({"jsonrpc": "2.0", "method": what.changed_method()});
            let notice = read(&msg, None).unwrap();
            assert_eq!(notice.body, Body::ListChanged(what));
            assert_eq!(notice.level(), None);
            assert_eq!(notice.summary(), format!("{what} changed"));
        }
        let keys: Vec<&str> = Listing::ALL.iter().map(|l| l.listen_key()).collect();
        assert_eq!(
            keys,
            [
                "toolsListChanged",
                "resourcesListChanged",
                "promptsListChanged"
            ]
        );
    }

    #[test]
    fn a_resource_update_names_its_uri_and_an_acknowledgement_its_filter() {
        let msg = json!({"jsonrpc": "2.0", "method": RESOURCE_UPDATED_METHOD,
                         "params": {"uri": "file:///notes.md\r"}});
        let notice = read(&msg, None).unwrap();
        assert_eq!(
            notice.body,
            Body::ResourceUpdated {
                uri: "file:///notes.md\\r".to_string()
            },
            "a server's own text cannot move the cursor"
        );
        assert_eq!(notice.summary(), "updated file:///notes.md\\r");

        let ack = json!({"jsonrpc": "2.0", "method": ACKNOWLEDGED_METHOD,
                         "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": 1},
                                    "notifications": {"toolsListChanged": true}}});
        let notice = read(&ack, None).unwrap();
        assert_eq!(
            notice.body,
            Body::Acknowledged {
                notifications: json!({"toolsListChanged": true})
            }
        );
    }

    #[test]
    fn a_servers_own_text_cannot_move_the_cursor_or_open_a_line() {
        let ours = json!(1);
        let msg = progress(json!({"progressToken": 1, "progress": 1,
                                  "message": "done\rrewound\nsecond\x1b[0m"}));
        assert_eq!(
            read(&msg, Some(&ours)).unwrap().summary(),
            "1 done\\rrewound second\\x1b[0m"
        );
    }
}
