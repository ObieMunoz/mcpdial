//! Named servers become live sessions here: target resolution, credential
//! selection and refresh, and the parallel status probe behind `mcpdial ls` -
//! whose answers are remembered for [`STATUS_TTL`], so the next listing dials
//! nothing.

use crate::config::{now, Credential, ProbeRecord, ServerConfig, Store};
use crate::oauth;
use crate::protocol::{Error, Result};
use crate::session::Session;
use crate::transport::http::{is_legacy_sse_error, HttpTransport, USER_AGENT};
use crate::transport::stdio::StdioTransport;
use crate::transport::{Logger, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

/// Per-invocation knobs that apply to any server.
#[derive(Debug, Clone)]
pub struct Options {
    pub timeout: Duration,
    pub user_agent: String,
    pub extra_headers: Vec<(String, String)>,
    /// `--token-env` on the command line: beats everything else.
    pub token_env: Option<String>,
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            user_agent: USER_AGENT.to_string(),
            extra_headers: Vec::new(),
            token_env: None,
            verbose: false,
        }
    }
}

impl Options {
    fn logger(&self) -> Option<Logger> {
        self.verbose
            .then(|| Box::new(|s: &str| eprintln!("{s}")) as Logger)
    }
}

/// A target argument turned into something we can connect to.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Config name, or the URL / command itself for an ad-hoc target.
    pub name: String,
    pub config: ServerConfig,
    /// True if it came from `servers.json`.
    pub saved: bool,
}

/// `name` from the store, `http(s)://...` for an ad-hoc HTTP server, or
/// `stdio:<command>` for an ad-hoc local process.
pub fn resolve(store: &Store, target: &str) -> Result<Resolved> {
    if let Some(cfg) = store.server(target)? {
        return Ok(Resolved {
            name: target.to_string(),
            config: cfg,
            saved: true,
        });
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Resolved {
            name: target.to_string(),
            config: ServerConfig::http(target),
            saved: false,
        });
    }
    if let Some(cmd) = target.strip_prefix("stdio:") {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return Err(Error::usage(
                "stdio: target needs a command after the colon",
            ));
        }
        return Ok(Resolved {
            name: target.to_string(),
            config: ServerConfig::stdio(cmd),
            saved: false,
        });
    }
    Err(Error::usage(format!(
        "unknown server {target:?}. Add it with `mcpdial add {target} --http URL` \
         (or --stdio CMD), or pass a URL or stdio:<command> directly."
    )))
}

pub struct Connection {
    pub name: String,
    pub session: Session<Box<dyn Transport>>,
    pub server_info: Value,
    /// Which credential was used, for status reporting.
    pub auth: AuthUsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthUsed {
    None,
    Env,
    Saved,
}

fn env_token(var: &str) -> Result<String> {
    std::env::var(var).ok().filter(|t| !t.is_empty()).ok_or_else(|| {
        Error::usage(format!(
            "${var} is unset or empty.\n       export {var}=... first (don't pass the token as an argument)."
        ))
    })
}

/// Pick the bearer token for an HTTP server, refreshing a saved one if it has expired.
fn select_token(store: &Store, r: &Resolved, opts: &Options) -> Result<(Option<String>, AuthUsed)> {
    if let Some(var) = opts.token_env.as_deref().or(r.config.token_env.as_deref()) {
        return Ok((Some(env_token(var)?), AuthUsed::Env));
    }
    let Some(mut cred) = store.credential(&r.name)? else {
        return Ok((None, AuthUsed::None));
    };
    if cred.is_expired() && cred.can_refresh() {
        cred = oauth::refresh(
            &oauth::Http::new(opts.timeout, Some(opts.user_agent.clone())),
            &cred,
        )?;
        store.save_credential(&r.name, cred.clone())?;
    }
    Ok((cred.access_token.filter(|t| !t.is_empty()), AuthUsed::Saved))
}

fn http_transport(r: &Resolved, token: Option<String>, opts: &Options) -> HttpTransport {
    let mut b = HttpTransport::builder(r.config.http.clone().unwrap_or_default())
        .token(token)
        .timeout(opts.timeout)
        .user_agent(opts.user_agent.clone());
    for (k, v) in &r.config.headers {
        b = b.header(k.clone(), v.clone());
    }
    for (k, v) in &opts.extra_headers {
        b = b.header(k.clone(), v.clone());
    }
    if let Some(log) = opts.logger() {
        b = b.log(log);
    }
    b.build()
}

/// Open a session and complete the `initialize` handshake.
///
/// For HTTP servers with a saved OAuth credential, a 401 triggers one refresh and
/// retry before giving up, so an expired token that the clock did not predict still
/// works without a visible hiccup.
pub fn connect(store: &Store, r: &Resolved, opts: &Options) -> Result<Connection> {
    if let Some(cmd) = &r.config.stdio {
        let argv = crate::transport::stdio::split_command(cmd)?;
        let env: Vec<(String, String)> = r
            .config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let t = StdioTransport::spawn_with(
            &argv,
            &env,
            r.config.cwd.as_deref().map(std::path::Path::new),
            opts.timeout,
            opts.verbose,
            opts.logger(),
        )?;
        let mut session = Session::new(Box::new(t) as Box<dyn Transport>);
        let info = session.initialize()?.clone();
        return Ok(Connection {
            name: r.name.clone(),
            session,
            server_info: info,
            auth: AuthUsed::None,
        });
    }

    let (token, auth) = select_token(store, r, opts)?;
    let mut session = Session::new(Box::new(http_transport(r, token, opts)) as Box<dyn Transport>);
    match session.initialize() {
        Ok(info) => {
            let info = info.clone();
            Ok(Connection {
                name: r.name.clone(),
                session,
                server_info: info,
                auth,
            })
        }
        Err(e) if e.is_auth_challenge() && auth == AuthUsed::Saved => {
            let cred = store.credential(&r.name)?.unwrap_or_default();
            if !cred.can_refresh() {
                return Err(e);
            }
            let cred = oauth::refresh(
                &oauth::Http::new(opts.timeout, Some(opts.user_agent.clone())),
                &cred,
            )?;
            store.save_credential(&r.name, cred.clone())?;
            let mut session = Session::new(
                Box::new(http_transport(r, cred.access_token, opts)) as Box<dyn Transport>
            );
            let info = session.initialize()?.clone();
            Ok(Connection {
                name: r.name.clone(),
                session,
                server_info: info,
                auth,
            })
        }
        Err(e) => Err(e),
    }
}

// -- status -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Connected,
    /// The server wants a credential and we had none to offer.
    AuthRequired,
    /// We sent a credential and the server refused it.
    TokenRejected,
    /// 403 without a challenge: WAF, allowlist, geo. A token will not help.
    Blocked,
    /// Reachable and healthy, but speaking protocol 2024-11-05's HTTP+SSE transport.
    LegacySse,
    Http {
        status: u16,
    },
    Unreachable {
        detail: String,
    },
    Error {
        detail: String,
    },
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Connected => "connected".into(),
            Status::AuthRequired => "auth required".into(),
            Status::TokenRejected => "token rejected".into(),
            Status::Blocked => "blocked (403)".into(),
            Status::LegacySse => "legacy sse".into(),
            Status::Http { status } => format!("http {status}"),
            Status::Unreachable { .. } => "unreachable".into(),
            Status::Error { .. } => "error".into(),
        }
    }
    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::Unreachable { detail } | Status::Error { detail } => Some(detail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub name: String,
    pub kind: &'static str,
    pub location: String,
    pub status: Status,
    pub auth: AuthUsed,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
}

/// Connect, list tools, and classify the outcome. Never panics, never prompts.
pub fn probe(store: &Store, r: &Resolved, opts: &Options, with_tools: bool) -> Probe {
    let mut p = Probe {
        name: r.name.clone(),
        kind: r.config.kind(),
        location: r.config.location().to_string(),
        status: Status::Connected,
        auth: AuthUsed::None,
        server: None,
        tools: None,
    };
    let had_credential = r.config.token_env.is_some()
        || opts.token_env.is_some()
        || store
            .credential(&r.name)
            .ok()
            .flatten()
            .is_some_and(|c| c.has_token());

    match connect(store, r, opts) {
        Ok(mut conn) => {
            p.auth = conn.auth;
            let si = &conn.server_info["serverInfo"];
            p.server = si["name"].as_str().map(|n| match si["version"].as_str() {
                Some(v) if !v.is_empty() => format!("{n} {v}"),
                _ => n.to_string(),
            });
            if with_tools {
                match conn.session.list_tools() {
                    Ok(t) => p.tools = Some(t),
                    Err(e) => {
                        p.status = Status::Error {
                            detail: e.to_string(),
                        }
                    }
                }
            }
        }
        Err(e) => p.status = classify(&e, had_credential),
    }
    p
}

fn classify(e: &Error, had_credential: bool) -> Status {
    match e {
        Error::Http {
            status,
            www_authenticate,
            ..
        } => match (status, www_authenticate) {
            (_, Some(_)) | (401, None) if had_credential => Status::TokenRejected,
            (_, Some(_)) | (401, None) => Status::AuthRequired,
            (403, None) => Status::Blocked,
            (s, _) => Status::Http { status: *s },
        },
        Error::Transport(d) if is_legacy_sse_error(d) => Status::LegacySse,
        Error::Transport(d) => Status::Unreachable { detail: d.clone() },
        other => Status::Error {
            detail: other.to_string(),
        },
    }
}

/// Probe the given servers concurrently, keeping the order they were given in.
fn probe_each(
    store: &Store,
    servers: &[(String, ServerConfig)],
    opts: &Options,
    with_tools: bool,
) -> Vec<Probe> {
    let mut results: Vec<Option<Probe>> = vec![None; servers.len()];
    std::thread::scope(|scope| {
        for (slot, (name, cfg)) in results.iter_mut().zip(servers.iter()) {
            let r = Resolved {
                name: name.clone(),
                config: cfg.clone(),
                saved: true,
            };
            let store = store.clone();
            let opts = Options {
                verbose: false,
                ..opts.clone()
            };
            scope.spawn(move || *slot = Some(probe(&store, &r, &opts, with_tools)));
        }
    });
    results.into_iter().flatten().collect()
}

/// Probe every configured server concurrently, in config order.
pub fn probe_all(store: &Store, opts: &Options, with_tools: bool) -> Result<Vec<Probe>> {
    let servers: Vec<(String, ServerConfig)> = store.servers()?.into_iter().collect();
    Ok(probe_each(store, &servers, opts, with_tools))
}

// -- listing ------------------------------------------------------------------------

/// How long a remembered status stands in for a live one.
///
/// Long enough that a burst of listings costs nothing, short enough that the
/// answer still describes this sitting at the terminal rather than the last one.
pub const STATUS_TTL: Duration = Duration::from_secs(5 * 60);

/// Whether [`listing`] may answer from the remembered probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Reuse a status younger than [`STATUS_TTL`]; probe the rest.
    Remembered,
    /// Probe every server, whatever was remembered.
    Live,
}

/// One row of `mcpdial ls`: a status, and how old that status is.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub name: String,
    pub kind: &'static str,
    pub location: String,
    pub status: Status,
    pub auth: AuthUsed,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<usize>,
    pub checked_at: u64,
    pub age_seconds: u64,
}

impl Listing {
    fn probed(p: &Probe, checked_at: u64) -> Self {
        Self {
            name: p.name.clone(),
            kind: p.kind,
            location: p.location.clone(),
            status: p.status.clone(),
            auth: p.auth,
            server: p.server.clone(),
            tools: p.tools.as_ref().map(Vec::len),
            checked_at,
            age_seconds: 0,
        }
    }

    /// The row a remembered probe stands for, or `None` if this build cannot
    /// read the status back.
    fn remembered(name: &str, cfg: &ServerConfig, rec: &ProbeRecord, now: u64) -> Option<Self> {
        Some(Self {
            name: name.to_string(),
            kind: cfg.kind(),
            location: cfg.location().to_string(),
            status: serde_json::from_value(rec.status.clone()).ok()?,
            auth: serde_json::from_value(rec.auth.clone()).ok()?,
            server: rec.server.clone(),
            tools: rec.tools,
            checked_at: rec.checked_at,
            age_seconds: now.saturating_sub(rec.checked_at),
        })
    }

    fn record(&self, key: u64) -> ProbeRecord {
        ProbeRecord {
            checked_at: self.checked_at,
            key,
            status: serde_json::to_value(&self.status).expect("a status is serializable"),
            auth: serde_json::to_value(self.auth).expect("an auth is serializable"),
            server: self.server.clone(),
            tools: self.tools,
        }
    }
}

/// What each server's status is a status *of*. Edit the server, or save or drop
/// a credential for it, and what was remembered no longer describes it.
fn probe_keys(store: &Store, servers: &BTreeMap<String, ServerConfig>) -> BTreeMap<String, u64> {
    let credentials = store.credentials().unwrap_or_default();
    servers
        .iter()
        .map(|(name, cfg)| {
            let mut h = DefaultHasher::new();
            serde_json::to_string(cfg)
                .expect("a server config is serializable")
                .hash(&mut h);
            credentials
                .get(name)
                .is_some_and(Credential::has_token)
                .hash(&mut h);
            (name.clone(), h.finish())
        })
        .collect()
}

fn still_current(rec: &ProbeRecord, key: u64, now: u64) -> bool {
    rec.key == key && now.saturating_sub(rec.checked_at) < STATUS_TTL.as_secs()
}

/// Every saved server with a status, dialing only the ones the remembered
/// probes cannot answer for.
pub fn listing(store: &Store, opts: &Options, freshness: Freshness) -> Result<Vec<Listing>> {
    let servers = store.servers()?;
    let keys = probe_keys(store, &servers);
    let now = now();
    let remembered = match freshness {
        Freshness::Remembered => store.probes().unwrap_or_default(),
        Freshness::Live => BTreeMap::new(),
    };

    let mut rows: BTreeMap<String, Listing> = BTreeMap::new();
    let mut cold: Vec<(String, ServerConfig)> = Vec::new();
    for (name, cfg) in &servers {
        let usable = remembered
            .get(name)
            .filter(|rec| still_current(rec, keys[name], now))
            .and_then(|rec| Listing::remembered(name, cfg, rec, now));
        match usable {
            Some(row) => {
                rows.insert(name.clone(), row);
            }
            None => cold.push((name.clone(), cfg.clone())),
        }
    }

    let fresh: Vec<Listing> = probe_each(store, &cold, opts, true)
        .iter()
        .map(|p| Listing::probed(p, now))
        .collect();
    if !fresh.is_empty() {
        // A cache that cannot be written is a slow `ls`, not a failed one.
        let _ = store.save_probes(
            fresh
                .iter()
                .map(|row| (row.name.clone(), row.record(keys[&row.name])))
                .collect(),
        );
    }
    rows.extend(fresh.into_iter().map(|row| (row.name.clone(), row)));
    Ok(rows.into_values().collect())
}

/// Human-readable parameter summary from a tool's JSON schema.
pub fn describe_params(tool: &Value) -> Vec<String> {
    let schema = &tool["inputSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema["properties"].as_object() else {
        return Vec::new();
    };
    // Required parameters first: they are what a caller has to get right. The sort
    // is stable, so everything else keeps the order the schema listed it in.
    let mut ordered: Vec<(&String, &Value)> = props.iter().collect();
    ordered.sort_by_key(|(name, _)| !required.contains(&name.as_str()));
    ordered
        .into_iter()
        .map(|(name, spec)| {
            let ty = match &spec["type"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("|"),
                _ if spec.get("enum").is_some() => "enum".into(),
                _ => "any".into(),
            };
            let mut line = format!("{name}: {ty}");
            if required.contains(&name.as_str()) {
                line.push_str(" (required)");
            }
            if let Some(d) = spec["description"].as_str() {
                let first = d.trim().lines().next().unwrap_or("");
                if !first.is_empty() {
                    line.push_str(&format!(" - {first}"));
                }
            }
            line
        })
        .collect()
}

/// A skeleton arguments object for a tool: every required property with a
/// placeholder for its type. Tools that require nothing get `{}`.
pub fn example_arguments(tool: &Value) -> String {
    let schema = &tool["inputSchema"];
    let props = schema["properties"].as_object();
    let fields: Vec<String> = schema["required"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(|name| {
            let spec = props.and_then(|p| p.get(name)).unwrap_or(&Value::Null);
            format!("{}: {}", json!(name), placeholder(spec))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// What stands in for one value in [`example_arguments`].
fn placeholder(spec: &Value) -> String {
    if let Some(values) = spec["enum"].as_array().filter(|v| !v.is_empty()) {
        return values
            .iter()
            .take(4)
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    match spec["type"].as_str() {
        Some("string") => "\"<string>\"".into(),
        Some("number") | Some("integer") => "<number>".into(),
        Some("boolean") => "true|false".into(),
        Some("array") => "[...]".into(),
        Some("object") => "{...}".into(),
        _ => "<value>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_by_challenge_not_status() {
        let http = |status, www: Option<&str>| Error::Http {
            status,
            body: String::new(),
            www_authenticate: www.map(str::to_string),
        };
        assert_eq!(
            classify(&http(401, Some("Bearer")), false),
            Status::AuthRequired
        );
        assert_eq!(
            classify(&http(401, Some("Bearer")), true),
            Status::TokenRejected
        );
        assert_eq!(
            classify(&http(403, Some("Bearer")), false),
            Status::AuthRequired
        );
        assert_eq!(classify(&http(403, None), true), Status::Blocked);
        assert_eq!(
            classify(&http(500, None), false),
            Status::Http { status: 500 }
        );
        assert!(matches!(
            classify(&Error::transport("x"), false),
            Status::Unreachable { .. }
        ));
    }

    #[test]
    fn the_older_transport_is_not_filed_under_unreachable() {
        let found = crate::transport::http::legacy_sse_error("https://example.test/sse");
        assert_eq!(classify(&found, false), Status::LegacySse);
        assert_eq!(Status::LegacySse.label(), "legacy sse");
        assert!(matches!(
            classify(&Error::transport("connection refused"), false),
            Status::Unreachable { .. }
        ));
    }

    #[test]
    fn resolves_adhoc_targets() {
        let store = Store::at(std::env::temp_dir().join("mcpdial-no-such-dir"));
        let r = resolve(&store, "https://x/mcp").unwrap();
        assert!(!r.saved);
        assert_eq!(r.config.http.as_deref(), Some("https://x/mcp"));
        let r = resolve(&store, "stdio:npx -y thing").unwrap();
        assert_eq!(r.config.stdio.as_deref(), Some("npx -y thing"));
        assert!(resolve(&store, "stdio:").is_err());
        assert!(resolve(&store, "nope").is_err());
    }

    #[test]
    fn describes_parameters() {
        let tool = json!({"name":"add","inputSchema":{"type":"object","properties":{
            "a":{"type":"number","description":"First\nsecond line"},
            "b":{"type":["number","null"]},
            "mode":{"enum":["x","y"]}
        },"required":["a"]}});
        let lines = describe_params(&tool);
        assert_eq!(lines[0], "a: number (required) - First");
        assert_eq!(lines[1], "b: number|null");
        assert_eq!(lines[2], "mode: enum");
        // Required first, whatever the order the schema listed them in.
        let reordered = json!({"inputSchema":{"properties":{
            "a":{"type":"string"}, "z":{"type":"string"}
        },"required":["z"]}});
        assert_eq!(
            describe_params(&reordered),
            ["z: string (required)", "a: string"]
        );
        assert!(describe_params(&json!({"name":"x"})).is_empty());
    }

    #[test]
    fn examples_show_the_required_arguments() {
        let tool = json!({"name":"new_page","inputSchema":{"type":"object","properties":{
            "url":{"type":"string"},
            "timeout":{"type":"number"},
            "mode":{"enum":["fast","slow"]},
            "flag":{"type":"boolean"}
        },"required":["url","mode","flag"]}});
        assert_eq!(
            example_arguments(&tool),
            r#"{"url": "<string>", "mode": "fast"|"slow", "flag": true|false}"#
        );
        // Nothing required means the empty object is already a complete call.
        assert_eq!(
            example_arguments(&json!({"name":"count","inputSchema":{"properties":{}}})),
            "{}"
        );
        assert_eq!(example_arguments(&json!({"name":"x"})), "{}");
    }
}
