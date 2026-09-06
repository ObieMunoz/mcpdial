//! Expose one saved server without handing over what authorizes it.
//!
//! The human logs in once, here; the agent runs somewhere that must never hold the
//! token. `serve` opens the upstream session with the saved credential and speaks
//! plain MCP to whoever connects, over loopback HTTP or over its own stdin and
//! stdout. Nothing crosses the middle: the upstream `Authorization` and any saved
//! headers are added on this side, a client's headers are read for `--bearer-env`
//! and dropped, and every reply the client sees is built here from the JSON-RPC
//! result alone.

use crate::client::{self, Connection, Options, Resolved};
use crate::config::{ServerConfig, Store};
use crate::protocol::{reply, reply_error, Error, Result, CLIENT_NAME, CLIENT_VERSION};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

const INVALID_REQUEST: i64 = -32600;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const PARSE_ERROR: i64 = -32700;

/// What Streamable HTTP has a server say to a request that names no session of its.
const NO_SESSION: &str = "Bad Request: No valid session ID provided";

/// One upstream session per client session, and an upstream stdio server is a
/// process apiece: a client that opens sessions and never ends them would
/// otherwise spawn without limit.
const MAX_SESSIONS: usize = 16;

/// How long the accept loop sleeps between tries, which is also how long ^C waits.
const POLL: Duration = Duration::from_millis(20);

/// How long a client connection may say nothing before it is dropped.
const IDLE: Duration = Duration::from_secs(300);

const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY: usize = 16 * 1024 * 1024;

/// What `serve` was asked for, straight off the command line.
pub struct Settings {
    /// Where to listen, unless `stdio` is set. Port 0 picks a free one.
    pub listen: String,
    /// Speak MCP on this process's own stdin and stdout instead of listening.
    pub stdio: bool,
    /// Permission to listen somewhere other than loopback.
    pub listen_any: bool,
    /// Environment variable holding the bearer token clients must present.
    pub bearer_env: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub json: bool,
}

/// Serve `target` until stdin ends or ^C arrives. Returns the process exit code.
pub fn run(store: Store, opts: Options, target: &str, settings: Settings) -> Result<u8> {
    let resolved = client::resolve(&store, target)?;
    let bearer = match &settings.bearer_env {
        Some(var) => Some(shared_secret(var)?),
        None => None,
    };
    let verbose = opts.verbose;
    let filter = ToolFilter::new(&resolved.config, &settings.allow, &settings.deny);
    if settings.stdio {
        serve_stdio(Proxy::new(store, opts, resolved, filter), verbose)
    } else {
        serve_http(store, opts, resolved, filter, &settings, bearer)
    }
}

fn shared_secret(var: &str) -> Result<String> {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::usage(format!(
                "${var} is unset or empty.\n       export {var}=... first, and give clients the \
                 same value (don't pass a token as an argument)."
            ))
        })
}

// -- the proxy ----------------------------------------------------------------------

/// One client session, and the upstream session it stands for.
struct Upstream {
    connection: Connection,
    used: Instant,
}

/// The upstream sessions and everything needed to open another. Lives on one
/// thread: requests are serialised, which is how both transports work anyway.
struct Proxy {
    store: Store,
    opts: Options,
    resolved: Resolved,
    filter: ToolFilter,
    sessions: HashMap<String, Upstream>,
}

/// What the proxy makes of one client message, before a transport decides how to
/// deliver it.
enum Answer {
    /// A notification: nothing is owed.
    Nothing,
    Message(Value),
    /// The reply to `initialize`, and the session id minted for it.
    Opened {
        id: String,
        message: Value,
    },
    /// The client named a session this proxy did not mint, or named none.
    NoSession(Value),
}

impl Proxy {
    fn new(store: Store, opts: Options, resolved: Resolved, filter: ToolFilter) -> Self {
        Self {
            store,
            opts,
            resolved,
            filter,
            sessions: HashMap::new(),
        }
    }

    fn handle(&mut self, session: Option<&str>, message: &Value) -> Answer {
        if !message.is_object() {
            return Answer::Message(reply_error(
                &Value::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC object; batches are not relayed",
            ));
        }
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let params = message.get("params").cloned();

        let Some(id) = message.get("id").filter(|v| !v.is_null()).cloned() else {
            // A notification. `initialized` belongs to the handshake this proxy
            // completed upstream on its own; the rest are passed on.
            if method != "notifications/initialized" {
                if let Some(up) = self.session_mut(session) {
                    let _ = up.connection.session.notify(&method, params);
                }
            }
            return Answer::Nothing;
        };

        match method.as_str() {
            "initialize" => return self.open(&id),
            // Answered here because the client is asking whether *this* is alive.
            "ping" => return Answer::Message(reply(&id, json!({}))),
            "tools/call" => {
                let tool = message["params"]["name"].as_str().unwrap_or_default();
                if !self.filter.permits(tool) {
                    return Answer::Message(reply_error(
                        &id,
                        INVALID_PARAMS,
                        &format!("Tool {tool} is not exposed by this proxy"),
                    ));
                }
            }
            _ => {}
        }

        let Some(up) = self.session_mut(session) else {
            return Answer::NoSession(reply_error(&id, INVALID_REQUEST, NO_SESSION));
        };
        let mut result = match up.connection.session.request(&method, params) {
            Ok(result) => result,
            Err(e) => return Answer::Message(upstream_error(&id, e)),
        };
        if method == "tools/list" {
            self.filter.retain(&mut result);
        }
        Answer::Message(reply(&id, result))
    }

    /// Open the upstream session this client session stands for, and answer with
    /// mcpdial's own identity over the upstream's capabilities.
    fn open(&mut self, id: &Value) -> Answer {
        self.make_room();
        let connection = match client::connect(&self.store, &self.resolved, &self.opts) {
            Ok(c) => c,
            Err(e) => {
                return Answer::Message(reply_error(
                    id,
                    INTERNAL_ERROR,
                    &format!(
                        "mcpdial could not open a session with {}: {e}",
                        self.resolved.name
                    ),
                ))
            }
        };
        let result = proxied_initialize(&connection);
        let session_id = match mint_session_id() {
            Ok(id) => id,
            Err(e) => return Answer::Message(upstream_error(id, e)),
        };
        self.sessions.insert(
            session_id.clone(),
            Upstream {
                connection,
                used: Instant::now(),
            },
        );
        Answer::Opened {
            id: session_id,
            message: reply(id, result),
        }
    }

    /// End one client session. Dropping it terminates the upstream session too.
    fn close(&mut self, session: Option<&str>) -> Answer {
        match session.and_then(|id| self.sessions.remove(id)) {
            Some(_ended) => Answer::Nothing,
            None => Answer::NoSession(reply_error(&Value::Null, INVALID_REQUEST, NO_SESSION)),
        }
    }

    fn session_mut(&mut self, session: Option<&str>) -> Option<&mut Upstream> {
        let up = self.sessions.get_mut(session?)?;
        up.used = Instant::now();
        Some(up)
    }

    /// Make room for one more session by ending the stalest. Its client gets
    /// [`NO_SESSION`] on its next request and can initialize again.
    fn make_room(&mut self) {
        while self.sessions.len() >= MAX_SESSIONS {
            let stalest = self
                .sessions
                .iter()
                .min_by_key(|(_, up)| up.used)
                .map(|(id, _)| id.clone());
            match stalest {
                Some(id) => drop(self.sessions.remove(&id)),
                None => break,
            }
        }
    }
}

/// The `initialize` result a client of the proxy gets: this proxy's identity, and
/// what of the upstream's capabilities survives the trip.
fn proxied_initialize(conn: &Connection) -> Value {
    let upstream = conn.server_info["serverInfo"]["name"]
        .as_str()
        .unwrap_or("an upstream server");
    let mut result = json!({
        "protocolVersion": conn.session.version().as_str(),
        "capabilities": relayable(&conn.server_info["capabilities"]),
        "serverInfo": {
            "name": CLIENT_NAME,
            "version": CLIENT_VERSION,
            "title": format!("{CLIENT_NAME} serving {upstream}"),
        },
    });
    if let Some(instructions) = conn.server_info.get("instructions") {
        result["instructions"] = instructions.clone();
    }
    result
}

/// The upstream's capabilities minus everything that only works if the server can
/// speak first. This proxy answers requests and has no channel back to the client
/// to carry a notification or a server-initiated request, so promising `logging`
/// or a `listChanged` that never arrives would be a lie the client acts on.
fn relayable(capabilities: &Value) -> Value {
    let mut caps = capabilities.clone();
    let Some(obj) = caps.as_object_mut() else {
        return json!({});
    };
    obj.remove("logging");
    for feature in ["tools", "resources", "prompts"] {
        if let Some(f) = obj.get_mut(feature).and_then(Value::as_object_mut) {
            f.remove("listChanged");
            f.remove("subscribe");
        }
    }
    caps
}

/// A failed upstream request, as the JSON-RPC error the client gets. A refusal the
/// server itself worded is passed through code and all; anything else is ours.
fn upstream_error(id: &Value, e: Error) -> Value {
    match e {
        Error::Rpc {
            code,
            message,
            data,
        } => {
            let mut m = reply_error(id, code, &message);
            if let Some(data) = data {
                m["error"]["data"] = data;
            }
            m
        }
        other => reply_error(id, INTERNAL_ERROR, &format!("upstream: {other}")),
    }
}

/// A session id the client echoes back: 128 bits, so one client cannot guess
/// another's and step into its upstream session.
fn mint_session_id() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| Error::transport(format!("no entropy source: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

// -- tool filtering -----------------------------------------------------------------

/// Which tools this proxy admits to having: the lists saved with the server, which
/// hold wherever it is dialed, and the ones this invocation added on top. Both
/// decide by [`ServerConfig::denial`] - deny beats allow, and an empty allow list
/// means everything not denied - so a proxy filters exactly as a direct call does.
struct ToolFilter {
    saved: ServerConfig,
    flags: ServerConfig,
}

impl ToolFilter {
    fn new(saved: &ServerConfig, allow: &[String], deny: &[String]) -> Self {
        Self {
            saved: saved.clone(),
            flags: ServerConfig {
                allow: allow.to_vec(),
                deny: deny.to_vec(),
                ..ServerConfig::default()
            },
        }
    }

    fn permits(&self, tool: &str) -> bool {
        self.saved.denial(tool).is_none() && self.flags.denial(tool).is_none()
    }

    /// Drop what the client may not see from a `tools/list` result, leaving any
    /// `nextCursor` alone so pagination still walks the whole list.
    fn retain(&self, result: &mut Value) {
        if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
            tools.retain(|t| self.permits(t["name"].as_str().unwrap_or_default()));
        }
    }
}

// -- stdio --------------------------------------------------------------------------

/// MCP on this process's own stdin and stdout, so a host application can list
/// `mcpdial serve NAME --stdio` and reach a server it has no credential for.
/// Nothing but protocol may go to stdout, so there is no receipt here.
fn serve_stdio(mut proxy: Proxy, verbose: bool) -> Result<u8> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    let mut session: Option<String> = None;

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Error::transport(format!("could not read stdin: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        if verbose {
            eprintln!("<- {line}");
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            let complaint = reply_error(&Value::Null, PARSE_ERROR, "message is not JSON");
            write_message(&mut out, &complaint, verbose)?;
            continue;
        };
        match proxy.handle(session.as_deref(), &message) {
            Answer::Nothing => {}
            Answer::Message(m) | Answer::NoSession(m) => write_message(&mut out, &m, verbose)?,
            Answer::Opened { id, message } => {
                session = Some(id);
                write_message(&mut out, &message, verbose)?;
            }
        }
    }
    Ok(0)
}

fn write_message(out: &mut impl Write, message: &Value, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!("-> {message}");
    }
    writeln!(out, "{message}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::transport(format!("could not write to stdout: {e}")))
}

// -- http ---------------------------------------------------------------------------

/// What a connection thread asks the one thread that owns the upstream sessions.
enum Job {
    Message {
        session: Option<String>,
        message: Value,
        reply: Sender<Answer>,
    },
    Close {
        session: Option<String>,
        reply: Sender<Answer>,
    },
    Shutdown,
}

fn serve_http(
    store: Store,
    opts: Options,
    resolved: Resolved,
    filter: ToolFilter,
    settings: &Settings,
    bearer: Option<String>,
) -> Result<u8> {
    let verbose = opts.verbose;
    let wanted = listen_addr(&settings.listen, settings.listen_any)?;
    let listener = TcpListener::bind(wanted)
        .map_err(|e| Error::transport(format!("could not listen on {wanted}: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::transport(format!("could not read back the listening port: {e}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::transport(format!("could not poll the listening socket: {e}")))?;

    announce(&resolved.name, addr, settings);
    interrupt::watch();

    // The sessions live on one thread and never cross to another: a `Session` is
    // not `Send`, and serialising what reaches the upstream server is what both
    // transports do anyway.
    let (jobs, inbox) = mpsc::channel();
    let upstream = thread::spawn(move || {
        let mut proxy = Proxy::new(store, opts, resolved, filter);
        while let Ok(job) = inbox.recv() {
            match job {
                Job::Message {
                    session,
                    message,
                    reply,
                } => {
                    let _ = reply.send(proxy.handle(session.as_deref(), &message));
                }
                Job::Close { session, reply } => {
                    let _ = reply.send(proxy.close(session.as_deref()));
                }
                Job::Shutdown => break,
            }
        }
        // Every upstream session is terminated here, on the way out.
    });

    while !interrupt::requested() {
        match listener.accept() {
            Ok((stream, _)) => {
                let jobs = jobs.clone();
                let bearer = bearer.clone();
                thread::spawn(move || serve_connection(stream, jobs, bearer, verbose));
            }
            // Nothing waiting, or a connection that died in the queue: either way
            // the next thing to do is look again, and the pause is what makes an
            // interrupt take effect within [`POLL`] rather than at the next client.
            Err(_) => thread::sleep(POLL),
        }
    }

    let _ = jobs.send(Job::Shutdown);
    let _ = upstream.join();
    Ok(0)
}

/// Where to listen. A proxy that is reachable from the network lends this
/// server's credentials to whoever finds the port, so that takes saying so.
fn listen_addr(listen: &str, any: bool) -> Result<SocketAddr> {
    let addr = listen
        .to_socket_addrs()
        .map_err(|e| Error::usage(format!("--listen {listen}: {e}")))?
        .next()
        .ok_or_else(|| Error::usage(format!("--listen {listen} names no address")))?;
    if !addr.ip().is_loopback() && !any {
        return Err(Error::usage(format!(
            "--listen {listen} is not a loopback address, and anyone who can reach it could \
             use this server's credentials.\n       Pass --listen-any if that is what you meant."
        )));
    }
    Ok(addr)
}

fn announce(name: &str, addr: SocketAddr, settings: &Settings) {
    let url = format!("http://{addr}/mcp");
    let exposed = (!addr.ip().is_loopback()).then(|| {
        format!("{url} is reachable beyond this machine; anyone who finds it can use {name}")
    });
    if settings.json {
        let mut receipt = json!({"serve": {
            "name": name,
            "url": url,
            "address": addr.to_string(),
            "bearer_env": settings.bearer_env,
            "allow": settings.allow,
            "deny": settings.deny,
        }});
        if let Some(warning) = exposed {
            receipt["serve"]["warning"] = json!(warning);
        }
        println!("{receipt}");
        let _ = io::stdout().flush();
    } else {
        if let Some(warning) = exposed {
            eprintln!("warning: {warning}");
        }
        eprintln!("serving {name} on {url}");
        if let Some(var) = &settings.bearer_env {
            eprintln!("clients must send Authorization: Bearer ${var}");
        }
        eprintln!("^C to stop");
    }
}

fn serve_connection(stream: TcpStream, jobs: Sender<Job>, bearer: Option<String>, verbose: bool) {
    // Accepted from a non-blocking listener, and on some platforms non-blocking
    // with it: a socket that answers every read with WouldBlock reads nothing.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IDLE));
    let Ok(incoming) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(incoming);
    let mut writer = stream;

    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err((status, why)) => {
                let _ = respond(&mut writer, status, &fault(why), &[], false);
                return;
            }
        };
        if verbose {
            // Headers are never traced: one holds the client's token and, upstream,
            // another holds the one this proxy exists to keep to itself.
            eprintln!("<= {} {} {}", request.method, request.path, request.body);
        }
        let keep_alive = request.keeps_alive();

        if !authorized(&request, bearer.as_deref()) {
            let _ = respond(
                &mut writer,
                401,
                &fault("a bearer token is required"),
                &[],
                false,
            );
            return;
        }

        let written = match request.method.as_str() {
            "POST" => post(&mut writer, &request, &jobs, keep_alive, verbose),
            "DELETE" => delete(&mut writer, &request, &jobs, keep_alive),
            _ => respond(
                &mut writer,
                405,
                &fault("this endpoint takes POST and DELETE"),
                &[("Allow", "POST, DELETE".to_string())],
                keep_alive,
            ),
        };
        if written.is_err() || !keep_alive {
            return;
        }
    }
}

fn post(
    writer: &mut TcpStream,
    request: &Request,
    jobs: &Sender<Job>,
    keep_alive: bool,
    verbose: bool,
) -> io::Result<()> {
    let Ok(message) = serde_json::from_str::<Value>(&request.body) else {
        let complaint = reply_error(&Value::Null, PARSE_ERROR, "message is not JSON");
        return respond(writer, 400, &complaint.to_string(), &[], keep_alive);
    };
    let Some(answer) = ask(jobs, |reply| Job::Message {
        session: request.session(),
        message,
        reply,
    }) else {
        return shutting_down(writer);
    };

    let (status, body, extra) = match answer {
        Answer::Nothing => (202, String::new(), Vec::new()),
        Answer::Message(m) => (200, m.to_string(), Vec::new()),
        Answer::Opened { id, message } => (200, message.to_string(), vec![("Mcp-Session-Id", id)]),
        Answer::NoSession(m) => (400, m.to_string(), Vec::new()),
    };
    if verbose {
        eprintln!("=> {status} {body}");
    }
    respond(writer, status, &body, &extra, keep_alive)
}

fn delete(
    writer: &mut TcpStream,
    request: &Request,
    jobs: &Sender<Job>,
    keep_alive: bool,
) -> io::Result<()> {
    let session = request.session();
    let Some(answer) = ask(jobs, |reply| Job::Close { session, reply }) else {
        return shutting_down(writer);
    };
    match answer {
        Answer::NoSession(m) => respond(writer, 400, &m.to_string(), &[], keep_alive),
        _ => respond(writer, 204, "", &[], keep_alive),
    }
}

/// The thread that owns the sessions has gone, which happens only on the way out.
fn shutting_down(writer: &mut TcpStream) -> io::Result<()> {
    respond(
        writer,
        500,
        &fault("the proxy is shutting down"),
        &[],
        false,
    )
}

/// Put one job to the thread that owns the sessions and wait for its answer.
fn ask(jobs: &Sender<Job>, job: impl FnOnce(Sender<Answer>) -> Job) -> Option<Answer> {
    let (tx, rx) = mpsc::channel();
    jobs.send(job(tx)).ok()?;
    rx.recv().ok()
}

fn authorized(request: &Request, bearer: Option<&str>) -> bool {
    let Some(secret) = bearer else {
        return true;
    };
    presented(request).is_some_and(|token| same_secret(token, secret))
}

fn presented(request: &Request) -> Option<&str> {
    let (scheme, token) = request.header("authorization")?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim_start())
}

/// Compare without stopping at the first wrong byte: the token is a secret, and a
/// timing difference is how one gets guessed a character at a time.
fn same_secret(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |seen, (x, y)| seen | (x ^ y)) == 0
}

fn fault(why: &str) -> String {
    json!({ "error": why }).to_string()
}

// -- the little that is needed of HTTP/1.1 ------------------------------------------

struct Request {
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn session(&self) -> Option<String> {
        self.header("mcp-session-id").map(str::to_string)
    }

    fn keeps_alive(&self) -> bool {
        match self.header("connection") {
            Some(v) if v.eq_ignore_ascii_case("close") => false,
            Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
            _ => self.version != "HTTP/1.0",
        }
    }
}

/// The next request on this connection, or `None` once there will not be one.
/// Every limit here bounds what one client can make this process allocate.
fn read_request<R: BufRead>(
    r: &mut R,
) -> std::result::Result<Option<Request>, (u16, &'static str)> {
    let Some(start) = read_line(r)? else {
        return Ok(None);
    };
    if start.is_empty() {
        return Ok(None);
    }
    let mut words = start.split_whitespace();
    let (Some(method), Some(path)) = (words.next(), words.next()) else {
        return Err((400, "malformed request line"));
    };
    let version = words.next().unwrap_or("HTTP/1.1").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let Some(line) = read_line(r)? else {
            return Err((400, "the headers ended early"));
        };
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err((431, "too many headers"));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let length = match header("content-length") {
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| (400, "Content-Length is not a number"))?,
        None if header("transfer-encoding").is_some() => {
            return Err((411, "send a Content-Length; chunked bodies are not read"))
        }
        None => 0,
    };
    if length > MAX_BODY {
        return Err((413, "body too large"));
    }
    let mut body = vec![0u8; length];
    r.read_exact(&mut body)
        .map_err(|_| (400, "the body ended early"))?;

    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path: path.to_string(),
        version,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }))
}

fn read_line<R: BufRead>(r: &mut R) -> std::result::Result<Option<String>, (u16, &'static str)> {
    let mut line = String::new();
    match r.by_ref().take(MAX_LINE).read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) if !line.ends_with('\n') => Err((431, "a header line ran past the limit")),
        Ok(_) => Ok(Some(line.trim_end_matches(['\r', '\n']).to_string())),
        Err(e) if e.kind() == ErrorKind::InvalidData => Err((400, "the headers are not UTF-8")),
        // A read timeout or a reset: there is no next request on this connection.
        Err(_) => Ok(None),
    }
}

fn respond(
    w: &mut impl Write,
    status: u16,
    body: &str,
    extra: &[(&str, String)],
    keep_alive: bool,
) -> io::Result<()> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    // A 204 carries no body, and RFC 9110 has it carry no length either.
    if status != 204 {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Connection: {connection}\r\n\r\n"));

    w.write_all(head.as_bytes())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

// -- ^C -----------------------------------------------------------------------------

/// The interrupt, on the two platforms that can be told about one. The handler
/// does the least it can: the accept loop sees the flag, ends every upstream
/// session on the way out, and exits 0. A second interrupt is left to the default
/// disposition, so a proxy waiting on a slow upstream can still be killed.
mod interrupt {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    pub fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    #[cfg(unix)]
    pub fn watch() {
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        const SIG_DFL: usize = 0;

        extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }

        extern "C" fn note(signum: i32) {
            REQUESTED.store(true, Ordering::Relaxed);
            unsafe { signal(signum, SIG_DFL) };
        }

        let handler = note as extern "C" fn(i32) as usize;
        unsafe {
            signal(SIGINT, handler);
            signal(SIGTERM, handler);
        }
    }

    #[cfg(windows)]
    pub fn watch() {
        const HANDLED: i32 = 1;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<unsafe extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }

        unsafe extern "system" fn note(_event: u32) -> i32 {
            REQUESTED.store(true, Ordering::Relaxed);
            HANDLED
        }

        unsafe { SetConsoleCtrlHandler(Some(note), HANDLED) };
    }

    #[cfg(not(any(unix, windows)))]
    pub fn watch() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fenced(deny: &[&str]) -> ServerConfig {
        ServerConfig {
            deny: deny.iter().map(|d| d.to_string()).collect(),
            ..ServerConfig::default()
        }
    }

    #[test]
    fn the_flags_and_the_server_own_lists_both_have_to_permit_a_tool() {
        let open = ToolFilter::new(&ServerConfig::default(), &[], &[]);
        assert!(open.permits("delete_file"));

        let listed = ToolFilter::new(
            &fenced(&["delete_*"]),
            &["read_*".into(), "echo".into()],
            &["read_secret".into()],
        );
        assert!(listed.permits("read_file"));
        assert!(listed.permits("echo"));
        assert!(!listed.permits("read_secret"), "--deny beats --allow");
        assert!(!listed.permits("write_file"), "not on the --allow list");
        assert!(
            !listed.permits("delete_file"),
            "the server's own deny list holds here too"
        );
    }

    #[test]
    fn a_denied_tool_is_dropped_from_a_listing_and_the_cursor_is_left_alone() {
        let filter = ToolFilter::new(&ServerConfig::default(), &[], &["add".into()]);
        let mut listing = json!({"tools": [{"name": "echo"}, {"name": "add"}], "nextCursor": "p2"});
        filter.retain(&mut listing);
        assert_eq!(listing["tools"], json!([{"name": "echo"}]));
        assert_eq!(listing["nextCursor"], "p2");
    }

    #[test]
    fn capabilities_lose_what_the_proxy_cannot_relay() {
        let upstream = json!({
            "logging": {},
            "tools": {"listChanged": true},
            "resources": {"subscribe": true, "listChanged": true},
            "completions": {},
        });
        assert_eq!(
            relayable(&upstream),
            json!({"tools": {}, "resources": {}, "completions": {}})
        );
        assert_eq!(relayable(&Value::Null), json!({}));
    }

    #[test]
    fn only_a_loopback_address_is_served_without_saying_so() {
        assert!(listen_addr("127.0.0.1:0", false).is_ok());
        assert!(listen_addr("[::1]:0", false).is_ok());

        let refused = listen_addr("0.0.0.0:8321", false).unwrap_err();
        assert!(refused.to_string().contains("--listen-any"), "{refused}");
        assert!(listen_addr("0.0.0.0:8321", true).is_ok());
        assert!(listen_addr("127.0.0.1", false).is_err(), "a port is needed");
    }

    #[test]
    fn a_wrong_token_is_refused_whatever_its_shape() {
        let request = |authorization: &str| Request {
            method: "POST".into(),
            path: "/mcp".into(),
            version: "HTTP/1.1".into(),
            headers: vec![("authorization".into(), authorization.into())],
            body: String::new(),
        };
        assert!(authorized(&request("Bearer s3cret"), Some("s3cret")));
        assert!(authorized(&request("bearer s3cret"), Some("s3cret")));
        assert!(!authorized(&request("Bearer wrong"), Some("s3cret")));
        assert!(!authorized(&request("Basic s3cret"), Some("s3cret")));
        assert!(!authorized(&request(""), Some("s3cret")));
        assert!(authorized(&request(""), None), "no token, no gate");
    }
}
