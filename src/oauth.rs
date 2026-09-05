//! OAuth 2.1 as MCP specifies it, done once so it never has to be done again.
//!
//! * Discovery: RFC 9728 protected-resource metadata, then RFC 8414 authorization
//!   server metadata (with the OpenID Connect document as a fallback).
//! * Registration: RFC 7591 dynamic client registration, public client, no secret.
//!   A client registered by hand instead can be confidential, in which case its
//!   secret is supplied to `login` and presented at the token endpoint.
//! * Grant: authorization code + PKCE (S256) on a loopback redirect, with the RFC 8707
//!   `resource` indicator so the token is bound to this MCP server.
//! * Refresh: `refresh_token` grant, transparently, whenever a saved token has expired.
//!
//! There is no `client_credentials` path in practice - the servers seen so far only
//! offer `authorization_code` and `refresh_token` - so the browser step cannot be
//! avoided. It can be made to happen once. That is what the credential store is for.

use crate::config::{now, Credential};
use crate::protocol::{request, Error, Result, CLIENT_NAME, PROTOCOL_VERSION};
use crate::transport::http::{header, redirect_error, USER_AGENT};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// What discovery found out about how to get a token for one MCP server.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// The MCP endpoint, sent as the `resource` indicator.
    pub resource: String,
}

/// Thin HTTP helper used only by the OAuth flow.
pub struct Http {
    agent: ureq::Agent,
    user_agent: String,
}

impl Http {
    pub fn new(timeout: Duration, user_agent: Option<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            user_agent: user_agent.unwrap_or_else(|| USER_AGENT.to_string()),
        }
    }

    fn get_json(&self, url: &str) -> Result<Option<Value>> {
        let mut resp = match self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent)
            .call()
        {
            Ok(r) => r,
            Err(e) => return Err(Error::auth(format!("GET {url}: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(None);
        }
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::auth(format!("GET {url}: {e}")))?;
        Ok(serde_json::from_str(&text).ok())
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        body: &str,
        basic: Option<&str>,
    ) -> Result<(u16, Value, String)> {
        let mut req = self
            .agent
            .post(url)
            .header("Content-Type", content_type)
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(credentials) = basic {
            req = req.header("Authorization", &format!("Basic {credentials}"));
        }
        let mut resp = req
            .send(body)
            .map_err(|e| Error::auth(format!("POST {url}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::auth(format!("POST {url}: {e}")))?;
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, value, text))
    }

    fn post_json(&self, url: &str, body: &Value) -> Result<(u16, Value, String)> {
        self.post(url, "application/json", &body.to_string(), None)
    }

    fn post_form(
        &self,
        url: &str,
        params: &[(&str, &str)],
        basic: Option<&str>,
    ) -> Result<(u16, Value, String)> {
        let body = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.post(url, "application/x-www-form-urlencoded", &body, basic)
    }
}

// -- discovery ----------------------------------------------------------------

/// Ask the MCP endpoint itself who it wants us to talk to. Returns the
/// `WWW-Authenticate` header if the server challenged, `None` if it let an
/// anonymous `initialize` through.
pub fn challenge(http: &Http, mcp_url: &str) -> Result<Option<String>> {
    let init = request(
        "initialize",
        1,
        Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": crate::VERSION},
        })),
    );
    let no_redirect = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    );
    let mut resp = no_redirect
        .post(mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("User-Agent", &http.user_agent)
        .send(&init.to_string())
        .map_err(|e| Error::auth(format!("could not reach {mcp_url}: {e}")))?;
    let status = resp.status().as_u16();
    if (300..400).contains(&status) {
        let to = header(&resp, "location");
        return Err(redirect_error(mcp_url, status, to.as_deref()));
    }
    let www = header(&resp, "www-authenticate");
    let _ = resp.body_mut().read_to_string();
    match (status, www) {
        (_, Some(w)) => Ok(Some(w)),
        (401, None) => Ok(Some(String::new())),
        _ => Ok(None),
    }
}

/// Pull `key="value"` or `key=value` out of a `WWW-Authenticate` header.
pub fn challenge_param<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = header;
    while let Some(idx) = rest.find(key) {
        let after = &rest[idx + key.len()..];
        if let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            return Some(if let Some(q) = v.strip_prefix('"') {
                q.split('"').next().unwrap_or("")
            } else {
                v.split([',', ' ']).next().unwrap_or("")
            });
        }
        rest = after;
    }
    None
}

fn split_url(url: &str) -> (String, String) {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = url[scheme_end..].find('/').map(|i| i + scheme_end);
    match path_start {
        Some(p) => {
            let path = url[p..].split(['?', '#']).next().unwrap_or("");
            (url[..p].to_string(), path.trim_end_matches('/').to_string())
        }
        None => (
            url.split(['?', '#']).next().unwrap_or(url).to_string(),
            String::new(),
        ),
    }
}

/// Find the authorization server and its endpoints for an MCP URL.
pub fn discover(http: &Http, mcp_url: &str, challenge: Option<&str>) -> Result<Metadata> {
    let (origin, path) = split_url(mcp_url);

    // 1. Protected resource metadata (RFC 9728).
    let mut prm_urls = Vec::new();
    if let Some(u) = challenge.and_then(|c| challenge_param(c, "resource_metadata")) {
        prm_urls.push(u.to_string());
    }
    if !path.is_empty() {
        prm_urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    prm_urls.push(format!("{origin}/.well-known/oauth-protected-resource"));

    let mut issuer = origin;
    let mut scopes: Vec<String> = Vec::new();
    for u in &prm_urls {
        if let Some(prm) = http.get_json(u)? {
            if let Some(as_) = prm["authorization_servers"].get(0).and_then(Value::as_str) {
                issuer = as_.trim_end_matches('/').to_string();
            }
            scopes = string_list(&prm["scopes_supported"]);
            break;
        }
    }

    // 2. Authorization server metadata (RFC 8414, then OIDC discovery).
    let (as_origin, as_path) = split_url(&issuer);
    let mut as_urls = Vec::new();
    if !as_path.is_empty() {
        as_urls.push(format!(
            "{as_origin}/.well-known/oauth-authorization-server{as_path}"
        ));
        as_urls.push(format!(
            "{as_origin}/.well-known/openid-configuration{as_path}"
        ));
        as_urls.push(format!("{issuer}/.well-known/openid-configuration"));
    } else {
        as_urls.push(format!("{issuer}/.well-known/oauth-authorization-server"));
        as_urls.push(format!("{issuer}/.well-known/openid-configuration"));
    }

    let mut meta = Value::Null;
    for u in &as_urls {
        if let Some(m) = http.get_json(u)? {
            if m.get("token_endpoint").is_some() {
                meta = m;
                break;
            }
        }
    }

    let field = |name: &str, default: String| -> String {
        meta[name].as_str().map(str::to_string).unwrap_or(default)
    };
    if scopes.is_empty() {
        scopes = string_list(&meta["scopes_supported"]);
    }

    Ok(Metadata {
        authorization_endpoint: field("authorization_endpoint", format!("{issuer}/authorize")),
        token_endpoint: field("token_endpoint", format!("{issuer}/token")),
        registration_endpoint: meta["registration_endpoint"].as_str().map(str::to_string),
        issuer,
        scopes_supported: scopes,
        token_endpoint_auth_methods_supported: string_list(
            &meta["token_endpoint_auth_methods_supported"],
        ),
        resource: mcp_url.to_string(),
    })
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// -- client authentication -------------------------------------------------------

pub const CLIENT_SECRET_POST: &str = "client_secret_post";
pub const CLIENT_SECRET_BASIC: &str = "client_secret_basic";

/// Which of the two ways to present a client secret this server wants. RFC 8414
/// leaves the list optional and the field defaults to `client_secret_basic` on
/// paper, but post is what every server accepts in practice, so basic is only
/// chosen when the server advertises it and not post.
pub fn client_auth_method(supported: &[String]) -> &'static str {
    let has = |m: &str| supported.iter().any(|s| s == m);
    if has(CLIENT_SECRET_BASIC) && !has(CLIENT_SECRET_POST) {
        CLIENT_SECRET_BASIC
    } else {
        CLIENT_SECRET_POST
    }
}

/// Present the client secret on a token request, either as a form field or as the
/// `Authorization: Basic` value returned here. RFC 6749 section 2.3.1 forbids using
/// both at once, which is why one call decides between them.
fn authenticate_client<'a>(
    params: &mut Vec<(&'a str, &'a str)>,
    method: Option<&str>,
    client_id: Option<&'a str>,
    client_secret: Option<&'a str>,
) -> Option<String> {
    let (id, secret) = (client_id?, client_secret?);
    if method == Some(CLIENT_SECRET_BASIC) {
        // Both halves are form-encoded before base64, so a secret containing a
        // colon or an ampersand survives the round trip.
        return Some(STANDARD.encode(format!("{}:{}", urlencode(id), urlencode(secret))));
    }
    params.push(("client_secret", secret));
    None
}

// -- registration ---------------------------------------------------------------

const REDIRECT_REFUSED: &str = "authorization server refused the loopback redirect URI";

/// What to tell a human when neither loopback host is accepted.
pub fn loopback_hint(issuer: &str) -> String {
    format!(
        "\nHint: {issuer} rejects http://127.0.0.1 and http://localhost redirect URIs. RFC 8252 \
         section 7.3 requires an authorization server to accept them for native clients, and \
         the MCP authorization spec builds on that. This is a server-side setting.\n\
         If the server is Doorkeeper (Rails), in config/initializers/doorkeeper.rb set:\n\
         \n    force_ssl_in_redirect_uri {{ |uri| !%w[localhost 127.0.0.1 ::1].include?(uri.host) }}\n\
         \nOtherwise register a client out of band and pass --client-id."
    )
}

fn is_redirect_refused(e: &Error) -> bool {
    matches!(e, Error::Auth(m) if m.starts_with(REDIRECT_REFUSED))
}

pub fn register(http: &Http, meta: &Metadata, redirect_uri: &str) -> Result<String> {
    let endpoint = meta.registration_endpoint.as_deref().ok_or_else(|| {
        Error::auth(format!(
            "{} does not offer dynamic client registration; pass --client-id with a \
             client registered out of band",
            meta.issuer
        ))
    })?;
    let body = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let (status, value, text) = http.post_json(endpoint, &body)?;
    if !(200..300).contains(&status) {
        let lower = text.to_ascii_lowercase();
        if lower.contains("redirect") || lower.contains("invalid_client_metadata") {
            return Err(Error::Auth(format!(
                "{REDIRECT_REFUSED}: HTTP {status}\n{text}"
            )));
        }
        return Err(Error::auth(format!(
            "registration at {endpoint} failed: HTTP {status}\n{text}"
        )));
    }
    value["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::auth(format!("registration response had no client_id:\n{text}")))
}

// -- pkce -----------------------------------------------------------------------

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::auth(format!("no entropy source: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn pkce() -> Result<(String, String)> {
    let verifier = random_urlsafe(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (urldecode(k), urldecode(v))
        })
        .collect()
}

// -- the loopback redirect ----------------------------------------------------

/// Serve exactly one `/callback` request on `listener` and return the `code`.
fn wait_for_code(
    listeners: Vec<TcpListener>,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let (tx, rx) = mpsc::channel::<Result<String>>();
    for listener in listeners {
        let tx = tx.clone();
        let state = expected_state.to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                match handle_callback(stream, &state) {
                    Some(result) => {
                        let _ = tx.send(result);
                        return;
                    }
                    None => continue, // favicon, health checks, a stray browser prefetch
                }
            }
        });
    }
    drop(tx);
    rx.recv_timeout(timeout).map_err(|_| {
        Error::auth(format!(
            "no authorization callback within {}s",
            timeout.as_secs()
        ))
    })?
}

fn handle_callback(mut stream: TcpStream, expected_state: &str) -> Option<Result<String>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // "GET /callback?code=...&state=... HTTP/1.1"
    let target = line.split_whitespace().nth(1)?.to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    if path != "/callback" {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    // Drain the headers so the browser does not see a reset.
    loop {
        line.clear();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
    }

    let params = query_params(query);
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.as_str())
    };

    let result = if let Some(err) = get("error") {
        Err(Error::auth(format!(
            "authorization server returned {err}: {}",
            get("error_description").unwrap_or("")
        )))
    } else if get("state") != Some(expected_state) {
        Err(Error::auth(
            "state mismatch on callback; possible CSRF, aborting",
        ))
    } else if let Some(code) = get("code") {
        Ok(code.to_string())
    } else {
        Err(Error::auth("callback had neither code nor error"))
    };

    let (title, msg) = match &result {
        Ok(_) => (
            "Signed in",
            "mcpdial has the authorization code. You can close this tab.",
        ),
        Err(e) => ("Sign-in failed", &*e.to_string()),
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font-family:system-ui;margin:3rem\"><h1>{title}</h1><p>{msg}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.flush();
    Some(result)
}

// -- the whole dance --------------------------------------------------------------

pub struct LoginOptions {
    pub scope: Option<String>,
    pub port: Option<u16>,
    pub client_id: Option<String>,
    /// Secret of a confidential client, for a `client_id` registered out of band.
    pub client_secret: Option<String>,
    /// Loopback host for the redirect URI. `None` tries 127.0.0.1 first and falls
    /// back to localhost if the server refuses it (Doorkeeper's common allowlist).
    pub redirect_host: Option<String>,
    pub open_browser: bool,
    pub timeout: Duration,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            scope: None,
            port: None,
            client_id: None,
            client_secret: None,
            redirect_host: None,
            open_browser: true,
            timeout: Duration::from_secs(300),
        }
    }
}

/// Bind the loopback port on IPv4, and on IPv6 too when possible, so a browser that
/// resolves `localhost` to `::1` still reaches us.
fn bind_loopback(port: u16) -> Result<(Vec<TcpListener>, u16)> {
    let v4 = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| Error::auth(format!("cannot bind 127.0.0.1:{port}: {e}")))?;
    let port = v4
        .local_addr()
        .map_err(|e| Error::auth(e.to_string()))?
        .port();
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)) {
        listeners.push(v6);
    }
    Ok((listeners, port))
}

/// Run the authorization-code flow for `mcp_url` and return a credential to save.
/// `notify` receives progress lines meant for a human (they go to stderr in the CLI).
pub fn login(
    http: &Http,
    mcp_url: &str,
    existing: Option<&Credential>,
    opts: &LoginOptions,
    mut notify: impl FnMut(&str),
) -> Result<Credential> {
    let chal = challenge(http, mcp_url)?;
    if chal.is_none() {
        notify("note: the server accepted an anonymous initialize; a token may not be required");
    }
    let meta = discover(http, mcp_url, chal.as_deref())?;
    notify(&format!("authorization server: {}", meta.issuer));

    // A saved client id is only reusable with the exact redirect URI it was
    // registered for, which means the same port and the same loopback host.
    let saved = existing.filter(|c| c.client_id.is_some());
    let saved_port = saved.and_then(|c| c.redirect_port);
    let want_port = opts.port.or(saved_port).unwrap_or(0);
    let (listeners, port) = match bind_loopback(want_port) {
        Ok(l) => l,
        Err(_) if opts.port.is_none() && want_port != 0 => bind_loopback(0)?,
        Err(e) => return Err(e),
    };

    let mut host = opts
        .redirect_host
        .clone()
        .or_else(|| {
            saved
                .filter(|c| c.redirect_port == Some(port))
                .and_then(|c| c.redirect_host.clone())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let redirect_for = |h: &str| format!("http://{h}:{port}/callback");

    let client_id = match (&opts.client_id, saved) {
        (Some(id), _) => id.clone(),
        (None, Some(c))
            if c.redirect_port == Some(port)
                && c.redirect_host.as_deref() == Some(host.as_str()) =>
        {
            c.client_id.clone().unwrap()
        }
        _ => {
            let mut registered = register(http, &meta, &redirect_for(&host));
            let may_fall_back = opts.redirect_host.is_none() && host == "127.0.0.1";
            if may_fall_back && registered.as_ref().is_err_and(is_redirect_refused) {
                notify("server refused http://127.0.0.1 as a redirect URI; retrying with http://localhost");
                host = "localhost".to_string();
                registered = register(http, &meta, &redirect_for(&host));
            }
            registered.map_err(|e| {
                if is_redirect_refused(&e) {
                    Error::Auth(format!("{e}{}", loopback_hint(&meta.issuer)))
                } else {
                    e
                }
            })?
        }
    };
    let redirect_uri = redirect_for(&host);
    if saved.is_none_or(|c| c.client_id.as_deref() != Some(client_id.as_str())) {
        notify(&format!("registered client {client_id}"));
    }

    let client_secret = opts.client_secret.clone().or_else(|| {
        saved
            .filter(|c| c.client_id.as_deref() == Some(client_id.as_str()))
            .and_then(|c| c.client_secret.clone())
    });
    let auth_method = client_auth_method(&meta.token_endpoint_auth_methods_supported);

    let (verifier, code_challenge) = pkce()?;
    let state = random_urlsafe(16)?;
    let scope = opts
        .scope
        .clone()
        .or_else(|| (!meta.scopes_supported.is_empty()).then(|| meta.scopes_supported.join(" ")));

    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.clone()),
        ("redirect_uri", redirect_uri.clone()),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256".to_string()),
        ("state", state.clone()),
        ("resource", meta.resource.clone()),
    ];
    if let Some(s) = &scope {
        params.push(("scope", s.clone()));
    }
    let auth_url = format!(
        "{}{}{}",
        meta.authorization_endpoint,
        if meta.authorization_endpoint.contains('?') {
            "&"
        } else {
            "?"
        },
        params
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencode(v)))
            .collect::<Vec<_>>()
            .join("&")
    );

    notify(&format!("open this URL to authorize:\n\n  {auth_url}\n"));
    if opts.open_browser {
        if open_browser(&auth_url) {
            notify("(opened in your browser)");
        } else {
            notify("(could not open a browser automatically)");
        }
    }
    notify(&format!("waiting for the callback on {redirect_uri} ..."));

    let code = wait_for_code(listeners, &state, opts.timeout)?;

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", meta.resource.as_str()),
    ];
    let basic = authenticate_client(
        &mut params,
        Some(auth_method),
        Some(&client_id),
        client_secret.as_deref(),
    );
    let (status, value, text) = http.post_form(&meta.token_endpoint, &params, basic.as_deref())?;
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "token exchange failed: HTTP {status}\n{text}"
        )));
    }

    let mut cred = credential_from_token_response(&value, Credential::default())?;
    cred.token_endpoint = Some(meta.token_endpoint.clone());
    cred.token_endpoint_auth_method = client_secret.is_some().then(|| auth_method.to_string());
    cred.client_secret = client_secret;
    cred.client_id = Some(client_id);
    cred.redirect_port = Some(port);
    cred.redirect_host = Some(host);
    cred.resource = Some(meta.resource.clone());
    cred.source = Some("oauth".into());
    if cred.scope.is_none() {
        cred.scope = scope;
    }
    Ok(cred)
}

/// Trade a refresh token for a new access token. Returns the updated credential.
pub fn refresh(http: &Http, cred: &Credential) -> Result<Credential> {
    let endpoint = cred
        .token_endpoint
        .as_deref()
        .ok_or_else(|| Error::auth("credential has no token endpoint; run login again"))?;
    let rt = cred
        .refresh_token
        .as_deref()
        .ok_or_else(|| Error::auth("credential has no refresh token; run login again"))?;
    let mut params = vec![("grant_type", "refresh_token"), ("refresh_token", rt)];
    if let Some(id) = cred.client_id.as_deref() {
        params.push(("client_id", id));
    }
    if let Some(r) = cred.resource.as_deref() {
        params.push(("resource", r));
    }
    let basic = authenticate_client(
        &mut params,
        cred.token_endpoint_auth_method.as_deref(),
        cred.client_id.as_deref(),
        cred.client_secret.as_deref(),
    );
    let (status, value, text) = http.post_form(endpoint, &params, basic.as_deref())?;
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "token refresh failed: HTTP {status}\n{text}\nRun `mcpdial login` again."
        )));
    }
    credential_from_token_response(&value, cred.clone())
}

fn credential_from_token_response(value: &Value, mut base: Credential) -> Result<Credential> {
    let access = value["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::auth(format!("token response had no access_token: {value}")))?;
    base.access_token = Some(access.to_string());
    if let Some(rt) = value["refresh_token"].as_str() {
        base.refresh_token = Some(rt.to_string()); // rotated, or first issued
    }
    base.expires_at = value["expires_in"].as_u64().map(|s| now() + s);
    if let Some(s) = value["scope"].as_str() {
        base.scope = Some(s.to_string());
    }
    Ok(base)
}

fn open_browser(url: &str) -> bool {
    let cmd: (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        assert_eq!(
            split_url("https://a.b/mcp"),
            ("https://a.b".into(), "/mcp".into())
        );
        assert_eq!(
            split_url("https://a.b:8443/x/y/?q=1"),
            ("https://a.b:8443".into(), "/x/y".into())
        );
        assert_eq!(split_url("https://a.b"), ("https://a.b".into(), "".into()));
    }

    #[test]
    fn reads_challenge_params() {
        let h = r#"Bearer realm="Doorkeeper", error="invalid_token", resource_metadata="https://x/.well-known/oauth-protected-resource/mcp""#;
        assert_eq!(challenge_param(h, "realm"), Some("Doorkeeper"));
        assert_eq!(
            challenge_param(h, "resource_metadata"),
            Some("https://x/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(challenge_param("Bearer scope=mcp", "scope"), Some("mcp"));
        assert_eq!(challenge_param("Bearer", "scope"), None);
    }

    #[test]
    fn pkce_is_s256() {
        let (v, c) = pkce().unwrap();
        assert!(v.len() >= 43 && v.len() <= 128);
        assert_eq!(c, URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
    }

    #[test]
    fn url_coding_round_trips() {
        let s = "a b&c=d/e~f%";
        assert_eq!(urlencode(s), "a%20b%26c%3Dd%2Fe~f%25");
        assert_eq!(urldecode(&urlencode(s)), s);
        assert_eq!(
            query_params("code=ab%2Fc&state=x+y"),
            vec![
                ("code".into(), "ab/c".into()),
                ("state".into(), "x y".into())
            ]
        );
    }

    #[test]
    fn client_auth_defaults_to_post_and_picks_basic_only_when_post_is_not_offered() {
        let offered = |ms: &[&str]| ms.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(client_auth_method(&[]), CLIENT_SECRET_POST);
        assert_eq!(client_auth_method(&offered(&["none"])), CLIENT_SECRET_POST);
        assert_eq!(
            client_auth_method(&offered(&["client_secret_basic"])),
            CLIENT_SECRET_BASIC
        );
        assert_eq!(
            client_auth_method(&offered(&["client_secret_basic", "client_secret_post"])),
            CLIENT_SECRET_POST
        );
    }

    #[test]
    fn a_secret_goes_in_the_form_or_the_basic_header_but_never_both() {
        let grant = || vec![("grant_type", "refresh_token")];

        let mut params = grant();
        let basic = authenticate_client(
            &mut params,
            Some(CLIENT_SECRET_POST),
            Some("id"),
            Some("p&:w"),
        );
        assert!(basic.is_none());
        assert_eq!(params.last(), Some(&("client_secret", "p&:w")));

        let mut params = grant();
        let basic = authenticate_client(
            &mut params,
            Some(CLIENT_SECRET_BASIC),
            Some("id"),
            Some("p&:w"),
        )
        .unwrap();
        assert_eq!(params, grant());
        assert_eq!(
            String::from_utf8(STANDARD.decode(basic).unwrap()).unwrap(),
            "id:p%26%3Aw"
        );

        let mut params = grant();
        assert!(
            authenticate_client(&mut params, Some(CLIENT_SECRET_BASIC), Some("id"), None).is_none()
        );
        assert_eq!(params, grant(), "a public client sends no secret");
    }

    #[test]
    fn token_response_keeps_old_refresh_token_when_not_rotated() {
        let base = Credential {
            refresh_token: Some("old".into()),
            ..Default::default()
        };
        let c =
            credential_from_token_response(&json!({"access_token":"new","expires_in":60}), base)
                .unwrap();
        assert_eq!(c.access_token.as_deref(), Some("new"));
        assert_eq!(c.refresh_token.as_deref(), Some("old"));
        assert!(c.expires_at.unwrap() > now());
        assert!(credential_from_token_response(&json!({}), Credential::default()).is_err());
    }
}
