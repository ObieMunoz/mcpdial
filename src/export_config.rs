//! Write saved servers back out in the shape a host reads: the inverse of
//! [`crate::import_config`]. Three shapes are written, matching three of the four
//! that are read: the `mcpServers` object of Claude Code, Claude Desktop, Cursor
//! and Windsurf; VS Code's `servers`, which spells the transport out under
//! `type`; and Codex's `[mcp_servers.NAME]` tables.
//!
//! Nothing is written to a file. The document goes to stdout and the user
//! redirects it, so what lands in a host's config stays theirs to review, and
//! `--merge` prints what the merged file would be rather than being it.
//!
//! No credential is ever part of the output. A saved OAuth token stays in
//! mcpdial's own store and the host is told to log in for itself; a `token_env`
//! travels as the `${VAR}` placeholder it already is, never as its value.

use crate::config::{Credential, ServerConfig, Store};
use crate::import_config::to_host_entry;
use crate::protocol::{Error, Result};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The host shapes `--format` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `{"mcpServers": {...}}`: Claude Code, Claude Desktop, Cursor, Windsurf.
    McpServers,
    /// `{"servers": {...}}`, each entry naming its `type`.
    VsCode,
    /// Codex's `config.toml`: one `[mcp_servers.NAME]` table per server.
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
    /// The key the servers live under in that host's file.
    pub fn key(self) -> &'static str {
        match self {
            Format::McpServers => "mcpServers",
            Format::VsCode => "servers",
            Format::Codex => "mcp_servers",
        }
    }
}

/// A finished export: the document for stdout, and what the host is not being
/// given, one line each for stderr.
#[derive(Debug, Clone, PartialEq)]
pub struct Export {
    pub document: String,
    pub notes: Vec<String>,
}

/// Every saved server, or the named ones, in `format`. `merge` is an existing
/// host file whose other contents are carried through untouched.
pub fn export(
    store: &Store,
    names: &[String],
    format: Format,
    merge: Option<&Path>,
) -> Result<Export> {
    let saved = store.servers()?;
    let chosen = choose(&saved, names)?;
    let credentials = store.credentials()?;
    let notes = chosen
        .iter()
        .flat_map(|(name, config)| notes_for(name, config, format, &credentials))
        .collect();
    let document = match format {
        Format::Codex => codex_document(&chosen, merge)?,
        _ => json_document(&chosen, format, merge)?,
    };
    Ok(Export { document, notes })
}

/// The servers to export, in the order they were named; all of them, by name,
/// when none was. A name that is not saved is a usage error, so a typo in a
/// list of ten does not silently export nine.
fn choose<'a>(
    saved: &'a BTreeMap<String, ServerConfig>,
    names: &[String],
) -> Result<Vec<(&'a str, &'a ServerConfig)>> {
    if names.is_empty() {
        return Ok(saved.iter().map(|(n, c)| (n.as_str(), c)).collect());
    }
    names
        .iter()
        .map(|name| {
            saved
                .get_key_value(name)
                .map(|(n, c)| (n.as_str(), c))
                .ok_or_else(|| Error::usage(format!("no server named {name:?}")))
        })
        .collect()
}

fn notes_for(
    name: &str,
    config: &ServerConfig,
    format: Format,
    credentials: &BTreeMap<String, Credential>,
) -> Vec<String> {
    let mut notes = Vec::new();
    match &config.token_env {
        // VS Code expands `${env:VAR}` and `${input:ID}`, never a bare `${VAR}`.
        Some(var) if format == Format::VsCode => notes.push(format!(
            "{name}: Authorization carries ${{{var}}}, which VS Code does not expand; \
             write it as ${{env:{var}}} or as an inputs entry"
        )),
        Some(_) => {}
        None => {
            if credentials.get(name).is_some_and(Credential::has_token) {
                notes.push(format!(
                    "{name}: exported without its saved token; the host will need its own login"
                ));
            }
        }
    }
    if !config.allow.is_empty() || !config.deny.is_empty() {
        notes.push(format!(
            "{name}: its allow and deny lists are not exported; the host will offer every tool"
        ));
    }
    notes
}

fn json_document(
    chosen: &[(&str, &ServerConfig)],
    format: Format,
    merge: Option<&Path>,
) -> Result<String> {
    let key = format.key();
    let mut doc = match merge {
        Some(path) => read_json_object(path)?,
        None => Map::new(),
    };
    let mut table = match doc.remove(key) {
        Some(Value::Object(existing)) => existing,
        None => Map::new(),
        Some(_) => {
            let file = merge.map_or_else(String::new, |p| format!("{}: ", p.display()));
            return Err(Error::config(format!("{file}{key} is not an object")));
        }
    };
    for (name, config) in chosen {
        table.insert((*name).to_string(), json_entry(config, format));
    }
    doc.insert(key.to_string(), Value::Object(table));
    let text = serde_json::to_string_pretty(&Value::Object(doc))
        .map_err(|e| Error::config(e.to_string()))?;
    Ok(format!("{text}\n"))
}

/// One entry in a JSON host's shape. VS Code names the transport on every
/// entry; the others take a `command` to mean stdio, as `import` does.
fn json_entry(config: &ServerConfig, format: Format) -> Value {
    let mut entry = to_host_entry(config);
    let Some(fields) = entry.as_object_mut() else {
        return entry;
    };
    if format == Format::VsCode && !fields.contains_key("type") {
        fields.insert("type".into(), Value::from("stdio"));
    }
    if let Some(var) = &config.token_env {
        let headers = fields
            .entry("headers")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(headers) = headers.as_object_mut() {
            headers
                .entry("Authorization")
                .or_insert_with(|| Value::from(format!("Bearer ${{{var}}}")));
        }
    }
    entry
}

fn codex_document(chosen: &[(&str, &ServerConfig)], merge: Option<&Path>) -> Result<String> {
    let tables: String = chosen
        .iter()
        .map(|(name, config)| codex_table(name, config))
        .collect();
    let body = match merge {
        None => tables,
        Some(path) => {
            let exported: BTreeSet<&str> = chosen.iter().map(|(name, _)| *name).collect();
            let kept = codex_without(&read_text(path)?, &exported, path)?;
            match kept.trim_end() {
                "" => tables,
                kept => format!("{kept}\n\n{tables}"),
            }
        }
    };
    let body = body.trim_end();
    Ok(match body.is_empty() {
        true => String::new(),
        false => format!("{body}\n"),
    })
}

/// One `[mcp_servers.NAME]` table, and a sub-table for `env` or `http_headers`
/// when there is one, in the order a person reads them.
fn codex_table(name: &str, config: &ServerConfig) -> String {
    let head = format!("mcp_servers.{}", toml_key(name));
    let entry = to_host_entry(config);
    let mut out = format!("[{head}]\n");
    for key in ["command", "args", "url", "cwd", "timeout"] {
        if let Some(value) = entry.get(key).and_then(toml_value) {
            out.push_str(&format!("{key} = {value}\n"));
        }
    }
    if let Some(var) = &config.token_env {
        out.push_str(&format!(
            "bearer_token_env_var = {}\n",
            toml_string(var.as_str())
        ));
    }
    out.push('\n');
    for (key, table) in [("env", "env"), ("headers", "http_headers")] {
        let Some(pairs) = entry.get(key).and_then(Value::as_object) else {
            continue;
        };
        out.push_str(&format!("[{head}.{table}]\n"));
        for (k, v) in pairs {
            if let Some(value) = toml_value(v) {
                out.push_str(&format!("{} = {value}\n", toml_key(k)));
            }
        }
        out.push('\n');
    }
    out
}

/// The file with every `[mcp_servers.NAME]` table of an exported name removed,
/// line for line otherwise. Two constructs would make that a guess rather than
/// a fact, and neither belongs in a server entry, so they are refused instead:
/// the same stand the TOML reader takes.
fn codex_without(text: &str, exported: &BTreeSet<&str>, path: &Path) -> Result<String> {
    let refuse = |what: &str| Error::config(format!("{}: {what} are not read", path.display()));
    if text.contains("\"\"\"") || text.contains("'''") {
        return Err(refuse("multi-line strings"));
    }
    if text.lines().any(|l| l.trim_start().starts_with("[[")) {
        return Err(refuse("arrays of tables"));
    }
    let mut out = String::with_capacity(text.len());
    let mut dropping = false;
    for line in text.split_inclusive('\n') {
        if let Some(header) = table_header(line) {
            dropping = matches!(header.as_slice(), [table, name, ..]
                if table == "mcp_servers" && exported.contains(name.as_str()));
        }
        if !dropping {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// The table path a `[a.b."c d"]` header names, or `None` when the line is not
/// a table header.
fn table_header(line: &str) -> Option<Vec<String>> {
    let mut rest = line.trim_start().strip_prefix('[')?;
    let mut path = Vec::new();
    loop {
        rest = rest.trim_start();
        let (segment, tail) = match rest.chars().next()? {
            quote @ ('"' | '\'') => quoted_segment(&rest[1..], quote)?,
            _ => {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(rest.len());
                (rest[..end].to_string(), &rest[end..])
            }
        };
        if segment.is_empty() {
            return None;
        }
        path.push(segment);
        rest = tail.trim_start();
        match rest.strip_prefix('.') {
            Some(next) => rest = next,
            None => return rest.strip_prefix(']').map(|_| path),
        }
    }
}

/// One quoted key and what follows its closing quote.
fn quoted_segment(rest: &str, quote: char) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Some((out, &rest[i + c.len_utf8()..]));
        }
        if c == '\\' && quote == '"' {
            let (_, escaped) = chars.next()?;
            out.push(match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            continue;
        }
        out.push(c);
    }
    None
}

/// A key as TOML spells it: bare where that is legal, quoted otherwise.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if bare {
        key.to_string()
    } else {
        toml_string(key)
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
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One JSON value as TOML, or `None` for a shape no server entry has.
fn toml_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(toml_string(s)),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => {
            let written: Option<Vec<String>> = items.iter().map(toml_value).collect();
            Some(format!("[{}]", written?.join(", ")))
        }
        _ => None,
    }
}

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| Error::config(format!("{}: {e}", path.display())))
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    let text = read_text(path)?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&text) {
        Ok(Value::Object(doc)) => Ok(doc),
        Ok(_) => Err(Error::config(format!(
            "{}: not a JSON object",
            path.display()
        ))),
        Err(e) => Err(Error::config(format!("{}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_config::extract;
    use serde_json::json;

    fn config(entry: Value) -> ServerConfig {
        let doc = json!({ "mcpServers": { "x": entry } });
        extract(&doc).remove(0).config
    }

    fn credentials(names: &[&str]) -> BTreeMap<String, Credential> {
        names
            .iter()
            .map(|n| {
                let cred = Credential {
                    access_token: Some("secret-token".into()),
                    ..Credential::default()
                };
                ((*n).to_string(), cred)
            })
            .collect()
    }

    #[test]
    fn every_format_writes_the_shape_its_host_reads() {
        let stdio = config(
            json!({"command": "npx", "args": ["-y", "fs", "/tmp/my dir"],
                                  "env": {"DEBUG": "1"}, "cwd": "/p", "timeout": 120}),
        );
        let http = config(json!({"type": "http", "url": "https://x/mcp", "headers": {"X-A": "1"}}));
        let chosen = [("fs", &stdio), ("wiki", &http)];

        let doc: Value =
            serde_json::from_str(&json_document(&chosen, Format::McpServers, None).unwrap())
                .unwrap();
        assert_eq!(
            doc,
            json!({"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "fs", "/tmp/my dir"],
                       "env": {"DEBUG": "1"}, "cwd": "/p", "timeout": 120},
                "wiki": {"type": "http", "url": "https://x/mcp", "headers": {"X-A": "1"}}}})
        );

        let doc: Value =
            serde_json::from_str(&json_document(&chosen, Format::VsCode, None).unwrap()).unwrap();
        assert_eq!(doc["servers"]["fs"]["type"], "stdio");
        assert_eq!(doc["servers"]["wiki"]["type"], "http");
        assert_eq!(doc["servers"]["fs"]["args"][2], "/tmp/my dir");

        let toml = codex_document(&chosen, None).unwrap();
        assert_eq!(
            toml,
            r#"[mcp_servers.fs]
command = "npx"
args = ["-y", "fs", "/tmp/my dir"]
cwd = "/p"
timeout = 120

[mcp_servers.fs.env]
DEBUG = "1"

[mcp_servers.wiki]
url = "https://x/mcp"

[mcp_servers.wiki.http_headers]
X-A = "1"
"#
        );
        let back = crate::import_config::read(Path::new("config.toml"), &toml).unwrap();
        assert_eq!(
            back.iter()
                .map(|f| (f.name.as_str(), &f.config))
                .collect::<Vec<_>>(),
            [("fs", &stdio), ("wiki", &http)],
            "the tables read back as the servers they were written from"
        );
    }

    #[test]
    fn a_token_env_is_a_placeholder_and_a_saved_token_is_a_note() {
        let mut http = config(json!({"type": "http", "url": "https://x/mcp"}));
        http.token_env = Some("API_TOKEN".into());
        let chosen = [("wiki", &http)];

        let doc: Value =
            serde_json::from_str(&json_document(&chosen, Format::McpServers, None).unwrap())
                .unwrap();
        assert_eq!(
            doc["mcpServers"]["wiki"]["headers"]["Authorization"],
            "Bearer ${API_TOKEN}"
        );
        assert!(codex_document(&chosen, None)
            .unwrap()
            .contains("bearer_token_env_var = \"API_TOKEN\""));

        let none = BTreeMap::new();
        assert!(notes_for("wiki", &http, Format::McpServers, &none).is_empty());
        assert!(notes_for("wiki", &http, Format::Codex, &none).is_empty());
        let vscode = notes_for("wiki", &http, Format::VsCode, &none);
        assert!(
            vscode[0].contains("${API_TOKEN}") && vscode[0].contains("${env:API_TOKEN}"),
            "{vscode:?}"
        );

        // A saved OAuth token is never written, whatever the format.
        let plain = config(json!({"type": "http", "url": "https://x/mcp"}));
        let saved = credentials(&["wiki"]);
        assert_eq!(
            notes_for("wiki", &plain, Format::McpServers, &saved),
            ["wiki: exported without its saved token; the host will need its own login"]
        );
        let chosen = [("wiki", &plain)];
        for format in [Format::McpServers, Format::VsCode] {
            let document = json_document(&chosen, format, None).unwrap();
            assert!(!document.contains("secret-token"), "{document}");
        }
        assert!(!codex_document(&chosen, None)
            .unwrap()
            .contains("secret-token"));

        // A hidden tool stays hidden here, so the loss is named rather than silent.
        let mut restricted = plain.clone();
        restricted.deny = vec!["delete_*".into()];
        let notes = notes_for("wiki", &restricted, Format::McpServers, &BTreeMap::new());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("allow and deny lists are not exported"));
    }

    #[test]
    fn merge_keeps_everything_it_did_not_write() {
        let dir = std::env::temp_dir().join(format!(
            "mcpdial-export-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("mcp.json");
        std::fs::write(
            &json_path,
            json!({"inputs": [{"id": "tok"}], "mcpServers": {"old": {"command": "old"},
                   "fs": {"command": "stale"}}})
            .to_string(),
        )
        .unwrap();
        let fs = config(json!({"command": "npx", "args": ["fs"]}));
        let merged: Value = serde_json::from_str(
            &json_document(&[("fs", &fs)], Format::McpServers, Some(&json_path)).unwrap(),
        )
        .unwrap();
        assert_eq!(merged["inputs"][0]["id"], "tok");
        assert_eq!(merged["mcpServers"]["old"]["command"], "old");
        assert_eq!(merged["mcpServers"]["fs"]["args"], json!(["fs"]));

        let toml_path = dir.join("config.toml");
        std::fs::write(
            &toml_path,
            "model = \"o3\"\n\n# keep me\n[profiles.fast]\nmodel = \"o4-mini\"\n\n\
             [mcp_servers.fs]\ncommand = \"stale\"\n\n[mcp_servers.fs.env]\nA = \"1\"\n\n\
             [mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        let merged = codex_document(&[("fs", &fs)], Some(&toml_path)).unwrap();
        assert!(merged.starts_with("model = \"o3\"\n\n# keep me\n[profiles.fast]\n"));
        assert!(merged.contains("[mcp_servers.other]\ncommand = \"other\""));
        assert!(!merged.contains("stale") && !merged.contains("A = \"1\""));
        assert!(merged.ends_with("[mcp_servers.fs]\ncommand = \"npx\"\nargs = [\"fs\"]\n"));

        std::fs::write(&toml_path, "x = \"\"\"a\nb\"\"\"\n").unwrap();
        let e = codex_document(&[("fs", &fs)], Some(&toml_path)).unwrap_err();
        assert!(e.to_string().contains("multi-line strings"), "{e}");

        std::fs::write(&json_path, "[1, 2]").unwrap();
        let e = json_document(&[("fs", &fs)], Format::McpServers, Some(&json_path)).unwrap_err();
        assert!(e.to_string().contains("not a JSON object"), "{e}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn table_headers_are_read_the_way_the_reader_writes_them() {
        assert_eq!(
            table_header("[mcp_servers.fs.env]").unwrap(),
            ["mcp_servers", "fs", "env"]
        );
        assert_eq!(
            table_header("  [ mcp_servers . \"web api\" ]  # why").unwrap(),
            ["mcp_servers", "web api"]
        );
        assert_eq!(table_header("[a]").unwrap(), ["a"]);
        assert_eq!(table_header("command = \"[x]\""), None);
        assert_eq!(table_header("[unterminated"), None);
        assert_eq!(table_header("[]"), None);
        assert_eq!(toml_key("web api"), "\"web api\"");
        assert_eq!(toml_key("X-Tenant"), "X-Tenant");
        assert_eq!(toml_string("a\"b\\c\td"), "\"a\\\"b\\\\c\\td\"");
    }

    #[test]
    fn a_name_that_is_not_saved_is_a_usage_error() {
        let mut saved = BTreeMap::new();
        saved.insert("fs".to_string(), ServerConfig::stdio("npx fs"));
        assert_eq!(choose(&saved, &[]).unwrap().len(), 1);
        assert_eq!(choose(&saved, &["fs".into()]).unwrap()[0].0, "fs");
        let e = choose(&saved, &["fs".into(), "nope".into()]).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
        assert!(e.to_string().contains("no server named \"nope\""), "{e}");
    }

    #[test]
    fn unknown_formats_name_the_known_ones() {
        assert_eq!("codex".parse::<Format>().unwrap(), Format::Codex);
        let e = "emacs".parse::<Format>().unwrap_err();
        assert!(
            e.contains("unknown format emacs") && e.contains("mcpservers"),
            "{e}"
        );
    }
}
