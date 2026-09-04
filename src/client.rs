//! Named servers become live sessions here: target resolution, credential
//! selection and refresh, and the parallel status probe behind `mcpdial ls`.

use crate::config::{ServerConfig, Store};
use crate::oauth;
use crate::protocol::{Error, Result};
use crate::session::Session;
use crate::transport::http::{HttpTransport, USER_AGENT};
use crate::transport::stdio::StdioTransport;
use crate::transport::{Logger, Transport};
use serde::Serialize;
use serde_json::Value;
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
        self.verbose.then(|| Box::new(|s: &str| eprintln!("{s}")) as Logger)
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
        return Ok(Resolved { name: target.to_string(), config: cfg, saved: true });
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Resolved { name: target.to_string(), config: ServerConfig::http(target), saved: false });
    }
    if let Some(cmd) = target.strip_prefix("stdio:") {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return Err(Error::usage("stdio: target needs a command after the colon"));
        }
        return Ok(Resolved { name: target.to_string(), config: ServerConfig::stdio(cmd), saved: false });
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
        cred = oauth::refresh(&oauth::Http::new(opts.timeout, Some(opts.user_agent.clone())), &cred)?;
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
        let t = StdioTransport::spawn_str(cmd, opts.timeout, opts.verbose, opts.logger())?;
        let mut session = Session::new(Box::new(t) as Box<dyn Transport>);
        let info = session.initialize()?.clone();
        return Ok(Connection { name: r.name.clone(), session, server_info: info, auth: AuthUsed::None });
    }

    let (token, auth) = select_token(store, r, opts)?;
    let mut session = Session::new(Box::new(http_transport(r, token, opts)) as Box<dyn Transport>);
    match session.initialize() {
        Ok(info) => {
            let info = info.clone();
            Ok(Connection { name: r.name.clone(), session, server_info: info, auth })
        }
        Err(e) if e.is_auth_challenge() && auth == AuthUsed::Saved => {
            let cred = store.credential(&r.name)?.unwrap_or_default();
            if !cred.can_refresh() {
                return Err(e);
            }
            let cred = oauth::refresh(&oauth::Http::new(opts.timeout, Some(opts.user_agent.clone())), &cred)?;
            store.save_credential(&r.name, cred.clone())?;
            let mut session =
                Session::new(Box::new(http_transport(r, cred.access_token, opts)) as Box<dyn Transport>);
            let info = session.initialize()?.clone();
            Ok(Connection { name: r.name.clone(), session, server_info: info, auth })
        }
        Err(e) => Err(e),
    }
}

// -- status -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Connected,
    /// The server wants a credential and we had none to offer.
    AuthRequired,
    /// We sent a credential and the server refused it.
    TokenRejected,
    /// 403 without a challenge: WAF, allowlist, geo. A token will not help.
    Blocked,
    Http { status: u16 },
    Unreachable { detail: String },
    Error { detail: String },
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Connected => "connected".into(),
            Status::AuthRequired => "auth required".into(),
            Status::TokenRejected => "token rejected".into(),
            Status::Blocked => "blocked (403)".into(),
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
        || store.credential(&r.name).ok().flatten().is_some_and(|c| c.has_token());

    match connect(store, r, opts) {
        Ok(mut conn) => {
            p.auth = conn.auth;
            let si = &conn.server_info["serverInfo"];
            p.server = si["name"].as_str().map(|n| {
                match si["version"].as_str() {
                    Some(v) if !v.is_empty() => format!("{n} {v}"),
                    _ => n.to_string(),
                }
            });
            if with_tools {
                match conn.session.list_tools() {
                    Ok(t) => p.tools = Some(t),
                    Err(e) => p.status = Status::Error { detail: e.to_string() },
                }
            }
        }
        Err(e) => p.status = classify(&e, had_credential),
    }
    p
}

fn classify(e: &Error, had_credential: bool) -> Status {
    match e {
        Error::Http { status, www_authenticate, .. } => match (status, www_authenticate) {
            (_, Some(_)) | (401, None) if had_credential => Status::TokenRejected,
            (_, Some(_)) | (401, None) => Status::AuthRequired,
            (403, None) => Status::Blocked,
            (s, _) => Status::Http { status: *s },
        },
        Error::Transport(d) => Status::Unreachable { detail: d.clone() },
        other => Status::Error { detail: other.to_string() },
    }
}

/// Probe every configured server concurrently, in config order.
pub fn probe_all(store: &Store, opts: &Options, with_tools: bool) -> Result<Vec<Probe>> {
    let servers = store.servers()?;
    let mut results: Vec<Option<Probe>> = vec![None; servers.len()];
    std::thread::scope(|scope| {
        for (slot, (name, cfg)) in results.iter_mut().zip(servers.iter()) {
            let r = Resolved { name: name.clone(), config: cfg.clone(), saved: true };
            let store = store.clone();
            let opts = Options { verbose: false, ..opts.clone() };
            scope.spawn(move || *slot = Some(probe(&store, &r, &opts, with_tools)));
        }
    });
    Ok(results.into_iter().flatten().collect())
}

/// Human-readable parameter summary from a tool's JSON schema.
pub fn describe_params(tool: &Value) -> Vec<String> {
    let schema = &tool["inputSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema["properties"].as_object() else { return Vec::new() };
    props
        .iter()
        .map(|(name, spec)| {
            let ty = match &spec["type"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|"),
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
        assert_eq!(classify(&http(401, Some("Bearer")), false), Status::AuthRequired);
        assert_eq!(classify(&http(401, Some("Bearer")), true), Status::TokenRejected);
        assert_eq!(classify(&http(403, Some("Bearer")), false), Status::AuthRequired);
        assert_eq!(classify(&http(403, None), true), Status::Blocked);
        assert_eq!(classify(&http(500, None), false), Status::Http { status: 500 });
        assert!(matches!(classify(&Error::transport("x"), false), Status::Unreachable { .. }));
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
        assert!(describe_params(&json!({"name":"x"})).is_empty());
    }
}
