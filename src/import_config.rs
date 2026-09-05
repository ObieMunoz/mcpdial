//! Read the server lists other hosts keep, so a server configured once anywhere can
//! be dialed from here. Four shapes are known:
//!
//! ```json
//! { "mcpServers": {
//!     "chrome":  { "command": "npx", "args": ["chrome-devtools-mcp@latest"], "env": {} },
//!     "remote":  { "type": "http", "url": "https://example.com/mcp", "headers": {} } } }
//! ```
//!
//! is what Claude Code, Claude Desktop, Cursor and Windsurf write; Claude Code's
//! `~/.claude.json` nests the same object under `projects.<path>`, and those are
//! collected too. VS Code's `mcp.json` calls the object `servers` and adds an `inputs`
//! array whose entries are referenced as `${input:ID}`; OpenCode's `opencode.json`
//! calls it `mcp`, with `command` as an array and `environment` for `env`; Codex's
//! `config.toml` keeps one `[mcp_servers.NAME]` table per server. Nothing else in any
//! of those files is read.
//!
//! A numeric `timeout` on an entry is seconds, as mcpc, Cline and Roo Code write it;
//! no host puts milliseconds in this field (Claude Code's `MCP_TIMEOUT` is an
//! environment variable, not part of the entry).

use crate::config::ServerConfig;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One server found in a config document and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub config: ServerConfig,
    /// "mcpServers", "servers", "mcp", "mcp_servers" or "projects./some/path"
    pub scope: String,
    /// Things the user should know: an unsupported transport, a dropped field, a
    /// variable to set before the server is dialed.
    pub notes: Vec<String>,
}

/// The hosts whose files are scanned, as `--from` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Claude,
    Cursor,
    Windsurf,
    VsCode,
    Codex,
    OpenCode,
}

pub const HOST_NAMES: [&str; 6] = [
    "vscode", "codex", "opencode", "claude", "cursor", "windsurf",
];

impl std::str::FromStr for Host {
    type Err = String;
    fn from_str(name: &str) -> Result<Self, String> {
        match name {
            "claude" => Ok(Host::Claude),
            "cursor" => Ok(Host::Cursor),
            "windsurf" => Ok(Host::Windsurf),
            "vscode" => Ok(Host::VsCode),
            "codex" => Ok(Host::Codex),
            "opencode" => Ok(Host::OpenCode),
            _ => Err(format!(
                "unknown host {name}; one of {}",
                HOST_NAMES.join(", ")
            )),
        }
    }
}

/// Where the hosts keep their config, most specific first; `host` keeps one host's.
pub fn candidates(host: Option<Host>) -> Vec<PathBuf> {
    locations()
        .into_iter()
        .filter(|(h, _)| host.is_none_or(|wanted| wanted == *h))
        .map(|(_, p)| p)
        .collect()
}

fn locations() -> Vec<(Host, PathBuf)> {
    let mut out = vec![
        (Host::Claude, PathBuf::from(".mcp.json")),
        (Host::VsCode, PathBuf::from(".vscode/mcp.json")),
    ];
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if let Some(home) = &home {
        out.push((Host::Claude, home.join(".claude.json")));
        out.push((Host::Cursor, home.join(".cursor/mcp.json")));
        out.push((
            Host::Windsurf,
            home.join(".codeium/windsurf/mcp_config.json"),
        ));
        out.push((Host::Codex, home.join(".codex/config.toml")));
        out.push((Host::OpenCode, home.join(".config/opencode/opencode.json")));
    }
    if cfg!(target_os = "windows") {
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            out.push((
                Host::Claude,
                appdata.join("Claude/claude_desktop_config.json"),
            ));
            out.push((Host::VsCode, appdata.join("Code/User/mcp.json")));
        }
    } else if let Some(home) = &home {
        let (claude, code) = if cfg!(target_os = "macos") {
            (
                "Library/Application Support/Claude",
                "Library/Application Support/Code",
            )
        } else {
            (".config/Claude", ".config/Code")
        };
        out.push((
            Host::Claude,
            home.join(claude).join("claude_desktop_config.json"),
        ));
        out.push((Host::VsCode, home.join(code).join("User/mcp.json")));
    }
    out
}

/// Every server in one host config file, whatever its shape: Codex's TOML by the
/// `.toml` extension, JSON otherwise. The error is the parse failure alone; the
/// caller names the file.
pub fn read(path: &Path, text: &str) -> Result<Vec<Found>, String> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
    {
        let doc = toml::parse(text)?;
        Ok(table(&doc, "mcp_servers").collect())
    } else {
        let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok(extract_in(&doc, Some(path)))
    }
}

pub fn extract(doc: &Value) -> Vec<Found> {
    extract_in(doc, None)
}

/// `path` is where `doc` was read from, so a VS Code `envFile` can be found next to it.
pub fn extract_in(doc: &Value, path: Option<&Path>) -> Vec<Found> {
    let mut out: Vec<Found> = table(doc, "mcpServers").collect();
    if let Some(projects) = doc.get("projects").and_then(Value::as_object) {
        for (project_path, project) in projects {
            out.extend(table(project, "mcpServers").map(|mut f| {
                f.scope = format!("projects.{project_path}");
                f
            }));
        }
    }
    let inputs = Inputs::declared(doc.get("inputs"));
    if let Some(servers) = doc.get("servers").and_then(Value::as_object) {
        for (name, entry) in servers {
            if let Some(mut f) = convert(name, entry, "servers") {
                rename_inputs(&mut f, &inputs);
                if let Some(file) = entry.get("envFile").and_then(Value::as_str) {
                    read_env_file(&mut f, file, path);
                }
                out.push(f);
            }
        }
    }
    out.extend(table(doc, "mcp"));
    out
}

fn table<'a>(doc: &'a Value, key: &'a str) -> impl Iterator<Item = Found> + 'a {
    doc.get(key)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(move |(name, entry)| convert(name, entry, key))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// One entry of any of the shapes. They differ only in spelling: `command` is a
/// string or, in OpenCode, the whole argv; `env` is `environment` there; Codex
/// says `http_headers` and names the token's variable in `bearer_token_env_var`.
fn convert(name: &str, entry: &Value, scope: &str) -> Option<Found> {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let mut notes = Vec::new();
    let as_string = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);

    let mut config = if let Some(url) = entry.get("url").and_then(Value::as_str) {
        if kind == "sse" {
            notes.push("configured as SSE; mcpdial speaks Streamable HTTP, which most servers also serve at the same URL".into());
        }
        let mut c = ServerConfig::http(url);
        c.headers = string_map(entry.get("headers").or_else(|| entry.get("http_headers")));
        c.token_env = as_string("bearer_token_env_var");
        c
    } else {
        let mut argv = match entry.get("command")? {
            Value::String(command) => vec![command.clone()],
            Value::Array(_) => strings(entry.get("command")),
            _ => return None,
        };
        if argv.is_empty() {
            return None;
        }
        argv.extend(strings(entry.get("args")));
        let mut c = ServerConfig::stdio(shell_join(&argv));
        c.env = string_map(entry.get("env").or_else(|| entry.get("environment")));
        c.cwd = as_string("cwd");
        c
    };
    config.timeout = entry.get("timeout").and_then(Value::as_f64);

    Some(Found {
        name: name.to_string(),
        config,
        scope: scope.to_string(),
        notes,
    })
}

/// VS Code's `inputs` array, by id: what the host would have prompted for.
struct Inputs(BTreeMap<String, (String, bool)>);

impl Inputs {
    fn declared(v: Option<&Value>) -> Self {
        Inputs(
            v.and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|i| {
                    let id = i.get("id")?.as_str()?;
                    let description = i
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let secret = i.get("password").and_then(Value::as_bool).unwrap_or(false);
                    Some((id.to_string(), (description, secret)))
                })
                .collect(),
        )
    }

    fn describe(&self, id: &str) -> String {
        let mut s = format!("{} (input {id}", input_var(id));
        match self.0.get(id) {
            Some((description, secret)) => {
                if *secret {
                    s.push_str(", secret");
                }
                if !description.is_empty() {
                    s.push_str(": ");
                    s.push_str(description);
                }
            }
            None => s.push_str(", not declared under inputs"),
        }
        s.push(')');
        s
    }
}

/// The environment variable a VS Code `${input:ID}` becomes: nothing is prompted
/// for and no secret is written, so the user sets it before dialing.
pub fn input_var(id: &str) -> String {
    let body: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("MCPDIAL_INPUT_{body}")
}

fn rename_inputs(f: &mut Found, inputs: &Inputs) {
    let mut used: Vec<String> = Vec::new();
    let c = &mut f.config;
    for s in c
        .http
        .iter_mut()
        .chain(c.stdio.iter_mut())
        .chain(c.cwd.iter_mut())
        .chain(c.headers.values_mut())
        .chain(c.env.values_mut())
    {
        *s = rewrite_inputs(s, &mut used);
    }
    if !used.is_empty() {
        let list: Vec<String> = used.iter().map(|id| inputs.describe(id)).collect();
        f.notes
            .push(format!("set before dialing: {}", list.join(", ")));
    }
}

/// `${input:ID}` becomes `${MCPDIAL_INPUT_ID}`; `used` collects each id once, in
/// order of first appearance.
fn rewrite_inputs(s: &str, used: &mut Vec<String>) -> String {
    const OPEN: &str = "${input:";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let id = &after[..end];
        out.push_str(&format!("${{{}}}", input_var(id)));
        if !used.iter().any(|u| u == id) {
            used.push(id.to_string());
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A VS Code `envFile` is read only to learn its keys: each becomes a `${KEY}`
/// placeholder in `env` (an explicit `env` entry wins), and the note lists them.
/// The values stay in the file.
fn read_env_file(f: &mut Found, raw: &str, config_path: Option<&Path>) {
    let path = env_file_path(raw, config_path);
    let shown = path.display();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let keys = env_file_keys(&text);
            for key in &keys {
                f.config
                    .env
                    .entry(key.clone())
                    .or_insert_with(|| format!("${{{key}}}"));
            }
            if keys.is_empty() {
                f.notes.push(format!("envFile {shown} names no variables"));
            } else {
                f.notes.push(format!(
                    "envFile {shown}: set {} before dialing; its values were not copied",
                    keys.join(", ")
                ));
            }
        }
        Err(e) => f.notes.push(format!(
            "envFile {shown} could not be read ({e}); its variables were not imported"
        )),
    }
}

/// `${workspaceFolder}` and a relative path both mean the directory holding
/// `.vscode`; a user-level `mcp.json` has no workspace, so its own directory serves.
fn env_file_path(raw: &str, config_path: Option<&Path>) -> PathBuf {
    let workspace = config_path.and_then(Path::parent).map(|dir| {
        let dir = match dir.file_name() {
            Some(n) if n == ".vscode" => dir.parent().unwrap_or(dir),
            _ => dir,
        };
        if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir.to_path_buf()
        }
    });
    let raw = match &workspace {
        Some(w) => raw.replace("${workspaceFolder}", &w.to_string_lossy()),
        None => raw.to_string(),
    };
    let path = PathBuf::from(raw);
    match workspace {
        Some(w) if path.is_relative() => w.join(path),
        _ => path,
    }
}

fn env_file_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let key = line.split_once('=')?.0.trim();
            (!key.is_empty()).then(|| key.to_string())
        })
        .collect()
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

/// The slice of TOML that Codex writes to `config.toml`, read into the same tree
/// `serde_json` would give for the equivalent JSON: `[table.sub]` headers, dotted
/// keys, basic and literal strings, arrays, inline tables, booleans, numbers and
/// `#` comments. Multi-line strings, arrays of tables and dates are refused with
/// a message rather than misread; a server entry has no use for them.
mod toml {
    use serde_json::{Map, Value};

    pub fn parse(text: &str) -> Result<Value, String> {
        let mut p = Parser { s: text, i: 0 };
        let mut root = Map::new();
        let mut table: Vec<String> = Vec::new();
        loop {
            p.skip_blank();
            if p.done() {
                break;
            }
            if p.eat('[') {
                if p.eat('[') {
                    return Err(p.err("arrays of tables are not read"));
                }
                table = p.key()?;
                p.skip_ws();
                if !p.eat(']') {
                    return Err(p.err("expected ]"));
                }
                ensure_table(&mut root, &table).map_err(|m| p.err(&m))?;
            } else {
                let mut path = table.clone();
                path.extend(p.key()?);
                p.skip_ws();
                if !p.eat('=') {
                    return Err(p.err("expected ="));
                }
                p.skip_ws();
                let value = p.value()?;
                insert(&mut root, &path, value).map_err(|m| p.err(&m))?;
            }
            p.skip_ws();
            p.skip_comment();
            if !p.done() && !p.eat_newline() {
                return Err(p.err("expected the end of the line"));
            }
        }
        Ok(Value::Object(root))
    }

    fn ensure_table<'a>(
        root: &'a mut Map<String, Value>,
        path: &[String],
    ) -> Result<&'a mut Map<String, Value>, String> {
        let mut cur = root;
        for seg in path {
            cur = cur
                .entry(seg.clone())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| format!("{seg} is not a table"))?;
        }
        Ok(cur)
    }

    fn insert(root: &mut Map<String, Value>, path: &[String], value: Value) -> Result<(), String> {
        let (last, parents) = path.split_last().ok_or("expected a key")?;
        let parent = ensure_table(root, parents)?;
        if parent.contains_key(last) {
            return Err(format!("{last} is defined twice"));
        }
        parent.insert(last.clone(), value);
        Ok(())
    }

    struct Parser<'a> {
        s: &'a str,
        i: usize,
    }

    impl Parser<'_> {
        fn done(&self) -> bool {
            self.i >= self.s.len()
        }

        fn peek(&self) -> Option<char> {
            self.s[self.i..].chars().next()
        }

        fn eat(&mut self, c: char) -> bool {
            if self.peek() == Some(c) {
                self.i += c.len_utf8();
                true
            } else {
                false
            }
        }

        fn eat_newline(&mut self) -> bool {
            if self.s[self.i..].starts_with("\r\n") {
                self.i += 2;
                true
            } else {
                self.eat('\n')
            }
        }

        fn err(&self, what: &str) -> String {
            let line = self.s[..self.i.min(self.s.len())].matches('\n').count() + 1;
            format!("line {line}: {what}")
        }

        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(' ' | '\t')) {
                self.i += 1;
            }
        }

        fn skip_comment(&mut self) {
            if self.peek() == Some('#') {
                while !self.done() && !matches!(self.peek(), Some('\n' | '\r')) {
                    self.i += self.peek().map_or(1, char::len_utf8);
                }
            }
        }

        /// Whitespace, newlines and comments: what may sit between statements and
        /// between array elements.
        fn skip_blank(&mut self) {
            loop {
                self.skip_ws();
                self.skip_comment();
                if !self.eat_newline() {
                    return;
                }
            }
        }

        fn key(&mut self) -> Result<Vec<String>, String> {
            let mut path = Vec::new();
            loop {
                self.skip_ws();
                let seg = match self.peek() {
                    Some('"') => self.basic_string()?,
                    Some('\'') => self.literal_string()?,
                    _ => {
                        let start = self.i;
                        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-')
                        {
                            self.i += 1;
                        }
                        if start == self.i {
                            return Err(self.err("expected a key"));
                        }
                        self.s[start..self.i].to_string()
                    }
                };
                path.push(seg);
                self.skip_ws();
                if !self.eat('.') {
                    return Ok(path);
                }
            }
        }

        fn value(&mut self) -> Result<Value, String> {
            match self.peek() {
                Some('"') if self.s[self.i..].starts_with("\"\"\"") => {
                    Err(self.err("multi-line strings are not read"))
                }
                Some('\'') if self.s[self.i..].starts_with("'''") => {
                    Err(self.err("multi-line strings are not read"))
                }
                Some('"') => self.basic_string().map(Value::String),
                Some('\'') => self.literal_string().map(Value::String),
                Some('[') => self.array(),
                Some('{') => self.inline_table(),
                Some(_) => self.bare(),
                None => Err(self.err("expected a value")),
            }
        }

        fn basic_string(&mut self) -> Result<String, String> {
            self.eat('"');
            let mut out = String::new();
            loop {
                let Some(c) = self.peek() else {
                    return Err(self.err("unterminated string"));
                };
                self.i += c.len_utf8();
                match c {
                    '"' => return Ok(out),
                    '\n' | '\r' => return Err(self.err("unterminated string")),
                    '\\' => {
                        let Some(e) = self.peek() else {
                            return Err(self.err("unterminated string"));
                        };
                        self.i += e.len_utf8();
                        out.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            'b' => '\u{8}',
                            'f' => '\u{c}',
                            '"' => '"',
                            '\\' => '\\',
                            'u' => self.unicode_escape(4)?,
                            'U' => self.unicode_escape(8)?,
                            _ => return Err(self.err(&format!("unknown escape \\{e}"))),
                        });
                    }
                    _ => out.push(c),
                }
            }
        }

        fn unicode_escape(&mut self, digits: usize) -> Result<char, String> {
            let hex = self
                .s
                .get(self.i..self.i + digits)
                .filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()))
                .ok_or_else(|| self.err("bad unicode escape"))?;
            self.i += digits;
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| self.err("bad unicode escape"))
        }

        fn literal_string(&mut self) -> Result<String, String> {
            self.eat('\'');
            let start = self.i;
            loop {
                match self.peek() {
                    Some('\'') => {
                        let s = self.s[start..self.i].to_string();
                        self.i += 1;
                        return Ok(s);
                    }
                    Some('\n' | '\r') | None => return Err(self.err("unterminated string")),
                    Some(c) => self.i += c.len_utf8(),
                }
            }
        }

        fn array(&mut self) -> Result<Value, String> {
            self.eat('[');
            let mut items = Vec::new();
            loop {
                self.skip_blank();
                if self.eat(']') {
                    return Ok(Value::Array(items));
                }
                items.push(self.value()?);
                self.skip_blank();
                if self.eat(',') {
                    continue;
                }
                self.skip_blank();
                if self.eat(']') {
                    return Ok(Value::Array(items));
                }
                return Err(self.err("expected , or ] in array"));
            }
        }

        fn inline_table(&mut self) -> Result<Value, String> {
            self.eat('{');
            let mut map = Map::new();
            self.skip_ws();
            if self.eat('}') {
                return Ok(Value::Object(map));
            }
            loop {
                let path = self.key()?;
                self.skip_ws();
                if !self.eat('=') {
                    return Err(self.err("expected = in inline table"));
                }
                self.skip_ws();
                let value = self.value()?;
                insert(&mut map, &path, value).map_err(|m| self.err(&m))?;
                self.skip_ws();
                if self.eat(',') {
                    self.skip_ws();
                    continue;
                }
                if self.eat('}') {
                    return Ok(Value::Object(map));
                }
                return Err(self.err("expected , or } in inline table"));
            }
        }

        /// A value written without quotes: a boolean or a number.
        fn bare(&mut self) -> Result<Value, String> {
            let start = self.i;
            while matches!(self.peek(), Some(c) if !c.is_whitespace() && !matches!(c, ',' | ']' | '}' | '#'))
            {
                self.i += self.peek().map_or(1, char::len_utf8);
            }
            let word = &self.s[start..self.i];
            match word {
                "true" => return Ok(Value::Bool(true)),
                "false" => return Ok(Value::Bool(false)),
                _ => {}
            }
            let digits = word.replace('_', "");
            if let Ok(n) = digits.parse::<i64>() {
                return Ok(Value::from(n));
            }
            if let Ok(x) = digits.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(x) {
                    return Ok(Value::Number(n));
                }
            }
            Err(self.err(&format!("cannot read the value {word}")))
        }
    }
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
        assert!(chrome.notes.is_empty());

        let legacy = &found[1];
        assert!(legacy.notes[0].contains("SSE"));

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
    fn vscode_servers_with_inputs_and_env_file() {
        let dir = std::env::temp_dir().join(format!(
            "mcpdial-vscode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".vscode")).unwrap();
        std::fs::write(
            dir.join(".env"),
            "# comment\nexport API_KEY=hunter2\nREGION = us\n\nDEBUG=1\n",
        )
        .unwrap();
        let config_path = dir.join(".vscode/mcp.json");

        let doc = json!({
            "inputs": [
                {"id": "github-token", "type": "promptString", "description": "GitHub PAT", "password": true},
                {"id": "org", "type": "promptString", "description": "Organization"}
            ],
            "servers": {
                "github": {
                    "type": "http",
                    "url": "https://api.example.com/${input:org}/mcp",
                    "headers": {"Authorization": "Bearer ${input:github-token}", "X-Org": "${input:org}"}
                },
                "local": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "server", "--token", "${input:github-token}"],
                    "env": {"TOKEN": "${input:github-token}", "DEBUG": "verbose", "UNKNOWN": "${input:nope}"},
                    "envFile": "${workspaceFolder}/.env"
                },
                "missing": {"command": "x", "envFile": "nowhere.env"}
            }
        });
        let found = extract_in(&doc, Some(&config_path));
        std::fs::remove_dir_all(&dir).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["github", "local", "missing"]);

        let github = &found[0];
        assert_eq!(github.scope, "servers");
        assert_eq!(
            github.config.http.as_deref(),
            Some("https://api.example.com/${MCPDIAL_INPUT_ORG}/mcp")
        );
        assert_eq!(
            github.config.headers["Authorization"],
            "Bearer ${MCPDIAL_INPUT_GITHUB_TOKEN}"
        );
        assert_eq!(github.config.headers["X-Org"], "${MCPDIAL_INPUT_ORG}");
        assert_eq!(github.notes.len(), 1, "{:?}", github.notes);
        let note = &github.notes[0];
        assert!(
            note.starts_with("set before dialing: MCPDIAL_INPUT_ORG (input org: Organization), MCPDIAL_INPUT_GITHUB_TOKEN (input github-token, secret: GitHub PAT)"),
            "{note}"
        );

        let local = &found[1];
        assert_eq!(
            local.config.stdio.as_deref(),
            Some("npx -y server --token ${MCPDIAL_INPUT_GITHUB_TOKEN}")
        );
        assert_eq!(local.config.env["TOKEN"], "${MCPDIAL_INPUT_GITHUB_TOKEN}");
        assert_eq!(local.config.env["UNKNOWN"], "${MCPDIAL_INPUT_NOPE}");
        assert_eq!(
            local.config.env["DEBUG"], "verbose",
            "an explicit env entry wins"
        );
        assert_eq!(local.config.env["API_KEY"], "${API_KEY}");
        assert_eq!(local.config.env["REGION"], "${REGION}");
        assert_eq!(local.notes.len(), 2, "{:?}", local.notes);
        assert!(
            local.notes[0].contains("MCPDIAL_INPUT_NOPE (input nope, not declared under inputs)"),
            "{}",
            local.notes[0]
        );
        assert!(
            local.notes[1].starts_with("envFile ")
                && local.notes[1].contains(": set API_KEY, REGION, DEBUG before dialing"),
            "{}",
            local.notes[1]
        );
        let all = format!("{found:?}");
        assert!(!all.contains("hunter2"), "values never leave the file");

        let missing = &found[2];
        assert!(missing.config.env.is_empty());
        assert!(
            missing.notes[0].contains("nowhere.env could not be read"),
            "{}",
            missing.notes[0]
        );
    }

    #[test]
    fn input_ids_become_variable_names() {
        assert_eq!(input_var("github-token"), "MCPDIAL_INPUT_GITHUB_TOKEN");
        assert_eq!(input_var("my.api key/2"), "MCPDIAL_INPUT_MY_API_KEY_2");
        let mut used = Vec::new();
        assert_eq!(
            rewrite_inputs("${input:a}-${input:b}-${input:a} ${input:open", &mut used),
            "${MCPDIAL_INPUT_A}-${MCPDIAL_INPUT_B}-${MCPDIAL_INPUT_A} ${input:open"
        );
        assert_eq!(used, ["a", "b"]);
    }

    #[test]
    fn env_file_is_found_next_to_the_workspace() {
        let cfg = Path::new("/w/.vscode/mcp.json");
        assert_eq!(
            env_file_path("${workspaceFolder}/.env", Some(cfg)),
            Path::new("/w/.env")
        );
        assert_eq!(env_file_path(".env", Some(cfg)), Path::new("/w/.env"));
        assert_eq!(
            env_file_path("/abs/.env", Some(cfg)),
            Path::new("/abs/.env")
        );
        let user = Path::new("/u/Code/User/mcp.json");
        assert_eq!(
            env_file_path("${workspaceFolder}/.env", Some(user)),
            Path::new("/u/Code/User/.env")
        );
        assert_eq!(
            env_file_path(".env", Some(Path::new(".vscode/mcp.json"))),
            Path::new("./.env")
        );
        assert_eq!(env_file_path(".env", None), Path::new(".env"));
    }

    #[test]
    fn codex_toml_with_both_transports() {
        let text = r#"
model = "o3" # not a server
[profiles.fast]
model = "o4-mini"

[mcp_servers.docs]
command = "npx"
args = ["-y", "docs-server", "--root", "/srv/my docs"]
cwd = "/srv"

[mcp_servers.docs.env]
LOG_LEVEL = "debug"

[mcp_servers."web api"]
url = "https://api.example.com/mcp"
bearer_token_env_var = "API_TOKEN"
http_headers = { X-Tenant = "acme", "X-Plan" = "pro" }
enabled = true
startup_timeout_sec = 30
"#;
        let found = read(Path::new("config.toml"), text).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["docs", "web api"]);

        let docs = &found[0];
        assert_eq!(docs.scope, "mcp_servers");
        assert_eq!(
            split_command(docs.config.stdio.as_deref().unwrap()).unwrap(),
            ["npx", "-y", "docs-server", "--root", "/srv/my docs"]
        );
        assert_eq!(docs.config.env["LOG_LEVEL"], "debug");
        assert_eq!(docs.config.cwd.as_deref(), Some("/srv"));

        let web = &found[1];
        assert_eq!(
            web.config.http.as_deref(),
            Some("https://api.example.com/mcp")
        );
        assert_eq!(web.config.token_env.as_deref(), Some("API_TOKEN"));
        assert_eq!(web.config.headers["X-Tenant"], "acme");
        assert_eq!(web.config.headers["X-Plan"], "pro");
        assert!(web.notes.is_empty());

        let e = read(
            Path::new("config.toml"),
            "[mcp_servers.x]\ncommand = \"\"\"a\"\"\"\n",
        )
        .unwrap_err();
        assert!(e.contains("line 2") && e.contains("multi-line"), "{e}");
        let e = read(Path::new("config.toml"), "[[mcp_servers]]\n").unwrap_err();
        assert!(e.contains("arrays of tables"), "{e}");
        let e = read(Path::new("config.toml"), "a = 1\na = 2\n").unwrap_err();
        assert!(e.contains("line 2") && e.contains("twice"), "{e}");
    }

    #[test]
    fn toml_subset_reads_what_codex_writes() {
        let text = "a.b = \"x\\ty\\u00e9\"\r\nc = 'lit\\eral'\nd = [\n  1, # one\n  -2.5,\n  true,\n]\ne = { f = \"g\", h.i = [] }\n[t]\n  j = false\n";
        let v = toml::parse(text).unwrap();
        assert_eq!(
            v,
            json!({
                "a": {"b": "x\ty\u{e9}"},
                "c": "lit\\eral",
                "d": [1, -2.5, true],
                "e": {"f": "g", "h": {"i": []}},
                "t": {"j": false}
            })
        );
        assert!(toml::parse("k = \"open\n").is_err());
        assert!(toml::parse("k = what\n").is_err());
        assert!(toml::parse("k = 1 extra\n").is_err());
    }

    #[test]
    fn opencode_local_and_remote() {
        let doc = json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {
                "local": {
                    "type": "local",
                    "command": ["bun", "x", "my-mcp", "--dir", "a b"],
                    "environment": {"MY_VAR": "v"},
                    "enabled": true
                },
                "remote": {
                    "type": "remote",
                    "url": "https://mcp.example.com/mcp",
                    "headers": {"Authorization": "Bearer ${TOKEN}"}
                },
                "junk": {"type": "local", "command": []}
            }
        });
        let found = extract(&doc);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["local", "remote"]);
        let local = &found[0];
        assert_eq!(local.scope, "mcp");
        assert_eq!(
            split_command(local.config.stdio.as_deref().unwrap()).unwrap(),
            ["bun", "x", "my-mcp", "--dir", "a b"]
        );
        assert_eq!(local.config.env["MY_VAR"], "v");
        let remote = &found[1];
        assert_eq!(
            remote.config.http.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(remote.config.headers["Authorization"], "Bearer ${TOKEN}");
    }

    #[test]
    fn candidates_cover_every_host_and_from_keeps_one() {
        for name in HOST_NAMES {
            let host: Host = name.parse().unwrap();
            assert!(!candidates(Some(host)).is_empty(), "{name}");
        }
        assert!("emacs".parse::<Host>().unwrap_err().contains("vscode"));
        let vscode = candidates(Some(Host::VsCode));
        assert_eq!(vscode[0], Path::new(".vscode/mcp.json"));
        assert!(vscode
            .iter()
            .all(|p| p.ends_with("mcp.json") && !p.ends_with(".cursor/mcp.json")));
        let all = candidates(None);
        assert_eq!(all[0], Path::new(".mcp.json"));
        assert!(all.iter().any(|p| p.ends_with(".codex/config.toml")));
        assert!(all
            .iter()
            .any(|p| p.ends_with(".config/opencode/opencode.json")));
        assert!(all.len() > vscode.len());
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
