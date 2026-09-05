//! The MCP registry (registry.modelcontextprotocol.io): search it, and turn one of
//! its entries into a [`ServerConfig`] without running anything.
//!
//! An entry lists `remotes` (a URL per transport) and `packages` (something to run
//! locally: an npm, PyPI or OCI identifier with the arguments and environment
//! variables it expects). [`Registry`] does the HTTP; [`convert`] is pure, so it can
//! be tested against fixture JSON. Everything in an entry is untrusted input: each
//! piece becomes one argv word, joined with the quoting `import` uses, so nothing in
//! it can grow into a second command.

use crate::config::ServerConfig;
use crate::import_config::shell_join;
use crate::protocol::{Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

pub const DEFAULT_URL: &str = "https://registry.modelcontextprotocol.io";
/// Points at a private registry that speaks the same API.
pub const ENV_URL: &str = "MCPDIAL_REGISTRY";
pub const SSE_NOTE: &str = "configured as SSE; mcpdial speaks Streamable HTTP, which most servers also serve at the same URL";

pub struct Registry {
    base: String,
    agent: ureq::Agent,
    user_agent: String,
}

impl Registry {
    pub fn from_env(timeout: Duration, user_agent: &str) -> Self {
        let base = std::env::var(ENV_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        Self::new(base, timeout, user_agent)
    }

    pub fn new(base: impl Into<String>, timeout: Duration, user_agent: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            agent: ureq::Agent::new_with_config(config),
            user_agent: user_agent.to_string(),
        }
    }

    /// The registry's own objects for the latest version of every server matching
    /// `query`, untouched: each holds the entry under `server` and its `_meta`.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<Value>> {
        let url = format!(
            "{}/v0.1/servers?search={}&limit={}&version=latest",
            self.base,
            encode(query),
            limit.clamp(1, 100)
        );
        let (status, body) = self.get(&url)?;
        if status != 200 {
            return Err(self.answered(status, &body));
        }
        let doc: Value = serde_json::from_str(&body)
            .map_err(|e| Error::transport(format!("the registry sent malformed JSON: {e}")))?;
        doc.get("servers")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| Error::transport("the registry's reply has no `servers` list"))
    }

    /// The entry for the latest version of the server called `name`, or `None`
    /// when the registry has no such server.
    pub fn latest(&self, name: &str) -> Result<Option<Value>> {
        let url = format!(
            "{}/v0.1/servers/{}/versions/latest",
            self.base,
            encode(name)
        );
        let (status, body) = self.get(&url)?;
        match status {
            200 => {
                let mut doc: Value = serde_json::from_str(&body).map_err(|e| {
                    Error::transport(format!("the registry sent malformed JSON: {e}"))
                })?;
                match doc.get_mut("server").map(Value::take) {
                    Some(server) if server.is_object() => Ok(Some(server)),
                    _ => Err(Error::transport(
                        "the registry's reply has no `server` object",
                    )),
                }
            }
            404 => Ok(None),
            _ => Err(self.answered(status, &body)),
        }
    }

    fn get(&self, url: &str) -> Result<(u16, String)> {
        let mut resp = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent)
            .call()
            .map_err(|e| match e {
                ureq::Error::Timeout(_) => Error::transport(format!(
                    "no reply from the registry at {} in time",
                    self.base
                )),
                other => Error::transport(format!(
                    "could not reach the registry at {}: {other}",
                    self.base
                )),
            })?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::transport(format!("could not read the registry's reply: {e}")))?;
        Ok((status, body))
    }

    /// A status other than the ones a lookup expects. The registry answers in
    /// problem+json, whose `detail` says what went wrong better than the body does.
    fn answered(&self, status: u16, body: &str) -> Error {
        let detail = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                ["detail", "title"]
                    .iter()
                    .find_map(|k| v[k].as_str().map(String::from))
            })
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        Error::transport(format!(
            "the registry at {} answered HTTP {status}: {detail}",
            self.base
        ))
    }
}

/// Percent-encode one URL component. A registry name holds a `/`, which the
/// registry wants as `%2F` in a path.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            write!(out, "%{b:02X}").unwrap();
        }
    }
    out
}

/// Which part of an entry to save.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A Streamable HTTP remote when there is one, else the first package mcpdial
    /// can run, else an SSE remote.
    Any,
    Remote,
    /// A package by registry type: `npm`, `pypi` or `oci`.
    Package(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Resolved {
    pub config: ServerConfig,
    /// What the user should know: variables to set, values the entry left to them,
    /// an SSE-only remote. Each may span several lines.
    pub notes: Vec<String>,
    /// Required values the entry leaves to the user that no `--arg` supplied, one
    /// description per entry. Non-empty means the config must not be saved.
    pub missing: Vec<String>,
}

/// The transports an entry offers, as the search table names them: `http`, `sse`,
/// `stdio (npm)`.
pub fn transports(server: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |label: String| {
        if !out.contains(&label) {
            out.push(label);
        }
    };
    for r in list(&server["remotes"]) {
        match r["type"].as_str() {
            Some("streamable-http") => add("http".into()),
            Some("sse") => add("sse".into()),
            _ => {}
        }
    }
    for p in list(&server["packages"]) {
        if let Some(kind) = p["registryType"].as_str() {
            add(format!("stdio ({kind})"));
        }
    }
    out
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// Build the config an entry describes. `given` holds the `--arg` values, in the
/// order the entry's user-supplied slots appear.
pub fn convert(server: &Value, pick: &Pick, given: &[String]) -> Result<Resolved> {
    let remotes = list(&server["remotes"]);
    let streamable = remotes.iter().find(|r| r["type"] == "streamable-http");
    let sse = remotes.iter().find(|r| r["type"] == "sse");
    let packages: Vec<&Value> = list(&server["packages"])
        .iter()
        .filter(|p| matches!(p["transport"]["type"].as_str(), None | Some("stdio")))
        .collect();
    let offered = || {
        let t = transports(server);
        if t.is_empty() {
            "nothing mcpdial can dial".to_string()
        } else {
            t.join(", ")
        }
    };

    let mut slots = Slots::new(given);
    let mut notes = Vec::new();
    let config = match pick {
        Pick::Remote => match streamable.or(sse) {
            Some(r) => remote(r, &mut slots, &mut notes)?,
            None => {
                return Err(Error::usage(format!(
                    "this entry has no remote endpoint; it offers {}",
                    offered()
                )))
            }
        },
        Pick::Package(kind) => match packages.iter().find(|p| p["registryType"] == *kind) {
            Some(p) => package(p, &mut slots, &mut notes)?,
            None => {
                return Err(Error::usage(format!(
                    "this entry has no {kind} package; it offers {}",
                    offered()
                )))
            }
        },
        Pick::Any => {
            let runnable = packages
                .iter()
                .find(|p| runtime(p["registryType"].as_str().unwrap_or("")).is_some());
            if let Some(r) = streamable {
                remote(r, &mut slots, &mut notes)?
            } else if let Some(p) = runnable {
                package(p, &mut slots, &mut notes)?
            } else if let Some(r) = sse {
                remote(r, &mut slots, &mut notes)?
            } else {
                return Err(Error::usage(format!(
                    "this entry offers {}; mcpdial dials http remotes and npm, pypi and oci packages",
                    offered()
                )));
            }
        }
    };

    let extra = slots.unused();
    if extra > 0 {
        return Err(Error::usage(format!(
            "--arg was given {} value(s) too many: this entry takes {}",
            extra,
            slots.seen.len()
        )));
    }
    if !slots.seen.is_empty() {
        notes.push(format!(
            "values this entry leaves to you, in --arg order:\n  {}",
            slots.seen.join("\n  ")
        ));
    }
    Ok(Resolved {
        config,
        notes,
        missing: slots.missing,
    })
}

/// The values an entry leaves to the user: an argument without a `value`, or a
/// `{variable}` inside one. `--arg` values fill them in the order they are met.
struct Slots<'a> {
    given: std::slice::Iter<'a, String>,
    /// Every slot met, described, in order.
    seen: Vec<String>,
    /// The required ones that got no value.
    missing: Vec<String>,
}

impl<'a> Slots<'a> {
    fn new(given: &'a [String]) -> Self {
        Self {
            given: given.iter(),
            seen: Vec::new(),
            missing: Vec::new(),
        }
    }

    /// The value for one slot: the next `--arg`, else the entry's default. `None`
    /// when it is optional and got neither.
    fn take(&mut self, input: &Value, label: &str) -> Result<Option<String>> {
        let required = input["isRequired"].as_bool().unwrap_or(false);
        let described = describe(input, label, required);
        self.seen.push(described.clone());
        let value = self
            .given
            .next()
            .cloned()
            .or_else(|| input["default"].as_str().map(String::from));
        let Some(value) = value else {
            if required {
                self.missing.push(described);
            }
            return Ok(None);
        };
        let choices = list(&input["choices"]);
        if !choices.is_empty() && !choices.iter().any(|c| c == &value) {
            return Err(Error::usage(format!(
                "{label} must be one of {}, not {value:?}",
                choices
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(Some(value))
    }

    /// `text` with each `{name}` from `variables` filled in, in the order they
    /// appear. `None` when an optional variable got no value: the whole argument is
    /// then left out rather than sent with a hole in it.
    fn fill(&mut self, text: &str, variables: &Value) -> Result<Option<String>> {
        let Some(vars) = variables.as_object() else {
            return Ok(Some(text.to_string()));
        };
        let mut used: Vec<(usize, &String, &Value)> = vars
            .iter()
            .filter_map(|(name, input)| {
                text.find(&format!("{{{name}}}"))
                    .map(|at| (at, name, input))
            })
            .collect();
        used.sort_by_key(|(at, ..)| *at);
        let mut out = text.to_string();
        for (_, name, input) in used {
            match self.take(input, name)? {
                Some(v) => out = out.replace(&format!("{{{name}}}"), &v),
                None => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    fn unused(&self) -> usize {
        self.given.len()
    }
}

/// One line for a slot: `label (required): description, e.g. placeholder`.
fn describe(input: &Value, label: &str, required: bool) -> String {
    let mut s = format!(
        "{label} ({})",
        if required { "required" } else { "optional" }
    );
    if let Some(d) = input["description"]
        .as_str()
        .filter(|d| !d.trim().is_empty())
    {
        write!(s, ": {}", d.trim()).unwrap();
    }
    if let Some(p) = input["placeholder"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
    {
        write!(s, ", e.g. {p}").unwrap();
    }
    s
}

/// The command a package type is run with, or `None` for one mcpdial cannot run.
fn runtime(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "npm" => Some(&["npx", "-y"]),
        "pypi" => Some(&["uvx"]),
        "oci" => Some(&["docker", "run", "-i", "--rm"]),
        _ => None,
    }
}

fn package(pkg: &Value, slots: &mut Slots, notes: &mut Vec<String>) -> Result<ServerConfig> {
    let kind = pkg["registryType"].as_str().unwrap_or("");
    let Some(runtime) = runtime(kind) else {
        return Err(Error::usage(format!(
            "package type {kind:?} is not one mcpdial can run (npm, pypi or oci)"
        )));
    };
    let identifier = word(&pkg["identifier"], "package identifier")?;
    let version = pkg["version"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if version.is_some_and(|v| v.contains(char::is_whitespace)) {
        return Err(Error::config(
            "registry entry has an unusable package version",
        ));
    }
    let pinned = match (kind, version) {
        ("npm", Some(v)) => format!("{identifier}@{v}"),
        ("pypi", Some(v)) => format!("{identifier}=={v}"),
        _ => identifier,
    };

    let mut argv: Vec<String> = runtime.iter().map(|s| s.to_string()).collect();
    let runtime_args = arguments(&pkg["runtimeArguments"], slots)?;
    if runtime_args.iter().any(|a| a == "-y" || a == "--yes") {
        argv.retain(|a| a != "-y");
    }
    argv.extend(runtime_args);
    let (env, names) = environment(&pkg["environmentVariables"], slots, notes)?;
    if kind == "oci" {
        // The process mcpdial spawns is docker; `-e NAME` carries each variable on
        // from there into the container, set or inherited alike.
        for name in names {
            argv.push("-e".into());
            argv.push(name);
        }
    }
    argv.push(pinned);
    argv.extend(arguments(&pkg["packageArguments"], slots)?);

    let mut c = ServerConfig::stdio(shell_join(&argv));
    c.env = env;
    Ok(c)
}

/// One string from the entry that has to stand as a single argv word.
fn word(v: &Value, what: &str) -> Result<String> {
    match v.as_str().map(str::trim) {
        Some(s) if !s.is_empty() && !s.starts_with('-') && !s.contains(char::is_whitespace) => {
            Ok(s.to_string())
        }
        _ => Err(Error::config(format!(
            "registry entry has an unusable {what}: {v}"
        ))),
    }
}

/// The argv words a list of registry arguments stands for. A named argument is
/// its flag then its value; either kind is left out when it is optional and got
/// no value.
fn arguments(args: &Value, slots: &mut Slots) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for arg in list(args) {
        let flag = arg["name"]
            .as_str()
            .filter(|_| arg["type"] == "named")
            .filter(|n| !n.is_empty());
        let label = arg["valueHint"]
            .as_str()
            .or(flag)
            .or(arg["placeholder"].as_str())
            .unwrap_or("value");
        let value = match arg["value"].as_str() {
            Some(text) => slots.fill(text, &arg["variables"])?,
            None => slots.take(arg, label)?,
        };
        let Some(value) = value else { continue };
        if let Some(flag) = flag {
            out.push(flag.to_string());
        }
        out.push(value);
    }
    Ok(out)
}

/// The `env` to save and the names of every variable the entry mentions.
///
/// A required variable is saved as a `${VAR}` placeholder and one with a default
/// as `${VAR:-default}`, so the file never holds the value. An optional one with no
/// default is only named in the note: exported in the shell, it reaches the server
/// anyway, and saving `${VAR}` would make it mandatory.
fn environment(
    vars: &Value,
    slots: &mut Slots,
    notes: &mut Vec<String>,
) -> Result<(BTreeMap<String, String>, Vec<String>)> {
    let mut env = BTreeMap::new();
    let mut names = Vec::new();
    let mut lines = Vec::new();
    for var in list(vars) {
        let Some(name) = var["name"].as_str().map(str::trim).filter(|n| {
            !n.is_empty() && !n.contains(['=', '\0']) && !n.contains(char::is_whitespace)
        }) else {
            continue;
        };
        let required = var["isRequired"].as_bool().unwrap_or(false);
        let default = var["default"].as_str();
        let value = match var["value"].as_str() {
            Some(text) => slots.fill(text, &var["variables"])?,
            None if required => Some(format!("${{{name}}}")),
            None => default.map(|d| format!("${{{name}:-{d}}}")),
        };
        if let Some(v) = value {
            env.insert(name.to_string(), v);
        }
        names.push(name.to_string());
        lines.push(input_line(var, name));
    }
    if !lines.is_empty() {
        let saved = !env.is_empty();
        let unsaved = env.len() < names.len();
        let mut note = format!(
            "environment variables this server reads:\n  {}\n",
            lines.join("\n  ")
        );
        if saved {
            note.push_str(
                "the required ones are saved as ${VAR} placeholders; set them before dialing.",
            );
        }
        if unsaved {
            let others = if saved { "The others" } else { "They" };
            write!(
                note,
                "{}{others} reach the server when exported in your shell, or with --env KEY=VALUE.",
                if saved { " " } else { "" }
            )
            .unwrap();
        }
        notes.push(note);
    }
    Ok((env, names))
}

/// One line of a note about a variable or header: `NAME (required, secret,
/// default x): description`.
fn input_line(input: &Value, name: &str) -> String {
    let mut tags = Vec::new();
    if input["isRequired"].as_bool().unwrap_or(false) {
        tags.push("required".to_string());
    }
    if input["isSecret"].as_bool().unwrap_or(false) {
        tags.push("secret".to_string());
    }
    if let Some(d) = input["default"].as_str() {
        tags.push(format!("default {d}"));
    }
    let mut line = name.to_string();
    if !tags.is_empty() {
        write!(line, " ({})", tags.join(", ")).unwrap();
    }
    if let Some(d) = input["description"]
        .as_str()
        .filter(|d| !d.trim().is_empty())
    {
        write!(line, ": {}", d.trim()).unwrap();
    }
    line
}

fn remote(r: &Value, slots: &mut Slots, notes: &mut Vec<String>) -> Result<ServerConfig> {
    let template = r["url"]
        .as_str()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| Error::config("registry entry has a remote with no URL"))?;
    // A URL cannot be left out the way an argument can. Unfilled because a required
    // variable is missing, it is reported through `missing` and never saved; unfilled
    // for any other reason, the entry is at fault.
    let missing_before = slots.missing.len();
    let url = match slots.fill(template, &r["variables"])? {
        Some(url) => url,
        None if slots.missing.len() > missing_before => template.to_string(),
        None => {
            return Err(Error::config(
                "registry entry has a remote URL with a variable that has no value",
            ))
        }
    };
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.contains(char::is_whitespace)
    {
        return Err(Error::config(format!(
            "registry entry has an unusable remote URL: {url:?}"
        )));
    }
    if r["type"] == "sse" {
        notes.push(SSE_NOTE.into());
    }

    // A required header with no value is saved as a placeholder read from the
    // environment, like a required variable. An optional one is only named: saved
    // as `${VAR}` it would be mandatory, and sent unset it would be a bad credential
    // where none was needed.
    let mut c = ServerConfig::http(url);
    let mut required = Vec::new();
    let mut optional = Vec::new();
    for h in list(&r["headers"]) {
        let Some(name) = h["name"].as_str().map(str::trim).filter(|n| {
            !n.is_empty() && !n.contains([':', '\r', '\n']) && !n.contains(char::is_whitespace)
        }) else {
            continue;
        };
        let value = match h["value"].as_str() {
            Some(text) => slots.fill(text, &h["variables"])?,
            None if h["isRequired"].as_bool().unwrap_or(false) => {
                let var = env_name(name);
                required.push(input_line(h, &format!("{name} as ${{{var}}}")));
                Some(format!("${{{var}}}"))
            }
            None => {
                optional.push(input_line(h, name));
                None
            }
        };
        if let Some(v) = value.filter(|v| !v.contains(['\r', '\n'])) {
            c.headers.insert(name.to_string(), v);
        }
    }
    if !required.is_empty() {
        notes.push(format!(
            "headers this server needs, saved as placeholders read from the environment:\n  {}",
            required.join("\n  ")
        ));
    }
    if !optional.is_empty() {
        notes.push(format!(
            "headers this server accepts, not saved:\n  {}\npass one with -H 'Name: value'; for Authorization, --token-env VAR or `mcpdial login` also serve.",
            optional.join("\n  ")
        ));
    }
    Ok(c)
}

/// The environment variable a header's value is read from: `X-Api-Key` becomes `X_API_KEY`.
fn env_name(header: &str) -> String {
    header
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::stdio::split_command;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn argv(c: &ServerConfig) -> Vec<String> {
        split_command(c.stdio.as_deref().expect("a stdio config")).unwrap()
    }

    /// com.pulsemcp/remote-filesystem, as the registry serves it.
    fn npm_with_env() -> Value {
        json!({
            "name": "com.pulsemcp/remote-filesystem",
            "description": "MCP server for remote filesystem operations on cloud storage (Google Cloud Storage).",
            "version": "0.1.3",
            "packages": [{
                "registryType": "npm", "registryBaseUrl": "https://registry.npmjs.org",
                "identifier": "remote-filesystem-mcp-server", "version": "0.1.3",
                "runtimeHint": "npx", "transport": {"type": "stdio"},
                "runtimeArguments": [{"value": "-y", "type": "positional"}],
                "environmentVariables": [
                    {"description": "Google Cloud Storage bucket name.", "isRequired": true, "name": "GCS_BUCKET"},
                    {"description": "Google Cloud project ID.", "name": "GCS_PROJECT_ID"},
                    {"description": "Service account private key for inline credentials.", "isSecret": true, "name": "GCS_PRIVATE_KEY"},
                    {"description": "Make uploaded files publicly accessible.", "default": "false", "name": "GCS_MAKE_PUBLIC"}
                ]
            }]
        })
    }

    /// ai.codenib/codenib: a pypi package with a named runtime argument and a
    /// required positional the user supplies.
    fn pypi_with_required_positional() -> Value {
        json!({
            "name": "ai.codenib/codenib",
            "description": "Source-linked CodeGraph exploration.",
            "version": "0.2.3",
            "packages": [{
                "registryType": "pypi", "registryBaseUrl": "https://pypi.org",
                "identifier": "codenib", "version": "0.2.3", "runtimeHint": "uvx",
                "transport": {"type": "stdio"},
                "runtimeArguments": [{"description": "Install the version-matched CodeNib CodeGraph and MCP runtime.",
                                      "value": "codenib[graph,mcp]==0.2.3", "type": "named", "name": "--with"}],
                "packageArguments": [
                    {"value": "mcp", "type": "positional"},
                    {"description": "Repository checkout previously indexed with CodeNib.", "isRequired": true,
                     "format": "filepath", "type": "positional", "valueHint": "repository"}
                ]
            }]
        })
    }

    /// app.lasers.guildcontrol/discord's oci package: a `{variable}` inside a named
    /// runtime argument, and a required secret.
    fn oci_with_variable() -> Value {
        json!({
            "name": "app.lasers.guildcontrol/discord",
            "version": "2.2.0",
            "packages": [{
                "registryType": "oci", "identifier": "ghcr.io/j-256/guildcontrol:2.2.0",
                "runtimeHint": "docker", "transport": {"type": "stdio"},
                "runtimeArguments": [
                    {"value": "--read-only", "type": "positional"},
                    {"description": "Read-only bind mount for the non-secret connector configuration", "isRequired": true,
                     "value": "type=bind,source={config_file},target=/configuration/guildcontrol.json,readonly",
                     "variables": {"config_file": {"description": "Absolute host path to the connector configuration",
                                                   "isRequired": true, "format": "filepath",
                                                   "placeholder": "/absolute/path/to/guildcontrol.json"}},
                     "type": "named", "name": "--mount"}
                ],
                "packageArguments": [
                    {"value": "serve", "type": "positional"},
                    {"value": "--config", "type": "positional"},
                    {"value": "/configuration/guildcontrol.json", "type": "positional"}
                ],
                "environmentVariables": [
                    {"description": "Discord bot token", "isRequired": true, "isSecret": true, "name": "DISCORD_BOT_TOKEN"}
                ]
            }]
        })
    }

    /// ae.propick/propick: a Streamable HTTP remote wanting a header, next to an
    /// SSE one, and ai.autorfp/mcp's URL variable folded in.
    fn remotes() -> Value {
        json!({
            "name": "ae.propick/propick",
            "version": "1.0.0",
            "remotes": [
                {"type": "sse", "url": "https://propick.ae/sse"},
                {"type": "streamable-http", "url": "https://{api_host}/mcp",
                 "variables": {"api_host": {"description": "API host for your region.", "isRequired": true,
                                            "choices": ["api.autorfp.ai", "api.eu.autorfp.ai"]}},
                 "headers": [{"description": "Bearer <integration key> issued in the Propick cabinet",
                              "isRequired": true, "isSecret": true, "name": "Authorization"}]}
            ]
        })
    }

    #[test]
    fn an_npm_package_runs_under_npx_with_its_version_and_placeholders() {
        let r = convert(&npm_with_env(), &Pick::Any, &[]).unwrap();
        assert_eq!(
            argv(&r.config),
            ["npx", "-y", "remote-filesystem-mcp-server@0.1.3"],
            "the entry's own -y is not doubled"
        );
        assert_eq!(r.config.env["GCS_BUCKET"], "${GCS_BUCKET}");
        assert_eq!(r.config.env["GCS_MAKE_PUBLIC"], "${GCS_MAKE_PUBLIC:-false}");
        assert!(
            !r.config.env.contains_key("GCS_PROJECT_ID"),
            "an optional variable with no default is not made mandatory"
        );
        assert!(r.missing.is_empty());
        let note = r
            .notes
            .iter()
            .find(|n| n.starts_with("environment"))
            .unwrap();
        assert!(
            note.contains("GCS_BUCKET (required): Google Cloud"),
            "{note}"
        );
        assert!(note.contains("GCS_PRIVATE_KEY (secret)"), "{note}");
        assert!(note.contains("GCS_MAKE_PUBLIC (default false)"), "{note}");
        assert!(
            note.contains("GCS_PROJECT_ID: Google Cloud project ID."),
            "{note}"
        );
        assert!(
            note.ends_with("set them before dialing. The others reach the server when exported in your shell, or with --env KEY=VALUE."),
            "{note}"
        );

        let mut only_optional = npm_with_env();
        only_optional["packages"][0]["environmentVariables"] =
            json!([{"name": "CONTEXT7_API_KEY", "isSecret": true, "description": "API key"}]);
        let r = convert(&only_optional, &Pick::Any, &[]).unwrap();
        assert!(r.config.env.is_empty());
        let note = r
            .notes
            .iter()
            .find(|n| n.starts_with("environment"))
            .unwrap();
        assert!(note.ends_with("API key\nThey reach the server when exported in your shell, or with --env KEY=VALUE."), "{note}");
    }

    #[test]
    fn a_pypi_package_runs_under_uvx_and_a_required_positional_comes_from_arg() {
        let entry = pypi_with_required_positional();
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(
            r.missing,
            ["repository (required): Repository checkout previously indexed with CodeNib."]
        );

        let r = convert(&entry, &Pick::Any, &args(&["/src/my repo"])).unwrap();
        assert!(r.missing.is_empty());
        assert_eq!(
            argv(&r.config),
            [
                "uvx",
                "--with",
                "codenib[graph,mcp]==0.2.3",
                "codenib==0.2.3",
                "mcp",
                "/src/my repo"
            ]
        );
        assert!(
            r.config
                .stdio
                .as_deref()
                .unwrap()
                .contains("'/src/my repo'"),
            "the value is quoted for split_command: {:?}",
            r.config.stdio
        );
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("in --arg order:\n  repository (required)")),
            "{:?}",
            r.notes
        );

        let e = convert(&entry, &Pick::Any, &args(&["/src", "extra"])).unwrap_err();
        assert!(e.to_string().contains("1 value(s) too many"), "{e}");
    }

    #[test]
    fn an_oci_package_runs_under_docker_and_forwards_its_variables() {
        let entry = oci_with_variable();
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(r.missing.len(), 1);
        assert!(
            r.missing[0].starts_with("config_file (required): Absolute host path"),
            "{:?}",
            r.missing
        );
        assert!(r.missing[0].ends_with(", e.g. /absolute/path/to/guildcontrol.json"));

        let r = convert(
            &entry,
            &Pick::Package("oci".into()),
            &args(&["/etc/gc.json"]),
        )
        .unwrap();
        assert_eq!(
            argv(&r.config),
            [
                "docker",
                "run",
                "-i",
                "--rm",
                "--read-only",
                "--mount",
                "type=bind,source=/etc/gc.json,target=/configuration/guildcontrol.json,readonly",
                "-e",
                "DISCORD_BOT_TOKEN",
                "ghcr.io/j-256/guildcontrol:2.2.0",
                "serve",
                "--config",
                "/configuration/guildcontrol.json"
            ]
        );
        assert_eq!(r.config.env["DISCORD_BOT_TOKEN"], "${DISCORD_BOT_TOKEN}");
    }

    #[test]
    fn a_remote_becomes_http_with_its_url_filled_and_headers_as_placeholders() {
        let entry = remotes();
        let r = convert(&entry, &Pick::Any, &args(&["api.eu.autorfp.ai"])).unwrap();
        assert_eq!(
            r.config.http.as_deref(),
            Some("https://api.eu.autorfp.ai/mcp")
        );
        assert_eq!(r.config.headers["Authorization"], "${AUTHORIZATION}");
        assert!(r.config.stdio.is_none());
        assert!(
            !r.notes.iter().any(|n| n.contains("SSE")),
            "streamable-http wins over sse"
        );
        let note = r.notes.iter().find(|n| n.starts_with("headers")).unwrap();
        assert!(
            note.contains("Authorization as ${AUTHORIZATION} (required, secret): Bearer"),
            "{note}"
        );

        let e = convert(&entry, &Pick::Any, &args(&["evil.example"])).unwrap_err();
        assert!(
            e.to_string()
                .contains("api_host must be one of api.autorfp.ai, api.eu.autorfp.ai"),
            "{e}"
        );

        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(r.missing.len(), 1, "a URL variable is required");

        // io.github.upstash/context7: Authorization is optional there, and saving
        // it as a placeholder would turn anonymous use into a bad credential.
        let entry = json!({"remotes": [{"type": "streamable-http", "url": "https://mcp.context7.com/mcp",
                                        "headers": [{"name": "Authorization", "isSecret": true,
                                                     "description": "API key for authentication."}]}]});
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert!(r.config.headers.is_empty(), "{:?}", r.config.headers);
        let note = r
            .notes
            .iter()
            .find(|n| n.starts_with("headers this server accepts"))
            .unwrap();
        assert!(
            note.contains("Authorization (secret): API key") && note.contains("--token-env"),
            "{note}"
        );
    }

    #[test]
    fn an_sse_only_remote_is_saved_with_the_note() {
        let entry = json!({"name": "ai.agentrapay/agentra", "version": "1.0.0",
                           "remotes": [{"type": "sse", "url": "https://api.agentrapay.ai/mcp"}]});
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(
            r.config.http.as_deref(),
            Some("https://api.agentrapay.ai/mcp")
        );
        assert_eq!(r.notes, [SSE_NOTE]);
        assert_eq!(transports(&entry), ["sse"]);
    }

    #[test]
    fn the_pick_chooses_a_remote_over_a_package_and_says_what_else_there_is() {
        let mut entry = npm_with_env();
        entry["remotes"] = json!([{"type": "streamable-http", "url": "https://fs.example/mcp"}]);
        assert_eq!(transports(&entry), ["http", "stdio (npm)"]);

        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(r.config.http.as_deref(), Some("https://fs.example/mcp"));
        assert!(
            r.notes.is_empty(),
            "a remote reads no environment: {:?}",
            r.notes
        );

        let r = convert(&entry, &Pick::Package("npm".into()), &[]).unwrap();
        assert!(r.config.stdio.is_some());

        let e = convert(&entry, &Pick::Package("oci".into()), &[]).unwrap_err();
        assert_eq!(
            e.to_string(),
            "this entry has no oci package; it offers http, stdio (npm)"
        );
        let e = convert(&npm_with_env(), &Pick::Remote, &[]).unwrap_err();
        assert_eq!(
            e.to_string(),
            "this entry has no remote endpoint; it offers stdio (npm)"
        );

        let unsupported = json!({"packages": [{"registryType": "nuget", "identifier": "X", "transport": {"type": "stdio"}}]});
        let e = convert(&unsupported, &Pick::Any, &[]).unwrap_err();
        assert!(
            e.to_string()
                .starts_with("this entry offers stdio (nuget);"),
            "{e}"
        );
        assert!(convert(&json!({}), &Pick::Any, &[]).is_err());
    }

    #[test]
    fn crafted_entries_cannot_grow_a_second_command() {
        let entry = json!({"packages": [{
            "registryType": "npm", "identifier": "pkg", "version": "1.0.0", "transport": {"type": "stdio"},
            "packageArguments": [{"value": "a; rm -rf /", "type": "positional"},
                                 {"value": "$(whoami) 'quoted' \"too\"", "type": "positional"}]
        }]});
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(
            argv(&r.config),
            [
                "npx",
                "-y",
                "pkg@1.0.0",
                "a; rm -rf /",
                "$(whoami) 'quoted' \"too\""
            ],
            "each value stays one word"
        );

        for identifier in ["--registry=http://evil", "", "two words"] {
            let entry = json!({"packages": [{"registryType": "npm", "identifier": identifier, "transport": {"type": "stdio"}}]});
            let e = convert(&entry, &Pick::Any, &[]).unwrap_err();
            assert!(
                e.to_string().contains("unusable package identifier"),
                "{identifier:?}: {e}"
            );
        }
        let entry = json!({"remotes": [{"type": "streamable-http", "url": "file:///etc/passwd"}]});
        assert!(convert(&entry, &Pick::Any, &[])
            .unwrap_err()
            .to_string()
            .contains("unusable remote URL"));
        let entry = json!({"remotes": [{"type": "streamable-http", "url": "https://x/mcp",
                                        "headers": [{"name": "X-A", "value": "ok\r\nInjected: yes"}, {"name": "X-B", "value": "fine"}]}]});
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(
            r.config.headers.len(),
            1,
            "a header carrying a line break is dropped"
        );
        assert_eq!(r.config.headers["X-B"], "fine");
    }

    #[test]
    fn optional_slots_are_left_out_whole_and_names_are_encoded() {
        let entry = json!({"packages": [{
            "registryType": "npm", "identifier": "pkg", "transport": {"type": "stdio"},
            "packageArguments": [{"type": "named", "name": "--port", "description": "Port to listen on"},
                                 {"type": "positional", "valueHint": "root", "default": "/srv"}]
        }]});
        let r = convert(&entry, &Pick::Any, &[]).unwrap();
        assert_eq!(
            argv(&r.config),
            ["npx", "-y", "pkg", "/srv"],
            "no flag without a value; the default fills the other"
        );
        assert!(r.missing.is_empty());
        let r = convert(&entry, &Pick::Any, &args(&["8080", "/data"])).unwrap();
        assert_eq!(
            argv(&r.config),
            ["npx", "-y", "pkg", "--port", "8080", "/data"]
        );

        assert_eq!(encode("io.github.owner/server"), "io.github.owner%2Fserver");
        assert_eq!(encode("a b&c"), "a%20b%26c");
        assert_eq!(env_name("X-Api-Key"), "X_API_KEY");
    }
}
