//! Read the `mcpServers` format that Claude Code, Claude Desktop, Cursor, and most
//! other hosts use, so a server configured once anywhere can be dialed from here.
//!
//! ```json
//! { "mcpServers": {
//!     "chrome":  { "command": "npx", "args": ["chrome-devtools-mcp@latest"], "env": {} },
//!     "remote":  { "type": "http", "url": "https://example.com/mcp", "headers": {} } } }
//! ```
//!
//! Claude Code's `~/.claude.json` nests the same shape under `projects.<path>`; those
//! are collected too. Nothing else in that file is read.
//!
//! A numeric `timeout` on an entry is seconds, as mcpc, Cline and Roo Code write it;
//! no host puts milliseconds in this field (Claude Code's `MCP_TIMEOUT` is an
//! environment variable, not part of the entry).

use crate::config::ServerConfig;
use serde_json::Value;
use std::collections::BTreeMap;

/// One server found in a config document and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub config: ServerConfig,
    /// "mcpServers" or "projects./some/path"
    pub scope: String,
    /// Something the user should know: an unsupported transport, a dropped field.
    pub note: Option<String>,
}

pub fn extract(doc: &Value) -> Vec<Found> {
    let mut out = Vec::new();
    if let Some(m) = doc.get("mcpServers").and_then(Value::as_object) {
        for (name, entry) in m {
            if let Some(f) = convert(name, entry, "mcpServers") {
                out.push(f);
            }
        }
    }
    if let Some(projects) = doc.get("projects").and_then(Value::as_object) {
        for (path, project) in projects {
            if let Some(m) = project.get("mcpServers").and_then(Value::as_object) {
                for (name, entry) in m {
                    if let Some(f) = convert(name, entry, &format!("projects.{path}")) {
                        out.push(f);
                    }
                }
            }
        }
    }
    out
}

fn string_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn convert(name: &str, entry: &Value, scope: &str) -> Option<Found> {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let mut note = None;

    let mut config = if let Some(url) = entry.get("url").and_then(Value::as_str) {
        if kind == "sse" {
            note = Some("configured as SSE; mcpdial speaks Streamable HTTP, which most servers also serve at the same URL".into());
        }
        let mut c = ServerConfig::http(url);
        c.headers = string_map(&entry["headers"]);
        c
    } else {
        let command = entry.get("command").and_then(Value::as_str)?;
        let mut argv = vec![command.to_string()];
        argv.extend(
            entry["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        let mut c = ServerConfig::stdio(shell_join(&argv));
        c.env = string_map(&entry["env"]);
        c.cwd = entry.get("cwd").and_then(Value::as_str).map(str::to_string);
        c
    };
    config.timeout = entry.get("timeout").and_then(Value::as_f64);

    Some(Found {
        name: name.to_string(),
        config,
        scope: scope.to_string(),
        note,
    })
}

/// Join argv back into one line that [`crate::transport::stdio::split_command`] will
/// split the same way: single-quote anything with whitespace or quote characters.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() {
                "''".to_string()
            } else if a
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\'))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::stdio::split_command;
    use serde_json::json;

    #[test]
    fn reads_both_shapes_and_nested_projects() {
        let doc = json!({
            "mcpServers": {
                "chrome": {"type":"stdio","command":"npx","args":["-y","chrome-devtools-mcp@latest"],"env":{"DEBUG":"1"}},
                "remote": {"type":"http","url":"https://x/mcp","headers":{"X-A":"1"}},
                "legacy": {"type":"sse","url":"https://y/sse"},
                "junk":   {"nothing": true}
            },
            "projects": {"/p": {"mcpServers": {"fs": {"command":"npx","args":["-y","fs","/tmp/my dir"],"cwd":"/p"}}}},
            "oauthAccount": {"secret": "must not matter"}
        });
        let found = extract(&doc);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["chrome", "legacy", "remote", "fs"]);

        let chrome = &found[0];
        assert_eq!(
            chrome.config.stdio.as_deref(),
            Some("npx -y chrome-devtools-mcp@latest")
        );
        assert_eq!(chrome.config.env["DEBUG"], "1");
        assert_eq!(chrome.scope, "mcpServers");

        let legacy = &found[1];
        assert!(legacy.note.as_deref().unwrap().contains("SSE"));

        let remote = &found[2];
        assert_eq!(remote.config.http.as_deref(), Some("https://x/mcp"));
        assert_eq!(remote.config.headers["X-A"], "1");

        let fs = &found[3];
        assert_eq!(fs.scope, "projects./p");
        assert_eq!(fs.config.cwd.as_deref(), Some("/p"));
        let argv = split_command(fs.config.stdio.as_deref().unwrap()).unwrap();
        assert_eq!(
            argv,
            ["npx", "-y", "fs", "/tmp/my dir"],
            "quoting round-trips"
        );
    }

    #[test]
    fn a_numeric_timeout_is_read_as_seconds() {
        let doc = json!({
            "mcpServers": {
                "slow":   {"command":"npx","args":["-y","slow"],"timeout":120},
                "remote": {"type":"http","url":"https://x/mcp","timeout":2.5},
                "words":  {"command":"npx","timeout":"soon"},
                "plain":  {"command":"npx"}
            }
        });
        let found = extract(&doc);
        let timeouts: Vec<(&str, Option<f64>)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.config.timeout))
            .collect();
        assert_eq!(
            timeouts,
            [
                ("plain", None),
                ("remote", Some(2.5)),
                ("slow", Some(120.0)),
                ("words", None),
            ]
        );
    }

    #[test]
    fn shell_join_round_trips_awkward_args() {
        let argv: Vec<String> = ["a", "b c", "it's", "q\"q", "", "back\\slash"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let joined = shell_join(&argv);
        assert_eq!(split_command(&joined).unwrap(), argv);
    }
}
