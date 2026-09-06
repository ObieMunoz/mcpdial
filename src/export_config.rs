//! Write saved servers back out in the shape a host reads, so a set that `ls`
//! shows working can be handed to Claude Code, Cursor, VS Code or Codex without
//! retyping. The inverse of [`crate::import_config`], and tested against it.
//!
//! Three shapes are written: the `mcpServers` object Claude Code, Claude Desktop,
//! Cursor and Windsurf read, VS Code's `servers` object with a `type` on every
//! entry, and Codex's `[mcp_servers.NAME]` TOML tables. Nothing secret is written:
//! a saved token stays in `credentials.json`, and a `token_env` becomes whatever
//! the host has for reading a variable at startup. What a shape cannot hold is
//! dropped and named in a note.

use crate::config::ServerConfig;
use crate::import_config::toml;
use crate::protocol::{Error, Result};
use crate::transport::stdio::split_command;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The shapes `--format` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `{"mcpServers": {...}}`: Claude Code, Claude Desktop, Cursor, Windsurf.
    McpServers,
    /// `{"servers": {...}}` with `"type"` on each entry: VS Code's `mcp.json`.
    VsCode,
    /// `[mcp_servers.NAME]` tables: Codex's `config.toml`.
    Codex,
}

pub const FORMAT_NAMES: [&str; 3] = ["mcpservers", "vscode", "codex"];

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(name: &str) -> std::result::Result<Self, String> {
        match name {
            "mcpservers" => Ok(Format::McpServers),
            "vscode" => Ok(Format::VsCode),
            "codex" => Ok(Format::Codex),
            _ => Err(format!(
                "unknown format {name}; one of {}",
                FORMAT_NAMES.join(", ")
            )),
        }
    }
}

impl Format {
    /// The key the host keeps its servers under.
    pub fn key(self) -> &'static str {
        match self {
            Format::McpServers => "mcpServers",
            Format::VsCode => "servers",
            Format::Codex => "mcp_servers",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Format::McpServers => "the mcpServers hosts",
            Format::VsCode => "VS Code",
            Format::Codex => "Codex",
        }
    }
}

/// One saved server in a host's shape: its fields in the order they are written,
/// and what the shape could not hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub fields: Vec<(String, Value)>,
    pub notes: Vec<String>,
}

impl Entry {
    pub fn to_value(&self) -> Value {
        Value::Object(self.fields.iter().cloned().collect())
    }
}

/// The host's entry for one server. `has_saved_token` says a credential with an
/// access token is saved for it, which is never written and so gets a note.
pub fn to_host_entry(cfg: &ServerConfig, format: Format, has_saved_token: bool) -> Result<Entry> {
    let mut fields: Vec<(String, Value)> = Vec::new();
    let mut notes = Vec::new();
    let mut field = |key: &str, value: Value| fields.push((key.to_string(), value));

    if let Some(url) = &cfg.http {
        if format != Format::Codex {
            field("type", json!("http"));
        }
        field("url", json!(url));
        let mut headers = cfg.headers.clone();
        match (&cfg.token_env, format) {
            (Some(var), Format::Codex) => field("bearer_token_env_var", json!(var)),
            (Some(var), Format::VsCode) => {
                headers.insert("Authorization".into(), format!("Bearer ${{env:{var}}}"));
            }
            (Some(var), Format::McpServers) => {
                headers.insert("Authorization".into(), format!("Bearer ${{{var}}}"));
                notes.push(format!(
                    "token_env {var} written as the header Authorization: Bearer ${{{var}}}; \
                     Claude Code expands that, Claude Desktop, Cursor and Windsurf do not"
                ));
            }
            (None, _) => {}
        }
        if !headers.is_empty() {
            let key = if format == Format::Codex {
                "http_headers"
            } else {
                "headers"
            };
            field(key, json!(headers));
        }
    } else {
        let argv = split_command(cfg.stdio.as_deref().unwrap_or(""))?;
        let Some((command, args)) = argv.split_first() else {
            return Err(Error::config("the stdio command line has no words"));
        };
        if format == Format::VsCode {
            field("type", json!("stdio"));
        }
        field("command", json!(command));
        field("args", json!(args));
        if !cfg.env.is_empty() {
            field("env", json!(cfg.env));
        }
        if let Some(cwd) = &cfg.cwd {
            field("cwd", json!(cwd));
        }
    }

    if let Some(secs) = cfg.timeout {
        match format {
            Format::McpServers => field("timeout", seconds(secs)),
            Format::Codex => {
                field("startup_timeout_sec", seconds(secs));
                field("tool_timeout_sec", seconds(secs));
            }
            Format::VsCode => notes.push(format!(
                "timeout {secs}s dropped: VS Code has no field for it"
            )),
        }
    }
    if let Some(v) = &cfg.protocol_version {
        notes.push(format!(
            "protocol_version {v} dropped: {} offers its own",
            format.host()
        ));
    }
    if !cfg.allow.is_empty() || !cfg.deny.is_empty() {
        let is_pattern = |p: &String| p.contains(['*', '?']);
        if format == Format::Codex && !cfg.allow.iter().chain(&cfg.deny).any(is_pattern) {
            if !cfg.allow.is_empty() {
                field("enabled_tools", json!(cfg.allow));
            }
            if !cfg.deny.is_empty() {
                field("disabled_tools", json!(cfg.deny));
            }
        } else if format == Format::Codex {
            notes.push(
                "allow and deny lists dropped: Codex takes exact tool names, not patterns".into(),
            );
        } else {
            notes.push(format!(
                "allow and deny lists dropped: {} keep no tool lists in a server entry",
                format.host()
            ));
        }
    }
    if has_saved_token && cfg.token_env.is_none() {
        notes.push("exported without its saved token; the host will need its own login".into());
    }
    Ok(Entry { fields, notes })
}

/// A whole number of seconds is written as an integer, the way hosts write it.
fn seconds(secs: f64) -> Value {
    if secs.fract() == 0.0 && secs >= 0.0 && secs < u64::MAX as f64 {
        json!(secs as u64)
    } else {
        json!(secs)
    }
}

/// What `export` prints: the document on stdout, the notes on stderr.
#[derive(Debug, Clone, PartialEq)]
pub struct Export {
    pub document: Document,
    /// `(server, note)` in export order.
    pub notes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Json(Value),
    Toml(String),
}

/// The servers named, in the order given (each once), or every saved one.
/// A name that is not saved is a usage error.
pub fn select<'a>(
    servers: &'a BTreeMap<String, ServerConfig>,
    names: &[String],
) -> Result<Vec<(&'a str, &'a ServerConfig)>> {
    if names.is_empty() {
        return Ok(servers.iter().map(|(n, c)| (n.as_str(), c)).collect());
    }
    let mut out: Vec<(&str, &ServerConfig)> = Vec::new();
    for name in names {
        let (name, cfg) = servers
            .get_key_value(name)
            .ok_or_else(|| Error::usage(format!("no server named {name:?}")))?;
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, cfg));
        }
    }
    Ok(out)
}

/// The document for `servers`, spliced into `existing` (the text of the host's
/// current file) when given: its entries under the host's key are replaced or
/// added, and the rest of the file is kept.
pub fn export(
    servers: &[(&str, &ServerConfig)],
    has_saved_token: impl Fn(&str) -> bool,
    format: Format,
    existing: Option<&str>,
) -> Result<Export> {
    let mut entries: Vec<(String, Entry)> = Vec::with_capacity(servers.len());
    let mut notes = Vec::new();
    for (name, cfg) in servers {
        let entry = to_host_entry(cfg, format, has_saved_token(name))
            .map_err(|e| Error::config(format!("{name}: {e}")))?;
        notes.extend(entry.notes.iter().map(|n| (name.to_string(), n.clone())));
        entries.push((name.to_string(), entry));
    }
    let document = match format {
        Format::Codex => Document::Toml(merge_toml(existing.unwrap_or(""), &entries)?),
        _ => Document::Json(merge_json(existing, format.key(), &entries)?),
    };
    Ok(Export { document, notes })
}

fn merge_json(existing: Option<&str>, key: &str, entries: &[(String, Entry)]) -> Result<Value> {
    let mut doc = match existing {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| Error::config(format!("the file to merge into: {e}")))?,
        None => Value::Object(Map::new()),
    };
    let root = doc
        .as_object_mut()
        .ok_or_else(|| Error::config("the file to merge into is not a JSON object"))?;
    let table = root
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::config(format!("the file to merge into: {key} is not an object")))?;
    for (name, entry) in entries {
        table.insert(name.clone(), entry.to_value());
    }
    Ok(doc)
}

/// `existing` is read by the same reader `import` uses, so a file it cannot read
/// is refused rather than spliced blind; then each `[mcp_servers.NAME]` table of
/// an exported server, and any `[mcp_servers.NAME.sub]` under it, is replaced by
/// the new table where the first one stood, and the rest are appended.
fn merge_toml(existing: &str, entries: &[(String, Entry)]) -> Result<String> {
    toml::parse(existing).map_err(|e| Error::config(format!("the file to merge into: {e}")))?;
    let mut pieces: Vec<String> = Vec::new();
    let mut written: Vec<&str> = Vec::new();
    for span in table_spans(existing) {
        let owner = span
            .path
            .as_ref()
            .filter(|p| p.len() >= 2 && p[0] == "mcp_servers")
            .and_then(|p| entries.iter().find(|(name, _)| *name == p[1]));
        match owner {
            None if !span.text.trim().is_empty() => pieces.push(span.text.to_string()),
            None => {}
            Some((name, entry)) if !written.contains(&name.as_str()) => {
                written.push(name);
                pieces.push(toml_table(name, entry));
            }
            Some(_) => {}
        }
    }
    for (name, entry) in entries {
        if !written.contains(&name.as_str()) {
            pieces.push(toml_table(name, entry));
        }
    }
    Ok(pieces
        .iter()
        .map(|p| p.trim_end_matches(['\n', '\r']))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// A stretch of a TOML file: what comes before the first table header (no
/// `path`), or one header and the lines up to the next.
struct Span<'a> {
    path: Option<Vec<String>>,
    text: &'a str,
}

fn table_spans(text: &str) -> Vec<Span<'_>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut start = 0;
    let mut path: Option<Vec<String>> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if let Some(header) = toml::table_header(line) {
            spans.push(Span {
                path: path.take(),
                text: &text[start..at],
            });
            start = at;
            path = Some(header);
        }
        at += line.len();
    }
    spans.push(Span {
        path,
        text: &text[start..],
    });
    spans
}

fn toml_table(name: &str, entry: &Entry) -> String {
    let mut out = format!("[mcp_servers.{}]\n", toml_key(name));
    for (key, value) in &entry.fields {
        out.push_str(&format!("{} = {}\n", toml_key(key), toml_value(value)));
    }
    out
}

fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

fn toml_value(v: &Value) -> String {
    match v {
        Value::String(s) => toml_string(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        Value::Object(map) => format!(
            "{{ {} }}",
            map.iter()
                .map(|(k, v)| format!("{} = {}", toml_key(k), toml_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Numbers, booleans and null print the same in both languages.
        other => other.to_string(),
    }
}

fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_config::{extract, read};
    use std::path::Path;

    fn fixture() -> Value {
        json!({
            "mcpServers": {
                "fs": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/my dir", "it's"],
                    "env": {"DEBUG": "1", "ROOT": "${ROOT}"},
                    "cwd": "/p",
                    "timeout": 120
                },
                "wiki": {
                    "type": "http",
                    "url": "https://mcp.deepwiki.com/mcp",
                    "headers": {"X-Tenant": "acme"},
                    "timeout": 2.5
                }
            }
        })
    }

    fn configs(doc: &Value) -> Vec<(String, ServerConfig)> {
        extract(doc)
            .into_iter()
            .map(|f| (f.name, f.config))
            .collect()
    }

    fn export_all(servers: &[(String, ServerConfig)], format: Format) -> Export {
        let refs: Vec<(&str, &ServerConfig)> =
            servers.iter().map(|(n, c)| (n.as_str(), c)).collect();
        export(&refs, |_| false, format, None).unwrap()
    }

    #[test]
    fn mcpservers_round_trips_through_import() {
        let before = configs(&fixture());
        let out = export_all(&before, Format::McpServers);
        let Document::Json(doc) = &out.document else {
            panic!("json")
        };
        assert_eq!(
            doc["mcpServers"]["fs"]["args"],
            json!([
                "-y",
                "@modelcontextprotocol/server-filesystem",
                "/tmp/my dir",
                "it's"
            ])
        );
        assert_eq!(doc["mcpServers"]["fs"]["timeout"], json!(120));
        assert_eq!(doc["mcpServers"]["wiki"]["type"], "http");
        assert_eq!(doc["mcpServers"]["wiki"]["timeout"], json!(2.5));
        assert!(doc["mcpServers"]["fs"].get("type").is_none());
        assert_eq!(configs(doc), before);
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    #[test]
    fn vscode_round_trips_through_import() {
        let before = configs(&fixture());
        let out = export_all(&before, Format::VsCode);
        let Document::Json(doc) = &out.document else {
            panic!("json")
        };
        assert_eq!(doc["servers"]["fs"]["type"], "stdio");
        assert_eq!(doc["servers"]["wiki"]["type"], "http");
        assert!(doc.get("mcpServers").is_none());
        let after = configs(doc);
        // VS Code has nowhere to keep a timeout; everything else comes back.
        let mut expected = before.clone();
        for (_, c) in &mut expected {
            c.timeout = None;
        }
        assert_eq!(after, expected);
        assert_eq!(
            out.notes,
            [
                (
                    "fs".to_string(),
                    "timeout 120s dropped: VS Code has no field for it".to_string()
                ),
                (
                    "wiki".to_string(),
                    "timeout 2.5s dropped: VS Code has no field for it".to_string()
                ),
            ]
        );
    }

    #[test]
    fn codex_round_trips_through_import() {
        let mut before = configs(&fixture());
        before[1].1.token_env = Some("API_TOKEN".into());
        before[1].1.allow = vec!["read_*".into()];
        before[0].1.deny = vec!["delete_file".into()];
        let out = export_all(&before, Format::Codex);
        let Document::Toml(text) = &out.document else {
            panic!("toml")
        };
        assert!(
            text.starts_with("[mcp_servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", "),
            "{text}"
        );
        assert!(
            text.contains("env = { DEBUG = \"1\", ROOT = \"${ROOT}\" }"),
            "{text}"
        );
        assert!(
            text.contains("\ndisabled_tools = [\"delete_file\"]\n"),
            "{text}"
        );
        assert!(
            text.contains("\nstartup_timeout_sec = 120\ntool_timeout_sec = 120\n"),
            "{text}"
        );
        assert!(text.contains("\n\n[mcp_servers.wiki]\nurl = \"https://mcp.deepwiki.com/mcp\"\nbearer_token_env_var = \"API_TOKEN\"\nhttp_headers = { X-Tenant = \"acme\" }\n"), "{text}");
        assert!(!text.contains("enabled_tools"), "{text}");
        assert_eq!(
            out.notes,
            [(
                "wiki".to_string(),
                "allow and deny lists dropped: Codex takes exact tool names, not patterns"
                    .to_string()
            )]
        );

        let after: Vec<(String, ServerConfig)> = read(Path::new("config.toml"), text)
            .unwrap()
            .into_iter()
            .map(|f| (f.name, f.config))
            .collect();
        // Codex's timeouts and tool lists are its own fields, which import does
        // not read; the transport, arguments, env, cwd and token variable come back.
        let mut expected = before.clone();
        for (_, c) in &mut expected {
            c.timeout = None;
            c.allow.clear();
            c.deny.clear();
        }
        assert_eq!(after, expected);
    }

    #[test]
    fn token_env_takes_each_hosts_form_and_a_saved_token_is_named() {
        let mut cfg = ServerConfig::http("https://x/mcp");
        cfg.token_env = Some("TOKEN".into());

        let e = to_host_entry(&cfg, Format::McpServers, true).unwrap();
        assert_eq!(e.to_value()["headers"]["Authorization"], "Bearer ${TOKEN}");
        assert_eq!(e.notes.len(), 1, "{:?}", e.notes);
        assert!(
            e.notes[0].starts_with("token_env TOKEN written as the header"),
            "{}",
            e.notes[0]
        );

        let e = to_host_entry(&cfg, Format::VsCode, false).unwrap();
        assert_eq!(
            e.to_value()["headers"]["Authorization"],
            "Bearer ${env:TOKEN}"
        );
        assert!(e.notes.is_empty(), "{:?}", e.notes);

        let e = to_host_entry(&cfg, Format::Codex, false).unwrap();
        assert_eq!(e.to_value()["bearer_token_env_var"], "TOKEN");
        assert!(e.to_value().get("http_headers").is_none());
        assert!(e.notes.is_empty(), "{:?}", e.notes);

        cfg.token_env = None;
        for format in [Format::McpServers, Format::VsCode, Format::Codex] {
            let e = to_host_entry(&cfg, format, true).unwrap();
            assert_eq!(
                e.notes,
                ["exported without its saved token; the host will need its own login"],
                "{format:?}"
            );
            assert!(!e.to_value().to_string().contains("Authorization"));
        }
    }

    #[test]
    fn what_no_host_holds_is_dropped_with_a_note() {
        let mut cfg = ServerConfig::stdio("srv");
        cfg.protocol_version = Some("2025-06-18".into());
        cfg.allow = vec!["read_*".into()];
        cfg.source = Some(crate::config::Source {
            catalog: Some("x".into()),
            ..Default::default()
        });
        let e = to_host_entry(&cfg, Format::McpServers, false).unwrap();
        assert_eq!(e.to_value(), json!({"command": "srv", "args": []}));
        assert_eq!(
            e.notes,
            [
                "protocol_version 2025-06-18 dropped: the mcpServers hosts offers its own",
                "allow and deny lists dropped: the mcpServers hosts keep no tool lists in a server entry",
            ]
        );
        let e = to_host_entry(&cfg, Format::Codex, false).unwrap();
        assert_eq!(
            e.notes,
            [
                "protocol_version 2025-06-18 dropped: Codex offers its own",
                "allow and deny lists dropped: Codex takes exact tool names, not patterns",
            ]
        );
        cfg.allow = vec!["read_file".into(), "list_dir".into()];
        let e = to_host_entry(&cfg, Format::Codex, false).unwrap();
        assert_eq!(
            e.to_value()["enabled_tools"],
            json!(["read_file", "list_dir"])
        );
        assert_eq!(e.notes.len(), 1);

        let e = to_host_entry(&ServerConfig::stdio("'open"), Format::McpServers, false);
        assert!(e.unwrap_err().to_string().contains("unterminated"));
    }

    #[test]
    fn select_keeps_the_order_given_and_refuses_a_stranger() {
        let mut servers = BTreeMap::new();
        servers.insert("b".to_string(), ServerConfig::stdio("b"));
        servers.insert("a".to_string(), ServerConfig::stdio("a"));
        fn names<'a>(v: Vec<(&'a str, &ServerConfig)>) -> Vec<&'a str> {
            v.into_iter().map(|(n, _)| n).collect()
        }
        assert_eq!(names(select(&servers, &[]).unwrap()), ["a", "b"]);
        assert_eq!(
            names(select(&servers, &["b".into(), "a".into(), "b".into()]).unwrap()),
            ["b", "a"]
        );
        let e = select(&servers, &["c".into()]).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
        assert!(e.to_string().contains("no server named \"c\""), "{e}");
    }

    #[test]
    fn merge_json_replaces_under_the_key_and_keeps_the_rest() {
        let existing = json!({
            "inputs": [{"id": "t"}],
            "servers": {
                "keep": {"command": "old"},
                "fs": {"command": "stale", "args": ["x"], "envFile": ".env"}
            },
            "other": 1
        })
        .to_string();
        let servers = [
            ("fs".to_string(), ServerConfig::stdio("npx -y fs")),
            ("new".to_string(), ServerConfig::http("https://n/mcp")),
        ];
        let refs: Vec<(&str, &ServerConfig)> =
            servers.iter().map(|(n, c)| (n.as_str(), c)).collect();
        let out = export(&refs, |_| false, Format::VsCode, Some(&existing)).unwrap();
        let Document::Json(doc) = out.document else {
            panic!("json")
        };
        assert_eq!(
            doc,
            json!({
                "inputs": [{"id": "t"}],
                "servers": {
                    "keep": {"command": "old"},
                    "fs": {"type": "stdio", "command": "npx", "args": ["-y", "fs"]},
                    "new": {"type": "http", "url": "https://n/mcp"}
                },
                "other": 1
            })
        );

        let e = export(&refs, |_| false, Format::McpServers, Some("[1]")).unwrap_err();
        assert!(e.to_string().contains("not a JSON object"), "{e}");
        let e = export(
            &refs,
            |_| false,
            Format::McpServers,
            Some("{\"mcpServers\": 3}"),
        )
        .unwrap_err();
        assert!(e.to_string().contains("mcpServers is not an object"), "{e}");
        let e = export(&refs, |_| false, Format::McpServers, Some("{")).unwrap_err();
        assert!(matches!(e, Error::Config(_)), "{e}");
    }

    #[test]
    fn merge_toml_replaces_a_table_with_its_subtables_and_keeps_the_rest() {
        let existing = "model = \"o3\"\n\n[profiles.fast]\nmodel = \"o4-mini\"\n\n[mcp_servers.docs]\ncommand = \"stale\"\n\n[mcp_servers.docs.env]\nOLD = \"1\"\n\n[mcp_servers.\"web api\"]\nurl = \"https://keep\"\n# trailing comment\n";
        let mut docs = ServerConfig::stdio("npx -y docs");
        docs.env.insert("LOG".into(), "debug".into());
        let servers = [
            ("docs".to_string(), docs),
            ("new".to_string(), ServerConfig::http("https://n/mcp")),
        ];
        let refs: Vec<(&str, &ServerConfig)> =
            servers.iter().map(|(n, c)| (n.as_str(), c)).collect();
        let out = export(&refs, |_| false, Format::Codex, Some(existing)).unwrap();
        let Document::Toml(text) = out.document else {
            panic!("toml")
        };
        assert_eq!(
            text,
            "model = \"o3\"\n\n[profiles.fast]\nmodel = \"o4-mini\"\n\n[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"docs\"]\nenv = { LOG = \"debug\" }\n\n[mcp_servers.\"web api\"]\nurl = \"https://keep\"\n# trailing comment\n\n[mcp_servers.new]\nurl = \"https://n/mcp\""
        );
        let found = read(Path::new("config.toml"), &text).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["docs", "new", "web api"]);
        assert_eq!(found[0].config.env["LOG"], "debug");
        assert!(!found[0].config.env.contains_key("OLD"));

        let e = export(&refs, |_| false, Format::Codex, Some("[[mcp_servers]]\n")).unwrap_err();
        assert!(e.to_string().contains("arrays of tables"), "{e}");
    }

    #[test]
    fn toml_writer_quotes_what_needs_it() {
        assert_eq!(toml_key("web-api_2"), "web-api_2");
        assert_eq!(toml_key("web api"), "\"web api\"");
        assert_eq!(toml_key(""), "\"\"");
        assert_eq!(
            toml_string("a\"b\\c\nd\te\u{1}"),
            "\"a\\\"b\\\\c\\nd\\te\\u0001\""
        );
        assert_eq!(toml_value(&json!([])), "[]");
        assert_eq!(toml_value(&json!({})), "{}");
        assert_eq!(toml_value(&json!(2.5)), "2.5");
        assert_eq!(seconds(120.0), json!(120));
        assert_eq!(seconds(2.5), json!(2.5));
        let text = "k = \"a\\\"b\\\\c\\nd\\te\\u0001\"\n";
        assert_eq!(toml::parse(text).unwrap()["k"], "a\"b\\c\nd\te\u{1}");
    }

    #[test]
    fn format_names_parse() {
        for name in FORMAT_NAMES {
            name.parse::<Format>().unwrap();
        }
        assert!("yaml".parse::<Format>().unwrap_err().contains("mcpservers"));
    }
}
