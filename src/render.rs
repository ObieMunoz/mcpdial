//! What mcpdial composed itself, on its way to a terminal or a pipe.
//!
//! Nothing here talks to a server: every function takes something already
//! fetched and decides how it reads. The split that matters is between a value a
//! program parses and a line a person scans - `--json` picks the first, and the
//! snapshots under `tests/snapshots/` hold both.

use crate::failure::Failure;
use crate::media::{file_stem, rendered, MediaFiles};
use crate::output;
use crate::output::{As, Output, Payload};
use crate::present::{truncate_at, Presenter};
use mcpdial::client::{describe_params, Listing, Status};
use mcpdial::session::render_content;
use mcpdial::{client, Backend, Credential, ServerConfig};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn print_json(ui: &dyn Presenter, v: &impl serde::Serialize) {
    ui.json(&serde_json::to_string_pretty(v).expect("a JSON value is serializable"));
}

pub(crate) fn print_value(ui: &dyn Presenter, v: &Value, compact: bool) {
    ui.json(&output::document(v, compact));
}

/// A note on stderr: prose for a human, `{"note": ...}` under `--json`, where
/// every line on either stream has to be an object.
pub(crate) fn print_note(ui: &dyn Presenter, note: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "note": note }).to_string());
    } else {
        ui.note(note);
    }
}

/// A hint on stderr: prose for a human, `{"hint": ...}` under `--json`, where
/// every line on either stream has to be an object.
pub(crate) fn print_hint(ui: &dyn Presenter, hint: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "hint": hint }).to_string());
    } else {
        ui.err_line(hint);
    }
}

/// A tool result on stdout, and a marker on stderr when the tool reported an
/// error: a failure whose text is a plain sentence otherwise reads as success
/// to anyone not checking `$?`. Under `--json` the object carries `isError`
/// itself. Returns whether the tool reported an error.
pub(crate) fn print_tool_result(
    ui: &dyn Presenter,
    out: &Output,
    result: &Value,
    text: &str,
    json: bool,
    one_line: bool,
) -> Result<bool, Failure> {
    let failed = result["isError"].as_bool().unwrap_or(false);
    let payload = if json {
        Payload::Json {
            value: result,
            one_line,
        }
    } else {
        Payload::Text(text)
    };
    let sent = out.deliver(payload, failed)?;
    output::show(ui, sent, shape(json), json, || {
        if json {
            print_value(ui, result, one_line);
        } else if !text.is_empty() {
            ui.text(text);
        }
    });
    if !json && failed {
        ui.err_line("(tool reported an error)");
    }
    Ok(failed)
}

/// A `tools/call` result on stdout, however it was fetched: the one the call
/// waited for, and the one a finished task was holding. Returns whether the
/// tool reported an error.
pub(crate) fn printed_result(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    save_dir: Option<&Path>,
    stem: &str,
) -> Result<(bool, String), Failure> {
    let files = MediaFiles {
        dir: save_dir,
        stem: file_stem(stem),
    };
    let text = rendered(ui, result, json, &files, render_content)?;
    let failed = ui.paged(|| print_tool_result(ui, out, result, &text, json, false))?;
    Ok((failed, text))
}

/// A result goes out as a document under `--json` and as the server's own text
/// otherwise, cut or whole.
pub(crate) fn shape(json: bool) -> As {
    if json {
        As::Json
    } else {
        As::Text
    }
}

/// The allow and deny lists a server has, under the keys they are saved as.
pub(crate) fn tool_lists(cfg: &ServerConfig) -> Vec<(&'static str, Vec<String>)> {
    [("allow", &cfg.allow), ("deny", &cfg.deny)]
        .into_iter()
        .filter(|(_, patterns)| !patterns.is_empty())
        .map(|(list, patterns)| (list, patterns.clone()))
        .collect()
}

/// One line per list that is set, for the receipt a human reads.
pub(crate) fn tool_lists_lines(lists: &[(&str, Vec<String>)]) -> Vec<String> {
    lists
        .iter()
        .map(|(list, patterns)| format!("{list}: {}", patterns.join(", ")))
        .collect()
}

/// How a credential store is named in a sentence.
pub(crate) fn credential_store(backend: Backend) -> &'static str {
    match backend {
        Backend::File => "credentials.json",
        Backend::Keychain => "the OS keychain",
    }
}

pub(crate) const LISTING_HEADERS: [&str; 8] = [
    "NAME", "TYPE", "STATUS", "AGE", "AUTH", "DAEMON", "SERVER", "TOOLS",
];

/// `tools` with no target: the probes under a key, as `tools TARGET` puts its
/// own list under `tools`. A bare array could never grow a field beside them.
#[derive(serde::Serialize)]
pub(crate) struct Servers<'a> {
    pub(crate) servers: &'a [client::Probe],
}

/// What was saved about one server, as every `ls --json` row carries it. A probe
/// adds its status fields on top of these rather than in place of them, so a
/// program reading a row never has to know whether `--no-probe` was passed.
pub(crate) fn saved_row(name: &str, cfg: &ServerConfig, credential: bool, running: bool) -> Value {
    json!({
        "name": name, "kind": cfg.kind(), "location": cfg.location(),
        "headers": cfg.headers, "token_env": cfg.token_env,
        "credential": credential,
        "source": cfg.source, "timeout": cfg.timeout,
        "running": running,
        "allow": cfg.allow, "deny": cfg.deny,
    })
}

/// One server as `ls` shows it, which is also what `add` shows after dialing.
pub(crate) fn listing_row(l: &Listing) -> Vec<String> {
    vec![
        l.name.clone(),
        l.kind.into(),
        l.status.label(),
        age_label(l.age_seconds),
        auth_label(l),
        daemon_label(l.running),
        l.server
            .clone()
            .or_else(|| l.status.detail().map(truncate))
            .unwrap_or_else(|| "-".into()),
        l.tools.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
    ]
}

pub(crate) fn auth_label(l: &Listing) -> String {
    match (l.auth, &l.status) {
        (client::AuthUsed::Env, _) => l
            .token_env
            .as_ref()
            .map_or_else(|| "env".into(), |var| format!("${var}")),
        (client::AuthUsed::Saved, _) => "saved".into(),
        (client::AuthUsed::None, Status::AuthRequired) => "needed".into(),
        (client::AuthUsed::None, Status::TokenRejected) => "rejected".into(),
        (client::AuthUsed::None, _) => "-".into(),
    }
}

pub(crate) fn daemon_label(running: bool) -> String {
    if running { "running" } else { "-" }.into()
}

pub(crate) fn age_label(seconds: u64) -> String {
    match seconds {
        0 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s => format!("{}m", s / 60),
    }
}

pub(crate) fn expiry_label(cred: &Credential) -> String {
    match cred.expires_at {
        None => "no expiry recorded".into(),
        Some(t) => {
            let now = mcpdial::config::now();
            if t <= now {
                "expired".into()
            } else {
                let secs = t - now;
                if secs >= 86_400 {
                    format!("expires in {}d", secs / 86_400)
                } else if secs >= 3600 {
                    format!("expires in {}h", secs / 3600)
                } else {
                    format!("expires in {}m", (secs / 60).max(1))
                }
            }
        }
    }
}

pub(crate) fn truncate(s: &str) -> String {
    truncate_at(s.lines().next().unwrap_or(""), 60)
}

pub(crate) fn print_tools(ui: &dyn Presenter, tools: &[Value], long: bool) {
    // A `--long` listing has room to spell out what calling a tool does; the
    // short one carries only the hint a caller cannot afford to miss.
    let hints: &dyn Fn(&Value) -> String = if long {
        &client::hint_tags
    } else {
        &client::hint_mark
    };
    ui.named(tools, long, "parameters", &describe_params, hints);
}

/// A `--long` listing reads like a document and may run past the screen; the
/// short one is a summary that belongs on it.
pub(crate) fn listed(ui: &dyn Presenter, long: bool, print: impl FnOnce()) {
    if long {
        ui.paged(print);
    } else {
        print();
    }
}

pub(crate) fn print_prompts(ui: &dyn Presenter, prompts: &[Value], long: bool) {
    ui.named(prompts, long, "arguments", &describe_prompt_args, &|_| {
        String::new()
    });
}

/// A prompt's arguments carry no schema: every one of them is a string.
pub(crate) fn describe_prompt_args(prompt: &Value) -> Vec<String> {
    prompt["arguments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|a| {
            let mut line = a["name"].as_str().unwrap_or("?").to_string();
            if a["required"].as_bool().unwrap_or(false) {
                line.push_str(" (required)");
            }
            if let Some(d) = a["description"].as_str() {
                let first = d.trim().lines().next().unwrap_or("");
                if !first.is_empty() {
                    line.push_str(&format!(" - {first}"));
                }
            }
            line
        })
        .collect()
}
