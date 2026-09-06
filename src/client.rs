//! Named servers become live sessions here: target resolution, credential
//! selection and refresh, and the parallel status probe behind `mcpdial ls` -
//! whose answers are remembered for [`STATUS_TTL`], so the next listing dials
//! nothing.

use crate::config::{now, Credential, ProbeRecord, ServerConfig, Store};
use crate::elicit::{self, Answers, Elicit};
use crate::notify::Level;
use crate::oauth;
use crate::protocol::{Error, KnownVersion, Result};
use crate::schema;
use crate::session::Session;
use crate::transport::http::{is_legacy_sse_error, HttpTransport, USER_AGENT};
use crate::transport::stdio::StdioTransport;
use crate::transport::trace::{self, Trace};
use crate::transport::{Logger, TraceEvent, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

/// What bounds a wait when neither the command line nor the server says.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// What bounds a status probe when neither the command line nor the server
/// says. `ls` is a health check, not a call: a server that takes longer than this
/// to answer `initialize` is worth reporting as unreachable, and one that is down
/// should not hold the whole table for a minute.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-invocation knobs that apply to any server.
#[derive(Debug, Clone)]
pub struct Options {
    /// `--timeout` on the command line: beats the one saved for the server.
    pub timeout: Option<Duration>,
    /// What bounds a wait when neither the flag nor the server says:
    /// [`DEFAULT_TIMEOUT`], or [`PROBE_TIMEOUT`] behind a status probe.
    pub fallback_timeout: Duration,
    pub user_agent: String,
    pub extra_headers: Vec<(String, String)>,
    /// `--token-env` on the command line: beats everything else.
    pub token_env: Option<String>,
    /// `--protocol-version` on the command line: beats the one saved for the server.
    pub protocol_version: Option<KnownVersion>,
    pub verbose: bool,
    /// `--no-daemon` or `$MCPDIAL_NO_DAEMON`: dial a stdio server even when a
    /// daemon started for it is running.
    pub no_daemon: bool,
    /// One more attempt after a transient HTTP failure; `--no-retry` clears it.
    pub retry: bool,
    /// `--log-level`: the threshold the caller set for a server's own log
    /// notifications. A server that advertises `logging` is told it, so that it
    /// does not spend bandwidth on levels that would only be filtered here.
    pub log_level: Option<Level>,
    /// `--trace FILE` or `$MCPDIAL_TRACE`: where every message and transport
    /// event is appended as JSON Lines, whatever `verbose` says.
    pub trace: Option<Trace>,
    /// What this invocation can answer when the server elicits mid-call.
    pub elicit: Elicit,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: None,
            fallback_timeout: DEFAULT_TIMEOUT,
            user_agent: USER_AGENT.to_string(),
            extra_headers: Vec::new(),
            token_env: None,
            protocol_version: None,
            verbose: false,
            no_daemon: false,
            retry: true,
            log_level: None,
            trace: None,
            elicit: Elicit::default(),
        }
    }
}

impl Options {
    /// The wait for anything that is not a saved server: the registry, the
    /// catalog, a login.
    pub fn timeout_or_default(&self) -> Duration {
        self.timeout.unwrap_or(self.fallback_timeout)
    }

    /// The wait for one server: the flag, else its saved timeout, else the
    /// fallback. A saved value that is not a number of seconds is a config error,
    /// since the file was edited to say something that cannot bound a wait.
    pub fn timeout_for(&self, r: &Resolved) -> Result<Duration> {
        if let Some(flag) = self.timeout {
            return Ok(flag);
        }
        match r.config.timeout {
            Some(secs) => Duration::try_from_secs_f64(secs).map_err(|_| {
                Error::config(format!(
                    "{}: timeout must be a non-negative number of seconds, not {secs}",
                    r.name
                ))
            }),
            None => Ok(self.fallback_timeout),
        }
    }

    /// The same knobs behind a status probe: a server that neither the flag nor
    /// its own entry says how long to wait for gets [`PROBE_TIMEOUT`].
    fn for_status(&self) -> Self {
        Self {
            fallback_timeout: PROBE_TIMEOUT,
            ..self.clone()
        }
    }

    /// Where a transport dialing `target` reports what it does: stderr prose
    /// for `-v`, the trace file for `--trace`, both when both are on.
    pub fn logger(&self, target: &str) -> Option<Logger> {
        if !self.verbose && self.trace.is_none() {
            return None;
        }
        let verbose = self.verbose;
        let trace = self.trace.clone();
        let target = target.to_string();
        Some(Box::new(move |event: &TraceEvent<'_>| {
            if verbose {
                if let Some(line) = trace::describe(event) {
                    eprintln!("{line}");
                }
            }
            if let Some(trace) = &trace {
                trace.record(&target, event);
            }
        }))
    }
}

/// The HTTP helper an OAuth exchange for `name` runs over, reporting where the
/// server's own transport would.
pub fn oauth_http(opts: &Options, name: &str, timeout: Duration) -> oauth::Http {
    oauth::Http::new(timeout, Some(opts.user_agent.clone()))
        .retry(opts.retry)
        .log(opts.logger(name))
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

impl Resolved {
    /// The same target with every `${VAR}` filled in from the environment: what
    /// is dialed, as opposed to what was resolved, listed or saved, which still
    /// names the variable rather than holding its value.
    pub fn dialed(&self) -> Result<Resolved> {
        Ok(Resolved {
            name: self.name.clone(),
            config: self.config.expanded(|var| std::env::var(var).ok())?,
            saved: self.saved,
        })
    }
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
    /// What was dialed, kept for its allow and deny lists.
    pub config: ServerConfig,
    pub session: Session<Box<dyn Transport>>,
    pub server_info: Value,
    /// Which credential was used, for status reporting.
    pub auth: AuthUsed,
    /// What a form elicitation is filled in with. `shell`'s `elicit` command
    /// replaces it partway through a session.
    pub elicit: Answers,
}

impl Connection {
    /// The tools the server offers that its allow and deny lists permit: what
    /// every listing, lookup and completion sees. `session.list_tools` is the
    /// unfiltered escape hatch, as `raw tools/list` is.
    pub fn list_tools(&mut self) -> Result<Vec<Value>> {
        Ok(permitted_tools(self.session.list_tools()?, &self.config))
    }

    /// Every tool, the hidden ones carrying `"denied": true`.
    pub fn list_all_tools(&mut self) -> Result<Vec<Value>> {
        Ok(marked_tools(self.session.list_tools()?, &self.config))
    }
}

/// `tools` without the ones `cfg` denies.
pub fn permitted_tools(tools: Vec<Value>, cfg: &ServerConfig) -> Vec<Value> {
    tools
        .into_iter()
        .filter(|t| cfg.denial(tool_name(t)).is_none())
        .collect()
}

/// `tools` in full, with `"denied": true` on each one `cfg` hides.
pub fn marked_tools(tools: Vec<Value>, cfg: &ServerConfig) -> Vec<Value> {
    tools
        .into_iter()
        .map(|mut t| {
            if cfg.denial(tool_name(&t)).is_some() {
                t["denied"] = json!(true);
            }
            t
        })
        .collect()
}

fn tool_name(tool: &Value) -> &str {
    tool["name"].as_str().unwrap_or("")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthUsed {
    None,
    Env,
    Saved,
}

fn refresh_and_save(
    store: &Store,
    name: &str,
    cred: &Credential,
    opts: &Options,
    timeout: Duration,
) -> Result<Credential> {
    let http = oauth_http(opts, name, timeout);
    let fresh = if cred.renews_by_grant() {
        oauth::renew_client_credentials(&http, cred)?
    } else {
        oauth::refresh(&http, cred)?
    };
    store.save_credential(name, fresh.clone())?;
    Ok(fresh)
}

fn env_token(var: &str) -> Result<String> {
    std::env::var(var).ok().filter(|t| !t.is_empty()).ok_or_else(|| {
        Error::usage(format!(
            "${var} is unset or empty.\n       export {var}=... first (don't pass the token as an argument)."
        ))
    })
}

/// Pick the bearer token for an HTTP server, refreshing a saved one if it has expired.
fn select_token(
    store: &Store,
    r: &Resolved,
    opts: &Options,
    timeout: Duration,
) -> Result<(Option<String>, AuthUsed)> {
    if let Some(var) = opts.token_env.as_deref().or(r.config.token_env.as_deref()) {
        return Ok((Some(env_token(var)?), AuthUsed::Env));
    }
    let Some(mut cred) = store.credential(&r.name)? else {
        return Ok((None, AuthUsed::None));
    };
    if cred.is_expired() && cred.can_refresh() {
        cred = refresh_and_save(store, &r.name, &cred, opts, timeout)?;
    }
    Ok((cred.access_token, AuthUsed::Saved))
}

fn http_transport(
    r: &Resolved,
    token: Option<String>,
    opts: &Options,
    timeout: Duration,
) -> HttpTransport {
    let mut b = HttpTransport::builder(r.config.http.clone().unwrap_or_default())
        .token(token)
        .timeout(timeout)
        .user_agent(opts.user_agent.clone())
        .retry(opts.retry);
    for (k, v) in &r.config.headers {
        b = b.header(k.clone(), v.clone());
    }
    for (k, v) in &opts.extra_headers {
        b = b.header(k.clone(), v.clone());
    }
    if let Some(log) = opts.logger(&r.name) {
        b = b.log(log);
    }
    b.build()
}

/// The revision the caller named: the flag, else the one saved with the server,
/// else `None` to let the session work out which era the server speaks. A saved
/// value this build does not know is a config error, since the file was edited to
/// say something we cannot send.
fn pinned_version(r: &Resolved, opts: &Options) -> Result<Option<KnownVersion>> {
    if let Some(pinned) = opts.protocol_version {
        return Ok(Some(pinned));
    }
    match &r.config.protocol_version {
        Some(saved) => saved
            .parse()
            .map(Some)
            .map_err(|e| Error::config(format!("{}: protocol_version {e}", r.name))),
        None => Ok(None),
    }
}

/// `initialize`, with whatever can answer the server back installed first: the
/// capability is declared in the same breath as the handler that honours it,
/// and only on a transport that took one. Streamable HTTP takes none - the
/// question arrives on the response stream, but the reply would need a second
/// POST while the first is still open - so nothing is promised to a server
/// that could only be left waiting.
fn handshake(
    r: &Resolved,
    transport: impl Transport + 'static,
    auth: AuthUsed,
    pinned: Option<KnownVersion>,
    opts: &Options,
) -> Result<Connection> {
    let mut transport = Box::new(transport) as Box<dyn Transport>;
    let elicit = Answers::new(opts.elicit.answers.clone());
    let answering = transport.answer_requests(elicit::responder(&opts.elicit, elicit.clone()));
    let declared = match answering {
        true => opts.elicit.capabilities(),
        false => json!({}),
    };
    let mut session = Session::new(transport).declaring(declared);
    if let Some(version) = pinned {
        session = session.offering(version);
    }
    let server_info = session.open()?.clone();
    if let Some(level) = opts.log_level {
        set_log_level(&mut session, &server_info, level);
    }
    Ok(Connection {
        name: r.name.clone(),
        config: r.config.clone(),
        session,
        server_info,
        auth,
        elicit,
    })
}

/// Tell a server that advertises `logging` which levels are worth sending, so
/// that the filtering happens where the bandwidth is rather than here.
///
/// Best effort by design: the client filters what arrives whatever the server
/// does with this, so a server that advertised the capability and then refused
/// the request has cost the call nothing and must not fail it.
fn set_log_level(session: &mut Session<Box<dyn Transport>>, server_info: &Value, level: Level) {
    if !server_info["capabilities"]["logging"].is_object() {
        return;
    }
    let _ = session.request("logging/setLevel", Some(json!({ "level": level.as_str() })));
}

/// The process behind a stdio server, spawned and ready for `initialize`.
fn spawn_stdio(r: &Resolved, opts: &Options) -> Result<StdioTransport> {
    let cmd = r
        .config
        .stdio
        .as_deref()
        .ok_or_else(|| Error::usage(format!("{} is not a stdio server", r.name)))?;
    let argv = crate::transport::stdio::split_command(cmd)?;
    let env: Vec<(String, String)> = r
        .config
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    StdioTransport::spawn_with(
        &argv,
        &env,
        r.config.cwd.as_deref().map(std::path::Path::new),
        opts.timeout_for(r)?,
        opts.verbose,
        opts.logger(&r.name),
    )
}

/// A stdio server's process with its session opened, on the revision the flag or
/// the config asks for or the one the server turns out to speak: what a daemon
/// holds on behalf of its callers.
pub fn open_stdio(r: &Resolved, opts: &Options) -> Result<Session<StdioTransport>> {
    let r = r.dialed()?;
    let mut session = Session::new(spawn_stdio(&r, opts)?).declaring(Elicit::relayed());
    if let Some(version) = pinned_version(&r, opts)? {
        session = session.offering(version);
    }
    session.open()?;
    Ok(session)
}

/// Open a session and complete the `initialize` handshake.
///
/// A `${VAR}` in the config's headers, env, cwd, URL or command line is read from
/// the environment first; one that is unset is a config error before anything is
/// sent. A saved stdio server with a daemon running (`mcpdial start`) is reached
/// through the daemon's socket, so the session is the one it holds open. For HTTP
/// servers with a saved OAuth credential, a 401 triggers one refresh and retry
/// before giving up, so an expired token that the clock did not predict still
/// works without a visible hiccup.
pub fn connect(store: &Store, r: &Resolved, opts: &Options) -> Result<Connection> {
    let dialed = r.dialed()?;
    let r = &dialed;
    let pinned = pinned_version(r, opts)?;
    let timeout = opts.timeout_for(r)?;
    if r.config.stdio.is_some() {
        #[cfg(unix)]
        if r.saved && !opts.no_daemon {
            if let Some(t) = crate::daemon::attach(store, &r.name, timeout, opts.logger(&r.name))? {
                return handshake(r, t, AuthUsed::None, pinned, opts);
            }
        }
        return handshake(r, spawn_stdio(r, opts)?, AuthUsed::None, pinned, opts);
    }

    let (token, auth) = select_token(store, r, opts, timeout)?;
    match handshake(
        r,
        http_transport(r, token, opts, timeout),
        auth,
        pinned,
        opts,
    ) {
        Err(e) if e.is_auth_challenge() && auth == AuthUsed::Saved => {
            let cred = store.credential(&r.name)?.unwrap_or_default();
            if !cred.can_refresh() {
                return Err(e);
            }
            let cred = refresh_and_save(store, &r.name, &cred, opts, timeout)?;
            handshake(
                r,
                http_transport(r, cred.access_token, opts, timeout),
                auth,
                pinned,
                opts,
            )
        }
        outcome => outcome,
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
    let had_credential = had_credential(store, r, opts);

    match connect(store, r, opts) {
        Ok(mut conn) => {
            p.auth = conn.auth;
            let si = &conn.server_info["serverInfo"];
            p.server = si["name"].as_str().map(|n| match si["version"].as_str() {
                Some(v) if !v.is_empty() => format!("{n} {v}"),
                _ => n.to_string(),
            });
            if with_tools {
                match conn.list_tools() {
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

/// How a dial of `r` that went wrong reads as a status, the same reading `ls`
/// gives it. Anything that visits every saved server at once needs it: one
/// server that is down, or wants a token nobody saved, is a row of its own to
/// report rather than the end of the whole errand.
pub fn status_of(store: &Store, r: &Resolved, opts: &Options, e: &Error) -> Status {
    classify(e, had_credential(store, r, opts))
}

/// Whether anything was offered to the server as proof of identity, which is
/// what tells a refusal that wants a token from one that rejected ours.
fn had_credential(store: &Store, r: &Resolved, opts: &Options) -> bool {
    r.config.token_env.is_some()
        || opts.token_env.is_some()
        || store
            .credential(&r.name)
            .ok()
            .flatten()
            .is_some_and(|c| c.has_token())
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
    servers: Vec<(String, ServerConfig)>,
    opts: &Options,
    with_tools: bool,
) -> Vec<Probe> {
    let quiet = Options {
        verbose: false,
        ..opts.clone()
    };
    let opts = &quiet;
    let mut results: Vec<Option<Probe>> = vec![None; servers.len()];
    std::thread::scope(|scope| {
        for (slot, (name, config)) in results.iter_mut().zip(servers) {
            let r = Resolved {
                name,
                config,
                saved: true,
            };
            scope.spawn(move || *slot = Some(probe(store, &r, opts, with_tools)));
        }
    });
    results.into_iter().flatten().collect()
}

/// Probe every configured server concurrently, in config order.
pub fn probe_all(store: &Store, opts: &Options, with_tools: bool) -> Result<Vec<Probe>> {
    Ok(probe_each(
        store,
        store.servers()?.into_iter().collect(),
        opts,
        with_tools,
    ))
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
    /// The variable a token was read from, when the row's `auth` is
    /// [`AuthUsed::Env`]. Never serialized: an `ls --json` row already carries
    /// `token_env` from what was saved, and this only names the variable in
    /// the AUTH column that `--no-probe` already names it in.
    #[serde(skip)]
    pub token_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<usize>,
    pub checked_at: u64,
    pub age_seconds: u64,
    /// A daemon started with `mcpdial start` is listening for this server.
    /// Always looked up live, whatever the rest of the row remembers.
    pub running: bool,
}

impl Listing {
    fn probed(p: Probe, token_env: Option<String>, checked_at: u64) -> Self {
        Self {
            name: p.name,
            kind: p.kind,
            location: p.location,
            status: p.status,
            auth: p.auth,
            token_env,
            server: p.server,
            tools: p.tools.as_ref().map(Vec::len),
            checked_at,
            age_seconds: 0,
            running: false,
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
            token_env: cfg.token_env.clone(),
            server: rec.server.clone(),
            tools: rec.tools,
            checked_at: rec.checked_at,
            age_seconds: now.saturating_sub(rec.checked_at),
            running: false,
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

fn token_env_of(servers: &BTreeMap<String, ServerConfig>, name: &str) -> Option<String> {
    servers.get(name).and_then(|cfg| cfg.token_env.clone())
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

    let fresh: Vec<Listing> = probe_each(store, cold, &opts.for_status(), true)
        .into_iter()
        .map(|p| {
            let token_env = token_env_of(&servers, &p.name);
            Listing::probed(p, token_env, now)
        })
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
    for row in rows.values_mut() {
        row.running = crate::daemon::is_running(store, &row.name);
    }
    Ok(rows.into_values().collect())
}

/// One saved server's row, dialed now and remembered, so that `add` can say
/// what `ls` would without dialing every other server.
pub fn listing_one(store: &Store, opts: &Options, name: &str) -> Result<Listing> {
    Ok(
        listing_named(store, opts, std::slice::from_ref(&name.to_string()))?
            .pop()
            .expect("one row per name"),
    )
}

/// The rows for some saved servers, dialed together now and remembered, in the
/// order named: what `browse` shows for what it just saved.
pub fn listing_named(store: &Store, opts: &Options, names: &[String]) -> Result<Vec<Listing>> {
    let servers = store.servers()?;
    let keys = probe_keys(store, &servers);
    let now = now();
    let chosen = names
        .iter()
        .map(|name| {
            servers
                .get(name)
                .cloned()
                .map(|cfg| (name.clone(), cfg))
                .ok_or_else(|| Error::usage(format!("no server named {name:?}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let rows: Vec<Listing> = probe_each(store, chosen, &opts.for_status(), true)
        .into_iter()
        .map(|p| {
            let token_env = token_env_of(&servers, &p.name);
            Listing::probed(p, token_env, now)
        })
        .collect();
    let _ = store.save_probes(
        rows.iter()
            .map(|row| (row.name.clone(), row.record(keys[&row.name])))
            .collect(),
    );
    Ok(rows)
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
            let mut line = format!("{name}: {}", schema::type_name(spec));
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

/// The hints a tool's `annotations` set, as the words a listing tags it with.
///
/// Only a hint that is present and true is named. The spec's default for
/// `destructiveHint` is true, so a tool that annotates nothing is a tool that
/// said nothing, which must never be read as a promise that a call is safe.
pub fn describe_annotations(tool: &Value) -> Vec<&'static str> {
    let mut tags: Vec<&'static str> = [
        ("readOnlyHint", "read-only"),
        ("destructiveHint", "destructive"),
        ("idempotentHint", "idempotent"),
        ("openWorldHint", "open-world"),
    ]
    .into_iter()
    .filter(|(hint, _)| tool["annotations"][hint] == true)
    .map(|(_, tag)| tag)
    .collect();
    // A tool a server can run as a task is one a caller may start and leave; one
    // that only runs as a task cannot be called any other way.
    match tool["execution"]["taskSupport"].as_str() {
        Some("optional") => tags.push("task:optional"),
        Some("required") => tags.push("task:required"),
        _ => {}
    }
    tags
}

/// The name a server gave a tool for a person to read, where it says more than
/// the tool's own name does. `title` is where protocol 2025-06-18 put it;
/// servers written against 2025-03-26 put the same string in `annotations`.
pub fn describe_title(tool: &Value) -> Option<&str> {
    let title = tool["title"]
        .as_str()
        .or_else(|| tool["annotations"]["title"].as_str())?
        .trim();
    (!title.is_empty() && Some(title) != tool["name"].as_str()).then_some(title)
}

/// What a `--long` listing writes after a tool's name: its title, then a tag per
/// hint. Empty for a tool that says nothing about itself, and for every prompt.
pub fn hint_tags(tool: &Value) -> String {
    let titled = describe_title(tool).map(|t| format!("  {t:?}"));
    let tags = describe_annotations(tool);
    let tagged = (!tags.is_empty()).then(|| format!("  [{}]", tags.join("] [")));
    format!(
        "{}{}",
        titled.unwrap_or_default(),
        tagged.unwrap_or_default()
    )
}

/// What the short listing writes after a tool's name, having room for one hint
/// only: the tool a caller must not try blind wears a `*`.
pub fn hint_mark(tool: &Value) -> String {
    let destroys_something = tool["annotations"]["destructiveHint"] == true;
    if destroys_something {
        "*".into()
    } else {
        String::new()
    }
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
            format!("{}: {}", json!(name), schema::json_placeholder(spec))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
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
    fn a_probe_waits_less_than_a_call_unless_told_how_long() {
        let server = |saved: Option<f64>| Resolved {
            name: "x".into(),
            config: ServerConfig {
                timeout: saved,
                ..ServerConfig::http("https://x/mcp")
            },
            saved: true,
        };
        let secs = Duration::from_secs;

        let unspecified = Options::default();
        assert!(PROBE_TIMEOUT < DEFAULT_TIMEOUT);
        assert_eq!(
            unspecified.timeout_for(&server(None)).unwrap(),
            DEFAULT_TIMEOUT
        );
        assert_eq!(unspecified.timeout_or_default(), DEFAULT_TIMEOUT);
        let probing = unspecified.for_status();
        assert_eq!(probing.timeout_for(&server(None)).unwrap(), PROBE_TIMEOUT);
        assert_eq!(probing.timeout_or_default(), PROBE_TIMEOUT);

        // A server that says how long it needs is believed by a probe too.
        assert_eq!(
            probing.timeout_for(&server(Some(120.0))).unwrap(),
            secs(120)
        );

        // An explicit --timeout means it, in both directions, for a probe and a call.
        for given in [3, 90] {
            let told = Options {
                timeout: Some(secs(given)),
                ..Options::default()
            };
            assert_eq!(told.timeout_for(&server(None)).unwrap(), secs(given));
            let probing = told.for_status();
            assert_eq!(probing.timeout_for(&server(None)).unwrap(), secs(given));
            assert_eq!(
                probing.timeout_for(&server(Some(120.0))).unwrap(),
                secs(given)
            );
        }
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
    fn the_lists_filter_a_listing_and_mark_the_hidden_ones() {
        let tools = || {
            vec![
                json!({"name": "read_file"}),
                json!({"name": "list_directory"}),
                json!({"name": "write_file"}),
                json!({"name": "delete_file"}),
                json!({"description": "nameless"}),
            ]
        };
        let open = ServerConfig::stdio("fs");
        assert_eq!(
            permitted_tools(tools(), &open),
            tools(),
            "no lists, no change"
        );
        assert_eq!(marked_tools(tools(), &open), tools());

        let mut fenced = ServerConfig::stdio("fs");
        fenced.allow = vec!["read_*".into(), "list_directory".into()];
        fenced.deny = vec!["delete_*".into()];
        let names = |v: Vec<Value>| -> Vec<String> {
            v.iter()
                .map(|t| t["name"].as_str().unwrap_or("").to_string())
                .collect()
        };
        assert_eq!(
            names(permitted_tools(tools(), &fenced)),
            ["read_file", "list_directory"],
            "order is the server's"
        );
        let marked = marked_tools(tools(), &fenced);
        assert_eq!(marked.len(), 5, "--all keeps every tool");
        assert_eq!(
            marked[0].get("denied"),
            None,
            "a permitted tool is untouched"
        );
        assert_eq!(marked[2]["denied"], true, "missed the allow list");
        assert_eq!(marked[3]["denied"], true, "hit the deny list");
        assert_eq!(marked[4]["denied"], true, "a nameless tool matches nothing");

        let mut deny_only = ServerConfig::stdio("fs");
        deny_only.deny = vec!["delete_*".into()];
        assert_eq!(
            names(permitted_tools(tools(), &deny_only)),
            ["read_file", "list_directory", "write_file", ""]
        );
    }

    #[test]
    fn describes_parameters() {
        let tool = json!({"name":"add","inputSchema":{"type":"object","properties":{
            "a":{"type":"number","description":"First\nsecond line"},
            "b":{"type":["number","null"]},
            "mode":{"enum":["x","y"]},
            "repoName":{"anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]},
            "pages":{"type":"array","items":{"type":"object"}},
            "at":{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"string"}},
                  "required":["x"]}
        },"required":["a"]}});
        // A parameter with more than one shape says both of them, a container
        // says what it holds, and an object names its fields one level in.
        assert_eq!(
            describe_params(&tool),
            [
                "a: number (required) - First",
                "at: object {x: number, y?: string}",
                "b: number|null",
                "mode: enum",
                "pages: object[]",
                "repoName: string|string[]",
            ]
        );
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
    fn describes_what_a_tool_says_about_itself() {
        let every = json!({"name":"wipe","title":"Wipe the disk","annotations":{
            "readOnlyHint":true,"destructiveHint":true,"idempotentHint":true,
            "openWorldHint":true},"execution":{"taskSupport":"optional"}});
        assert_eq!(
            describe_annotations(&every),
            [
                "read-only",
                "destructive",
                "idempotent",
                "open-world",
                "task:optional"
            ]
        );
        assert_eq!(
            hint_tags(&every),
            r#"  "Wipe the disk"  [read-only] [destructive] [idempotent] [open-world] [task:optional]"#
        );
        assert_eq!(hint_mark(&every), "*");

        // A tool that annotated nothing is tagged with nothing: the spec's
        // default for destructiveHint is true, so silence is not safety.
        let quiet = json!({"name":"add"});
        assert!(describe_annotations(&quiet).is_empty());
        assert_eq!(hint_tags(&quiet), "");
        assert_eq!(hint_mark(&quiet), "");

        // A hint that is present and false says the opposite of its tag.
        let safe =
            json!({"name":"read","annotations":{"destructiveHint":false,"readOnlyHint":true}});
        assert_eq!(describe_annotations(&safe), ["read-only"]);
        assert_eq!(hint_mark(&safe), "");

        // A title that only repeats the name has nothing to add.
        let same = json!({"name":"erase","title":"erase","annotations":{"destructiveHint":true}});
        assert_eq!(hint_tags(&same), "  [destructive]");
        assert_eq!(describe_title(&same), None);
        // The place an older server puts the same string is read too.
        let legacy = json!({"name":"erase","annotations":{"title":"Erase a file"}});
        assert_eq!(hint_tags(&legacy), r#"  "Erase a file""#);

        // A tool that only runs as a task cannot be called any other way, and
        // anything else the field could say is not a tag.
        let required = json!({"name":"render","execution":{"taskSupport":"required"}});
        assert_eq!(describe_annotations(&required), ["task:required"]);
        let forbidden = json!({"name":"render","execution":{"taskSupport":"forbidden"}});
        assert!(describe_annotations(&forbidden).is_empty());
    }

    #[test]
    fn examples_show_the_required_arguments() {
        let tool = json!({"name":"new_page","inputSchema":{"type":"object","properties":{
            "url":{"type":"string"},
            "timeout":{"type":"number"},
            "mode":{"enum":["fast","slow"]},
            "flag":{"type":"boolean"},
            "repoName":{"anyOf":[{"type":"array","items":{"type":"string"}},{"type":"string"}]},
            "at":{"type":"object","properties":{"x":{"type":"number"}}}
        },"required":["url","mode","flag","repoName","at"]}});
        assert_eq!(
            example_arguments(&tool),
            r#"{"url": "<string>", "mode": "fast"|"slow", "flag": true|false, "repoName": [...], "at": {...}}"#
        );
        // Nothing required means the empty object is already a complete call.
        assert_eq!(
            example_arguments(&json!({"name":"count","inputSchema":{"properties":{}}})),
            "{}"
        );
        assert_eq!(example_arguments(&json!({"name":"x"})), "{}");
    }

    #[test]
    fn a_remembered_status_round_trips_and_an_unreadable_one_is_a_miss() {
        let cfg = ServerConfig::http("https://x/mcp");
        let taken = Listing::probed(
            Probe {
                name: "x".into(),
                kind: cfg.kind(),
                location: cfg.location().to_string(),
                status: Status::Unreachable {
                    detail: "no route".into(),
                },
                auth: AuthUsed::Saved,
                server: Some("fake 1.0".into()),
                tools: Some(vec![json!({"name": "echo"})]),
            },
            None,
            100,
        );

        let record = taken.record(7);
        let back = Listing::remembered("x", &cfg, &record, 160).unwrap();
        assert_eq!(back.status, taken.status);
        assert_eq!(back.auth, AuthUsed::Saved);
        assert_eq!(back.server.as_deref(), Some("fake 1.0"));
        assert_eq!(back.tools, Some(1));
        assert_eq!(back.checked_at, 100);
        assert_eq!(back.age_seconds, 60);
        assert!(still_current(&record, 7, 100 + STATUS_TTL.as_secs() - 1));
        assert!(!still_current(&record, 7, 100 + STATUS_TTL.as_secs()));
        assert!(!still_current(&record, 8, 160), "a different server");

        let from_a_later_build = ProbeRecord {
            status: json!({"state": "a state this build has never heard of"}),
            ..record
        };
        assert!(Listing::remembered("x", &cfg, &from_a_later_build, 160).is_none());
    }
}
