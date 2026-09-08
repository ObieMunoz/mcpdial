//! What the prompt says about the session behind it.
//!
//! A session can be well, running out of token, or gone, and the prompt is the
//! only place that difference shows before a command fails on it.

use super::input::Lists;
use crate::present::{Health, Presenter};
use mcpdial::{client, Error};
use serde_json::{json, Value};

/// Where a line about something the server did on its own initiative goes.
///
/// This is the agent contract applied to asynchronous news. Under `--json` the
/// fact goes out as an object on stderr, which is exactly what a script waiting
/// for it reads. In prose it is company for a person watching, which is
/// [`Presenter::watched`] - the same answer the progress line is drawn on. A
/// pipe reading plain text gets nothing at all, because its bytes are frozen
/// and a server it has never heard of does not get to move them.
#[derive(Clone, Copy)]
pub(crate) enum Says {
    Wire,
    Prose,
    Nothing,
}

impl Says {
    pub(crate) fn choose(ui: &dyn Presenter, json: bool) -> Self {
        if json {
            Says::Wire
        } else if ui.watched() {
            Says::Prose
        } else {
            Says::Nothing
        }
    }

    pub(crate) fn tell(self, ui: &dyn Presenter, line: &str, wire: &Value) {
        match self {
            Says::Wire => ui.err_line(&wire.to_string()),
            Says::Prose => ui.aside(line),
            Says::Nothing => {}
        }
    }

    /// One fact about the session itself rather than about a server's list: the
    /// token running out, the transport dropping, the reconnect that followed.
    /// A new key on stderr, so nothing a script already reads there moves.
    pub(crate) fn about_the_session(self, ui: &dyn Presenter, state: &str, line: &str) {
        self.tell(
            ui,
            line,
            &json!({"session": {"state": state, "message": line}}),
        );
    }
}

/// How long before a token runs out the prompt starts saying so.
const TOKEN_RUNNING_OUT: u64 = 10 * 60;

impl Health {
    /// What the prompt shows now. A transport that has dropped outranks a token
    /// running out: a session that cannot carry a request has nothing left to
    /// spend a token on.
    pub(crate) fn read(dropped: bool, expires_at: Option<u64>, now: u64) -> Self {
        if dropped {
            Health::Lost
        } else if expires_at.is_some_and(|at| at <= now.saturating_add(TOKEN_RUNNING_OUT)) {
            Health::Expiring
        } else {
            Health::Fine
        }
    }
}

/// Whether this failure took the session with it.
///
/// A server that answers with an error is a server that is still there, and an
/// HTTP status is an answer as much as a JSON-RPC error object is. Only the
/// transport failing to carry the request at all - a dead socket, a server
/// process that exited, a reply that never came, a frame that made no sense -
/// says there is nothing on the other end to send the next command to.
pub(crate) fn dropped_the_session(error: &Error) -> bool {
    matches!(error, Error::Transport(_))
}

/// How long is left, in the largest unit that still says something true.
fn how_long(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// The one line a token running out is worth, and how to get another one where
/// there is a saved server to ask for it under.
pub(crate) fn expiring_line(r: &client::Resolved, left: u64) -> String {
    let when = match left {
        0 => "the token has expired".to_string(),
        s => format!("the token expires in {}", how_long(s)),
    };
    match r.saved {
        true => format!("{when}; `mcpdial login {}` renews it", r.name),
        false => when,
    }
}

/// The one line a shell prints when it is connected: what answered, and how
/// much of itself it offered. A list nobody fetched says nothing rather than
/// zero, because zero is a fact about the server and this would be a fact about
/// us.
pub(crate) fn connected_line(server_info: &Value, lists: &Lists) -> String {
    let si = &server_info["serverInfo"];
    let mut line = format!(
        "connected  {} {}",
        si["name"].as_str().unwrap_or("?"),
        si["version"].as_str().unwrap_or("")
    );
    let counts = [
        ("tool", lists.tools.as_ref()),
        ("resource", lists.resources.as_ref()),
        ("prompt", lists.prompts.as_ref()),
    ];
    for (what, found) in counts {
        if let Some(items) = found {
            let n = items.len();
            let plural = if n == 1 { "" } else { "s" };
            line.push_str(&format!("  {n} {what}{plural}"));
        }
    }
    line.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpdial::protocol::INVALID_PARAMS;
    use mcpdial::ServerConfig;
    use serde_json::json;

    #[test]
    fn the_prompt_says_lost_over_expiring_and_expiring_only_when_it_is_close() {
        const NOW: u64 = 1_700_000_000;
        let in_secs = |s: u64| Some(NOW + s);

        assert_eq!(Health::read(false, None, NOW), Health::Fine);
        assert_eq!(Health::read(false, in_secs(3600), NOW), Health::Fine);
        assert_eq!(
            Health::read(false, in_secs(TOKEN_RUNNING_OUT + 1), NOW),
            Health::Fine
        );
        assert_eq!(
            Health::read(false, in_secs(TOKEN_RUNNING_OUT), NOW),
            Health::Expiring
        );
        assert_eq!(Health::read(false, Some(NOW - 1), NOW), Health::Expiring);

        // A transport that dropped outranks both: a token is no use to a
        // session that cannot carry a request.
        assert_eq!(Health::read(true, None, NOW), Health::Lost);
        assert_eq!(Health::read(true, in_secs(1), NOW), Health::Lost);
    }

    #[test]
    fn only_the_transport_failing_says_the_session_is_gone() {
        assert!(dropped_the_session(&Error::transport("server exited")));
        // An answer, however unwelcome, is a server that is still there.
        for still_there in [
            Error::Rpc {
                code: INVALID_PARAMS,
                message: "no".into(),
                data: None,
            },
            Error::Http {
                status: 401,
                body: String::new(),
                www_authenticate: None,
            },
            Error::usage("typo"),
            Error::auth("login first"),
            Error::config("unreadable"),
        ] {
            assert!(!dropped_the_session(&still_there), "{still_there:?}");
        }
    }

    #[test]
    fn one_connected_line_names_the_server_and_counts_what_it_offered() {
        let info = json!({"serverInfo": {"name": "chrome-devtools-mcp", "version": "0.6.0"}});
        let listed = |n: usize| Some(vec![json!({}); n]);

        let everything = Lists {
            tools: listed(26),
            resources: listed(3),
            templates: listed(1),
            prompts: listed(1),
        };
        assert_eq!(
            connected_line(&info, &everything),
            "connected  chrome-devtools-mcp 0.6.0  26 tools  3 resources  1 prompt"
        );

        // A list nobody fetched is left out; a list fetched and empty is not.
        let tools_only = Lists {
            tools: listed(1),
            resources: listed(0),
            ..Lists::default()
        };
        assert_eq!(
            connected_line(&info, &tools_only),
            "connected  chrome-devtools-mcp 0.6.0  1 tool  0 resources"
        );
        assert_eq!(
            connected_line(&json!({}), &Lists::default()),
            "connected  ?"
        );
    }

    #[test]
    fn a_token_running_out_is_one_line_that_says_how_to_renew_it() {
        let saved = client::Resolved {
            name: "web".into(),
            config: ServerConfig::http("https://example.test/mcp"),
            saved: true,
        };
        let ad_hoc = client::Resolved {
            saved: false,
            ..saved.clone()
        };
        assert_eq!(
            expiring_line(&saved, 9 * 60 + 30),
            "the token expires in 9m; `mcpdial login web` renews it"
        );
        assert_eq!(
            expiring_line(&saved, 0),
            "the token has expired; `mcpdial login web` renews it"
        );
        // Nothing to log in to under an ad-hoc URL, so nothing is suggested.
        assert_eq!(expiring_line(&ad_hoc, 45), "the token expires in 45s");
        assert_eq!(how_long(3 * 3600 + 4 * 60), "3h04m");
    }
}
