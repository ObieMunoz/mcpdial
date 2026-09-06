//! OAuth 2.1 as MCP specifies it, done once so it never has to be done again.
//!
//! * Discovery: RFC 9728 protected-resource metadata, then RFC 8414 authorization
//!   server metadata (with the OpenID Connect document as a fallback).
//! * Registration: a client ID metadata document when the server accepts one (the
//!   `client_id` is the URL of a document this repository publishes, which the
//!   server fetches for itself), otherwise RFC 7591 dynamic client registration.
//!   Both make a public client, no secret. A client registered by hand instead can
//!   be confidential, in which case its secret is supplied to `login` and presented
//!   at the token endpoint.
//! * Grant: authorization code + PKCE (S256) on a loopback redirect, with the RFC 8707
//!   `resource` indicator so the token is bound to this MCP server.
//! * Refresh: `refresh_token` grant, transparently, whenever a saved token has expired.
//! * Machines: the `client_credentials` grant for a confidential client, which needs
//!   no browser at all and is simply run again when its token expires.
//!
//! Most servers only offer `authorization_code` and `refresh_token`, so for them the
//! browser step cannot be avoided. It can be made to happen once. That is what the
//! credential store is for.

use crate::config::{now, Credential, CLIENT_CREDENTIALS};
use crate::protocol::{request, Error, Result, CLIENT_NAME, PROTOCOL_VERSION};
use crate::transport::http::{header, redirect_error, USER_AGENT};
use crate::transport::retry::{self, Failed, Failure, Retry};
use crate::transport::trace::{Kind, TraceEvent, Wire};
use crate::transport::{silent, Logger};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// What discovery found out about how to get a token for one MCP server.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    /// The server fetches a client ID metadata document, so a URL can be the client id.
    pub client_id_metadata_document_supported: bool,
    /// RFC 9207: the server says every authorization response names its issuer, so
    /// one that arrives without an `iss` is a response to reject.
    pub authorization_response_iss_parameter_supported: bool,
    pub scopes_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// Empty when the server did not say; RFC 8414 then implies `authorization_code`
    /// and `implicit`, but servers that leave it out mostly support more.
    pub grant_types_supported: Vec<String>,
    /// The MCP endpoint, sent as the `resource` indicator.
    pub resource: String,
}

/// Thin HTTP helper used only by the OAuth flow.
pub struct Http {
    agent: ureq::Agent,
    user_agent: String,
    retry: Retry,
    log: Logger,
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
            retry: Retry {
                enabled: true,
                timeout,
            },
            log: silent(),
        }
    }

    /// Whether a discovery `GET` that fails transiently is sent once more.
    pub fn retry(mut self, enabled: bool) -> Self {
        self.retry.enabled = enabled;
        self
    }

    /// Where every exchange is reported, secrets redacted.
    pub fn log(mut self, log: Option<Logger>) -> Self {
        if let Some(log) = log {
            self.log = log;
        }
        self
    }

    /// A discovery document, or `None` where the server has none to offer. The
    /// `GET` is idempotent, so a transient failure gets the one retry any
    /// idempotent request does; a status the retry cannot mend is a miss.
    fn get_json(&self, url: &str) -> Result<Option<Value>> {
        let mut outcome = self.get_once(url);
        if let Err(failed) = &outcome {
            if let Some(delay) = self.retry.delay(true, false, &failed.failure) {
                (self.log)(&TraceEvent::Retrying {
                    wire: Wire::OAuth(url),
                    after: &failed.failure,
                });
                thread::sleep(delay);
                outcome = self.get_once(url);
            }
        }
        match outcome {
            Ok(json) => Ok(json),
            Err(Failed {
                failure: Failure::Status { .. },
                ..
            }) => Ok(None),
            Err(failed) => Err(failed.error),
        }
    }

    fn get_once(&self, url: &str) -> std::result::Result<Option<Value>, Failed> {
        let wire = Wire::OAuth(url);
        (self.log)(&TraceEvent::HttpRequest {
            wire,
            method: "GET",
            purpose: "discovery".into(),
        });
        let started = Instant::now();
        let failed = |error: String| {
            (self.log)(&TraceEvent::HttpFailed {
                wire,
                method: "GET",
                error,
                elapsed: started.elapsed(),
            });
        };
        let mut resp = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent)
            .call()
            .map_err(|e| {
                failed(e.to_string());
                Failed {
                    failure: retry::before_any_reply(&e),
                    error: Error::auth(format!("GET {url}: {e}")),
                }
            })?;
        let status = resp.status().as_u16();
        let content_type = header(&resp, "content-type").unwrap_or_default();
        let reply = |elapsed| TraceEvent::HttpReply {
            wire,
            method: "GET",
            status,
            content_type: Some(content_type.clone()),
            body: None,
            elapsed,
        };
        if !resp.status().is_success() {
            (self.log)(&reply(started.elapsed()));
            return Err(Failed {
                error: Error::auth(format!("GET {url}: HTTP {status}")),
                failure: Failure::Status {
                    status,
                    retry_after: retry::retry_after(header(&resp, "retry-after").as_deref()),
                },
            });
        }
        let text = resp.body_mut().read_to_string().map_err(|e| {
            failed(e.to_string());
            Failed {
                error: Error::auth(format!("GET {url}: {e}")),
                failure: Failure::Interrupted,
            }
        })?;
        (self.log)(&reply(started.elapsed()));
        let document: Option<Value> = serde_json::from_str(&text).ok();
        if let Some(message) = &document {
            (self.log)(&TraceEvent::Received {
                wire,
                message,
                kind: Kind::Reply,
            });
        }
        Ok(document)
    }

    /// One POST. `shown` is the body as the trace shows it: the JSON that was
    /// sent, or a form's fields as an object, either way redacted on the way out.
    fn post(
        &self,
        url: &str,
        content_type: &str,
        body: &str,
        shown: &Value,
        basic: Option<&str>,
    ) -> Result<(u16, Value, String)> {
        let wire = Wire::OAuth(url);
        (self.log)(&TraceEvent::Sent {
            wire,
            message: shown,
        });
        let started = Instant::now();
        let failed = |error: String| {
            (self.log)(&TraceEvent::HttpFailed {
                wire,
                method: "POST",
                error,
                elapsed: started.elapsed(),
            });
        };
        let mut req = self
            .agent
            .post(url)
            .header("Content-Type", content_type)
            .header("Accept", "application/json")
            .header("User-Agent", &self.user_agent);
        if let Some(credentials) = basic {
            req = req.header("Authorization", &format!("Basic {credentials}"));
        }
        let mut resp = req.send(body).map_err(|e| {
            failed(e.to_string());
            Error::auth(format!("POST {url}: {e}"))
        })?;
        let status = resp.status().as_u16();
        let reply_content_type = header(&resp, "content-type").unwrap_or_default();
        let text = resp.body_mut().read_to_string().map_err(|e| {
            failed(e.to_string());
            Error::auth(format!("POST {url}: {e}"))
        })?;
        (self.log)(&TraceEvent::HttpReply {
            wire,
            method: "POST",
            status,
            content_type: Some(reply_content_type),
            body: None,
            elapsed: started.elapsed(),
        });
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !value.is_null() {
            (self.log)(&TraceEvent::Received {
                wire,
                message: &value,
                kind: Kind::Reply,
            });
        }
        Ok((status, value, text))
    }

    fn post_json(&self, url: &str, body: &Value) -> Result<(u16, Value, String)> {
        self.post(url, "application/json", &body.to_string(), body, None)
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
        let fields = Value::Object(
            params
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect(),
        );
        self.post(
            url,
            "application/x-www-form-urlencoded",
            &body,
            &fields,
            basic,
        )
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
    let wire = Wire::Http(mcp_url);
    (http.log)(&TraceEvent::Sent {
        wire,
        message: &init,
    });
    let started = Instant::now();
    let mut resp = no_redirect
        .post(mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("User-Agent", &http.user_agent)
        .send(&init.to_string())
        .map_err(|e| {
            (http.log)(&TraceEvent::HttpFailed {
                wire,
                method: "POST",
                error: e.to_string(),
                elapsed: started.elapsed(),
            });
            Error::auth(format!("could not reach {mcp_url}: {e}"))
        })?;
    let status = resp.status().as_u16();
    (http.log)(&TraceEvent::HttpReply {
        wire,
        method: "POST",
        status,
        content_type: Some(header(&resp, "content-type").unwrap_or_default()),
        body: None,
        elapsed: started.elapsed(),
    });
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
    let stated_issuer = validated_issuer(&meta, &issuer)?;

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
        client_id_metadata_document_supported: meta["client_id_metadata_document_supported"]
            .as_bool()
            .unwrap_or(false),
        authorization_response_iss_parameter_supported: meta
            ["authorization_response_iss_parameter_supported"]
            .as_bool()
            .unwrap_or(false),
        issuer: stated_issuer,
        scopes_supported: scopes,
        token_endpoint_auth_methods_supported: string_list(
            &meta["token_endpoint_auth_methods_supported"],
        ),
        grant_types_supported: string_list(&meta["grant_types_supported"]),
        resource: mcp_url.to_string(),
    })
}

/// The issuer to hold the authorization response to, out of a metadata document
/// fetched for `discovered`.
///
/// RFC 8414 section 3.3 refuses a document that names a different issuer than the
/// one its URL was built from, which is the whole point of the `iss` check below:
/// an unvalidated issuer would be no protection at all. The document's own
/// spelling is what an `iss` will be compared with, byte for byte, so it is what
/// is kept: a trailing slash `discovered` had stripped to build the well-known
/// URL is the one difference that is not a different issuer.
fn validated_issuer(document: &Value, discovered: &str) -> Result<String> {
    match document["issuer"].as_str() {
        None => Ok(discovered.to_string()),
        Some(stated) if stated.trim_end_matches('/') == discovered.trim_end_matches('/') => {
            Ok(stated.to_string())
        }
        Some(stated) => Err(Error::auth(format!(
            "authorization server metadata for {discovered} names {stated} as its issuer; \
             refusing to use it"
        ))),
    }
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
        // An OpenID Connect registration endpoint reads an absent application_type
        // as "web", and a web client may not redirect to loopback. mcpdial is a
        // command-line client and has nowhere else to redirect to; a server that
        // does not implement OIDC ignores the field.
        "application_type": "native",
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

// -- client id metadata documents ---------------------------------------------------

/// Where `docs/client-metadata.json` is published; see `.github/workflows/pages.yml`.
/// Presented as the `client_id` to a server that fetches such documents, so that no
/// registration is stored per install.
pub const CLIENT_METADATA_URL: &str = "https://obiemunoz.github.io/mcpdial/client-metadata.json";

/// Whether `login` may identify itself with a client ID metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ClientMetadata {
    /// The document this repository publishes, when the server says it accepts one.
    #[default]
    IfAdvertised,
    /// A document of the caller's own, whether or not the server advertises support.
    Url(String),
    /// Register dynamically instead, for a server whose document support is broken.
    Never,
}

/// How the client id presented to the authorization server came to be. Saved with
/// the credential so `token show` can say, and so a later login knows what it may reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// `--client-id`: a client an administrator registered out of band.
    PreRegistered,
    /// RFC 7591 dynamic registration.
    Dynamic,
    /// The client id is the URL of a metadata document the server fetches itself.
    ClientMetadataDocument,
}

impl Registration {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreRegistered => "pre-registered",
            Self::Dynamic => "dynamic",
            Self::ClientMetadataDocument => "client_metadata_document",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::PreRegistered,
            Self::Dynamic,
            Self::ClientMetadataDocument,
        ]
        .into_iter()
        .find(|r| r.as_str() == s)
    }

    /// Completes "registered ...", for `login` and `token show`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::PreRegistered => "out of band (--client-id)",
            Self::Dynamic => "dynamically",
            Self::ClientMetadataDocument => "via client metadata document",
        }
    }
}

/// The spec's two rules for a metadata URL: https, and a path, so that an origin
/// alone cannot be a client id.
pub fn check_client_metadata_url(url: &str) -> Result<()> {
    let path = url
        .strip_prefix("https://")
        .and_then(|rest| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("");
    if path.trim_start_matches('/').is_empty() {
        return Err(Error::usage(format!(
            "client metadata URL must be https with a path, like https://example.com/client.json: {url}"
        )));
    }
    Ok(())
}

/// What the client id will be, before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientId {
    PreRegistered(String),
    MetadataDocument(String),
    /// Registered dynamically on an earlier login, for the same redirect URI.
    Registered(String),
    Register,
}

/// The authorization server a saved client id belongs to, when that is not the one
/// now being logged in to and the id may therefore not be presented.
///
/// RFC 6749 section 2.2 makes a client id unique to the authorization server that
/// issued it, and 2026-07-28 draws the conclusion: a persisted client id is keyed
/// by that issuer, is never reused with another, and is registered afresh when the
/// server behind an MCP endpoint changes. A client ID metadata document is the one
/// exception, being a URL every authorization server resolves for itself. A
/// credential saved before mcpdial recorded issuers names none, and is adopted by
/// the first login that discovers one rather than thrown away.
fn bound_elsewhere<'a>(cred: &'a Credential, issuer: &str) -> Option<&'a str> {
    let saved = cred.issuer.as_deref()?;
    let portable = saved_registration(cred) == Registration::ClientMetadataDocument;
    (cred.client_id.is_some() && saved != issuer && !portable).then_some(saved)
}

/// How a credential saved before the method was recorded came by its client id:
/// only a client registered out of band has a secret.
fn saved_registration(c: &Credential) -> Registration {
    c.registration
        .as_deref()
        .and_then(Registration::parse)
        .unwrap_or(if c.client_secret.is_some() {
            Registration::PreRegistered
        } else {
            Registration::Dynamic
        })
}

/// The order the spec asks for: a client registered out of band (given now, or on
/// an earlier login), then a metadata document when the server accepts one or the
/// caller insists on one, then dynamic registration. `saved` is a credential whose
/// client id was registered for the redirect URI about to be used; a metadata
/// document lists port-less loopback URIs, so it is never worth reusing.
fn choose_client_id(opts: &LoginOptions, saved: Option<&Credential>, advertised: bool) -> ClientId {
    if let Some(id) = &opts.client_id {
        return ClientId::PreRegistered(id.clone());
    }
    let saved = saved.and_then(|c| Some((c.client_id.clone()?, saved_registration(c))));
    if let Some((id, Registration::PreRegistered)) = &saved {
        return ClientId::PreRegistered(id.clone());
    }
    match &opts.client_metadata {
        ClientMetadata::Url(url) => ClientId::MetadataDocument(url.clone()),
        ClientMetadata::IfAdvertised if advertised => {
            ClientId::MetadataDocument(CLIENT_METADATA_URL.to_string())
        }
        ClientMetadata::IfAdvertised | ClientMetadata::Never => match saved {
            Some((id, Registration::Dynamic)) => ClientId::Registered(id),
            _ => ClientId::Register,
        },
    }
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

fn escape_html(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
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

/// Who the authorization response has to say it came from (RFC 9207), recorded
/// before the browser is opened and checked before the code is redeemed.
///
/// The attack this defends against is a mix-up: an authorization server the client
/// also trusts, or one that got itself into discovery, sending back a code minted
/// somewhere else. Redeeming it hands the wrong server's code to the right server's
/// token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedIssuer {
    issuer: String,
    /// Whether the server advertises that it always sends `iss`, which makes a
    /// response without one a rejection rather than a server that has not caught up.
    always_sent: bool,
}

impl ExpectedIssuer {
    /// The four rows of the 2026-07-28 table: a present `iss` is always compared,
    /// by simple string comparison (RFC 3986 section 6.2.1) with no normalisation
    /// of scheme, host, port, trailing slash or percent-encoding; an absent one is
    /// refused only where the server said it always sends one.
    fn check(&self, iss: Option<&str>) -> Result<()> {
        match iss {
            Some(found) if found == self.issuer => Ok(()),
            Some(found) => Err(Error::auth(format!(
                "authorization response came from {found}, not {}; \
                 refusing to redeem the code",
                self.issuer
            ))),
            None if self.always_sent => Err(Error::auth(format!(
                "authorization response named no issuer, and {} advertises that it \
                 always names one; refusing to redeem the code",
                self.issuer
            ))),
            None => Ok(()),
        }
    }
}

/// Serve exactly one `/callback` request on `listener` and return the `code`.
fn wait_for_code(
    listeners: Vec<TcpListener>,
    expected_state: &str,
    expected_issuer: &ExpectedIssuer,
    timeout: Duration,
) -> Result<String> {
    let (tx, rx) = mpsc::channel::<Result<String>>();
    for listener in listeners {
        let tx = tx.clone();
        let state = expected_state.to_string();
        let issuer = expected_issuer.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                match handle_callback(stream, &state, &issuer) {
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

/// What one callback's query string amounts to: the authorization code, or why
/// there is not one to redeem.
///
/// The issuer is settled first and alone. A response whose `iss` is not the one
/// expected is not this server's response, so nothing else in it may be believed:
/// the spec forbids acting on or displaying its `error`, `error_description` or
/// `error_uri`, and this is where that is enforced.
fn callback_result(
    params: &[(String, String)],
    expected_state: &str,
    expected_issuer: &ExpectedIssuer,
) -> Result<String> {
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.as_str())
    };
    expected_issuer.check(get("iss"))?;
    if let Some(err) = get("error") {
        return Err(Error::auth(format!(
            "authorization server returned {err}: {}",
            get("error_description").unwrap_or("")
        )));
    }
    if get("state") != Some(expected_state) {
        return Err(Error::auth(
            "state mismatch on callback; possible CSRF, aborting",
        ));
    }
    match get("code") {
        Some(code) => Ok(code.to_string()),
        None => Err(Error::auth("callback had neither code nor error")),
    }
}

fn handle_callback(
    mut stream: TcpStream,
    expected_state: &str,
    expected_issuer: &ExpectedIssuer,
) -> Option<Result<String>> {
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

    let result = callback_result(&query_params(query), expected_state, expected_issuer);

    let (title, msg) = match &result {
        Ok(_) => (
            "Signed in",
            "mcpdial has the authorization code. You can close this tab.".to_string(),
        ),
        // Half of what a failure says was written by whoever sent the browser here.
        Err(e) => ("Sign-in failed", escape_html(&e.to_string())),
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
    pub client_metadata: ClientMetadata,
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
            client_metadata: ClientMetadata::default(),
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
    if let ClientMetadata::Url(url) = &opts.client_metadata {
        check_client_metadata_url(url)?;
    }
    let chal = challenge(http, mcp_url)?;
    if chal.is_none() {
        notify("note: the server accepted an anonymous initialize; a token may not be required");
    }
    let meta = discover(http, mcp_url, chal.as_deref())?;
    notify(&format!("authorization server: {}", meta.issuer));

    let existing = match existing.map(|c| (c, bound_elsewhere(c, &meta.issuer))) {
        Some((c, Some(was))) => {
            if opts.client_id.is_none() && saved_registration(c) == Registration::PreRegistered {
                return Err(Error::auth(format!(
                    "the saved client was registered with {was}, and this server now \
                     authorizes at {}; pass --client-id for a client registered there, or \
                     `mcpdial logout` first",
                    meta.issuer
                )));
            }
            notify(&format!(
                "authorization server is no longer {was}; registering with {} instead",
                meta.issuer
            ));
            None
        }
        _ => existing,
    };

    // A saved client id is only reusable with the exact redirect URI it was
    // registered for, which means the same port and the same loopback host. The
    // port is still worth asking for first when the id turns out not to matter.
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

    let reusable = saved.filter(|c| {
        c.redirect_port == Some(port) && c.redirect_host.as_deref() == Some(host.as_str())
    });
    let (client_id, registration) = match choose_client_id(
        opts,
        reusable,
        meta.client_id_metadata_document_supported,
    ) {
        ClientId::PreRegistered(id) => (id, Registration::PreRegistered),
        ClientId::MetadataDocument(url) => (url, Registration::ClientMetadataDocument),
        ClientId::Registered(id) => (id, Registration::Dynamic),
        ClientId::Register => {
            let mut registered = register(http, &meta, &redirect_for(&host));
            let may_fall_back = opts.redirect_host.is_none() && host == "127.0.0.1";
            if may_fall_back && registered.as_ref().is_err_and(is_redirect_refused) {
                notify("server refused http://127.0.0.1 as a redirect URI; retrying with http://localhost");
                host = "localhost".to_string();
                registered = register(http, &meta, &redirect_for(&host));
            }
            let id = registered.map_err(|e| {
                if is_redirect_refused(&e) {
                    Error::Auth(format!("{e}{}", loopback_hint(&meta.issuer)))
                } else {
                    e
                }
            })?;
            (id, Registration::Dynamic)
        }
    };
    let redirect_uri = redirect_for(&host);
    match registration {
        Registration::Dynamic => {
            if saved.is_none_or(|c| c.client_id.as_deref() != Some(client_id.as_str())) {
                notify(&format!("registered client {client_id}"));
            }
        }
        other => notify(&format!(
            "client {client_id}, registered {}",
            other.describe()
        )),
    }

    let client_secret = opts.client_secret.clone().or_else(|| {
        saved
            .filter(|c| c.client_id.as_deref() == Some(client_id.as_str()))
            .and_then(|c| c.client_secret.clone())
    });
    let auth_method = client_auth_method(&meta.token_endpoint_auth_methods_supported);

    let (verifier, code_challenge) = pkce()?;
    let state = random_urlsafe(16)?;
    // Recorded here, beside the verifier and the state and before the browser is
    // sent anywhere, because that is what makes it worth comparing on the way back.
    let expected_issuer = ExpectedIssuer {
        issuer: meta.issuer.clone(),
        always_sent: meta.authorization_response_iss_parameter_supported,
    };
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

    let code = wait_for_code(listeners, &state, &expected_issuer, opts.timeout)?;

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
    cred.registration = Some(registration.as_str().to_string());
    cred.redirect_port = Some(port);
    cred.redirect_host = Some(host);
    cred.issuer = Some(meta.issuer.clone());
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

// -- client credentials ----------------------------------------------------------

/// Run the client-credentials grant for `mcp_url`: discovery as `login` does it, then
/// one token request that a confidential client answers for itself. No browser, no
/// redirect, no registration, so the flags that serve those are refused.
pub fn login_client_credentials(
    http: &Http,
    mcp_url: &str,
    opts: &LoginOptions,
    mut notify: impl FnMut(&str),
) -> Result<Credential> {
    if opts.port.is_some() || opts.redirect_host.is_some() || !opts.open_browser {
        return Err(Error::usage(
            "--port, --redirect-host and --no-browser belong to the authorization-code \
             grant; the client-credentials grant opens no browser and takes no redirect",
        ));
    }
    let client_id = opts.client_id.as_deref().ok_or_else(|| {
        Error::usage(
            "--grant client-credentials needs --client-id, a client registered out of band",
        )
    })?;
    let client_secret = opts.client_secret.as_deref().ok_or_else(|| {
        Error::usage(
            "--grant client-credentials needs that client's secret: --client-secret reads \
             it from stdin, --client-secret-env VAR from the environment",
        )
    })?;
    let chal = challenge(http, mcp_url)?;
    if chal.is_none() {
        notify("note: the server accepted an anonymous initialize; a token may not be required");
    }
    let meta = discover(http, mcp_url, chal.as_deref())?;
    notify(&format!("authorization server: {}", meta.issuer));
    client_credentials(http, &meta, client_id, client_secret, opts.scope.clone())
}

/// A usage error naming what the server does offer when `client_credentials` is
/// not among its advertised grants. A server that advertises nothing is given
/// the benefit of the doubt.
fn require_grant(meta: &Metadata, grant: &str) -> Result<()> {
    let offered = &meta.grant_types_supported;
    if offered.is_empty() || offered.iter().any(|g| g == grant) {
        return Ok(());
    }
    Err(Error::usage(format!(
        "{} does not offer the {grant} grant; it supports: {}",
        meta.issuer,
        offered.join(", ")
    )))
}

/// Ask the token endpoint for a token on the client's own behalf (RFC 6749
/// section 4.4) and return the credential to save.
pub fn client_credentials(
    http: &Http,
    meta: &Metadata,
    client_id: &str,
    client_secret: &str,
    scope: Option<String>,
) -> Result<Credential> {
    require_grant(meta, CLIENT_CREDENTIALS)?;
    let scope = scope
        .or_else(|| (!meta.scopes_supported.is_empty()).then(|| meta.scopes_supported.join(" ")));
    let base = Credential {
        token_endpoint: Some(meta.token_endpoint.clone()),
        token_endpoint_auth_method: Some(
            client_auth_method(&meta.token_endpoint_auth_methods_supported).to_string(),
        ),
        client_id: Some(client_id.to_string()),
        registration: Some(Registration::PreRegistered.as_str().to_string()),
        client_secret: Some(client_secret.to_string()),
        scope,
        issuer: Some(meta.issuer.clone()),
        resource: Some(meta.resource.clone()),
        source: Some(CLIENT_CREDENTIALS.into()),
        ..Default::default()
    };
    client_credentials_grant(http, base)
}

/// Run the grant again with everything the saved credential remembers, which is
/// how a client-credentials token is refreshed: there is no refresh token, and
/// nothing about it needs a human.
pub fn renew_client_credentials(http: &Http, cred: &Credential) -> Result<Credential> {
    if !cred.renews_by_grant() {
        return Err(Error::auth(
            "credential has no client id and secret to renew with; run login again",
        ));
    }
    client_credentials_grant(http, cred.clone())
}

/// The token request itself. `base` carries the endpoint, the client and the
/// scope; what comes back is written over its token fields.
fn client_credentials_grant(http: &Http, base: Credential) -> Result<Credential> {
    let endpoint = base
        .token_endpoint
        .as_deref()
        .ok_or_else(|| Error::auth("credential has no token endpoint; run login again"))?;
    let (Some(client_id), Some(client_secret)) =
        (base.client_id.as_deref(), base.client_secret.as_deref())
    else {
        return Err(Error::auth(
            "the client-credentials grant needs a client id and secret",
        ));
    };
    let mut params = vec![("grant_type", CLIENT_CREDENTIALS), ("client_id", client_id)];
    if let Some(s) = base.scope.as_deref() {
        params.push(("scope", s));
    }
    if let Some(r) = base.resource.as_deref() {
        params.push(("resource", r));
    }
    let basic = authenticate_client(
        &mut params,
        base.token_endpoint_auth_method.as_deref(),
        Some(client_id),
        Some(client_secret),
    );
    let (status, value, text) = http.post_form(endpoint, &params, basic.as_deref())?;
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "client credentials grant failed: HTTP {status}\n{text}"
        )));
    }
    let scope = base.scope.clone();
    let mut cred = credential_from_token_response(&value, base)?;
    if cred.scope.is_none() {
        cred.scope = scope;
    }
    Ok(cred)
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

/// Hand a URL to whatever the desktop opens web addresses with.
pub(crate) fn open_browser(url: &str) -> bool {
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
    fn the_published_document_is_the_one_the_constant_names() {
        let doc: Value =
            serde_json::from_str(include_str!("../docs/client-metadata.json")).unwrap();
        assert_eq!(doc["client_id"], CLIENT_METADATA_URL);
        assert!(check_client_metadata_url(CLIENT_METADATA_URL).is_ok());
        assert_eq!(doc["client_name"], CLIENT_NAME);
        assert_eq!(
            string_list(&doc["redirect_uris"]),
            ["http://127.0.0.1/callback", "http://localhost/callback"],
            "port-less loopback URIs, since the port is chosen at login"
        );
        assert_eq!(
            string_list(&doc["grant_types"]),
            ["authorization_code", "refresh_token"]
        );
        assert_eq!(string_list(&doc["response_types"]), ["code"]);
        assert_eq!(doc["token_endpoint_auth_method"], "none");
    }

    #[test]
    fn a_metadata_url_is_https_with_a_path() {
        for ok in ["https://example.com/client.json", "https://a.b/x/y?v=1"] {
            assert!(check_client_metadata_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com/client.json",
            "https://example.com",
            "https://example.com/",
            "example.com/client.json",
        ] {
            assert!(
                matches!(check_client_metadata_url(bad), Err(Error::Usage(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn registration_names_round_trip() {
        for r in [
            Registration::PreRegistered,
            Registration::Dynamic,
            Registration::ClientMetadataDocument,
        ] {
            assert_eq!(Registration::parse(r.as_str()), Some(r));
        }
        assert_eq!(Registration::parse("manual"), None);
    }

    #[test]
    fn the_client_id_is_chosen_in_the_order_the_spec_asks_for() {
        let opts = |client_id: Option<&str>, client_metadata: ClientMetadata| LoginOptions {
            client_id: client_id.map(str::to_string),
            client_metadata,
            ..Default::default()
        };
        let saved = |id: &str, registration: Option<&str>, secret: Option<&str>| Credential {
            client_id: Some(id.into()),
            registration: registration.map(str::to_string),
            client_secret: secret.map(str::to_string),
            ..Default::default()
        };
        let custom = || ClientMetadata::Url("https://x/y".into());
        let pre = |id: &str| ClientId::PreRegistered(id.into());
        let document = |url: &str| ClientId::MetadataDocument(url.into());

        // --client-id beats everything, including what was saved.
        assert_eq!(
            choose_client_id(
                &opts(Some("mine"), custom()),
                Some(&saved("old", Some("pre-registered"), None)),
                true
            ),
            pre("mine")
        );
        // A client registered out of band on an earlier login is kept, and a saved
        // secret is what marks one from before the method was recorded.
        let advertised = ClientMetadata::IfAdvertised;
        assert_eq!(
            choose_client_id(
                &opts(None, advertised.clone()),
                Some(&saved("conf", Some("pre-registered"), None)),
                true
            ),
            pre("conf")
        );
        assert_eq!(
            choose_client_id(
                &opts(None, advertised.clone()),
                Some(&saved("conf", None, Some("s"))),
                true
            ),
            pre("conf")
        );
        // The document when the server accepts one, over a saved dynamic registration.
        assert_eq!(
            choose_client_id(
                &opts(None, advertised.clone()),
                Some(&saved("client-abc", Some("dynamic"), None)),
                true
            ),
            document(CLIENT_METADATA_URL)
        );
        // A document of the caller's own, whether or not the server advertises.
        assert_eq!(
            choose_client_id(&opts(None, custom()), None, false),
            document("https://x/y")
        );
        // Otherwise the saved dynamic registration, else a new one.
        assert_eq!(
            choose_client_id(
                &opts(None, advertised.clone()),
                Some(&saved("client-abc", None, None)),
                false
            ),
            ClientId::Registered("client-abc".into())
        );
        assert_eq!(
            choose_client_id(&opts(None, advertised), None, false),
            ClientId::Register
        );
        // --no-client-metadata: never the document, and a saved document id is no use.
        assert_eq!(
            choose_client_id(
                &opts(None, ClientMetadata::Never),
                Some(&saved(
                    CLIENT_METADATA_URL,
                    Some("client_metadata_document"),
                    None
                )),
                true
            ),
            ClientId::Register
        );
    }

    #[test]
    fn the_grant_is_refused_only_when_the_server_lists_grants_and_leaves_it_out() {
        let meta = |grants: &[&str]| Metadata {
            issuer: "https://as".into(),
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            registration_endpoint: None,
            client_id_metadata_document_supported: false,
            authorization_response_iss_parameter_supported: false,
            scopes_supported: Vec::new(),
            token_endpoint_auth_methods_supported: Vec::new(),
            grant_types_supported: grants.iter().map(|g| g.to_string()).collect(),
            resource: String::new(),
        };
        assert!(require_grant(&meta(&[]), CLIENT_CREDENTIALS).is_ok());
        assert!(require_grant(
            &meta(&["authorization_code", "client_credentials"]),
            CLIENT_CREDENTIALS
        )
        .is_ok());
        let err = require_grant(
            &meta(&["authorization_code", "refresh_token"]),
            CLIENT_CREDENTIALS,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "https://as does not offer the client_credentials grant; it supports: \
             authorization_code, refresh_token"
        );
    }

    /// The table in the 2026-07-28 authorization spec, row by row.
    #[test]
    fn an_authorization_response_is_held_to_the_issuer_it_was_asked_of() {
        let expected = |always_sent| ExpectedIssuer {
            issuer: "https://as.example".into(),
            always_sent,
        };
        for always_sent in [true, false] {
            assert!(expected(always_sent)
                .check(Some("https://as.example"))
                .is_ok());
            let err = expected(always_sent)
                .check(Some("https://evil.example"))
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "authorization response came from https://evil.example, not \
                 https://as.example; refusing to redeem the code"
            );
        }
        // Absent is refused only where the server advertised that it always sends one.
        assert!(expected(false).check(None).is_ok());
        assert!(expected(true).check(None).is_err());
    }

    /// RFC 3986 section 6.2.1 and nothing beyond it: every one of these is a
    /// different issuer, however close it looks.
    #[test]
    fn the_issuer_comparison_normalises_nothing() {
        let expected = ExpectedIssuer {
            issuer: "https://as.example".into(),
            always_sent: true,
        };
        for near_miss in [
            "https://as.example/",
            "https://AS.example",
            "HTTPS://as.example",
            "https://as.example:443",
            "https://as.exa%6dple",
        ] {
            assert!(expected.check(Some(near_miss)).is_err(), "{near_miss}");
        }
    }

    #[test]
    fn a_mismatched_issuer_is_settled_before_the_rest_of_the_response_is_read() {
        let params = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>()
        };
        let expected = ExpectedIssuer {
            issuer: "https://as.example".into(),
            always_sent: false,
        };
        let code = params(&[
            ("code", "abc"),
            ("state", "st"),
            ("iss", "https://as.example"),
        ]);
        assert_eq!(callback_result(&code, "st", &expected).unwrap(), "abc");

        // An error response from somewhere else is neither acted on nor repeated.
        let elsewhere = params(&[
            ("error", "access_denied"),
            ("error_description", "no such user"),
            ("state", "st"),
            ("iss", "https://evil.example"),
        ]);
        let err = callback_result(&elsewhere, "st", &expected)
            .unwrap_err()
            .to_string();
        assert!(err.contains("https://evil.example"), "{err}");
        assert!(!err.contains("access_denied"), "{err}");
        assert!(!err.contains("no such user"), "{err}");

        // The state check still stands behind it.
        let wrong_state = params(&[("code", "abc"), ("state", "other")]);
        assert!(callback_result(&wrong_state, "st", &expected)
            .unwrap_err()
            .to_string()
            .contains("state mismatch"));
    }

    #[test]
    fn what_a_failed_callback_shows_the_browser_cannot_be_markup() {
        assert_eq!(
            escape_html(r#"<script>alert("x&y")</script>"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
        );
    }

    /// RFC 8414 section 3.3: metadata that names another issuer is not this
    /// server's metadata. The recorded issuer is the document's own spelling, so
    /// that a `iss` written the same way compares equal.
    #[test]
    fn metadata_that_names_another_issuer_is_refused() {
        let doc = |issuer: &str| json!({"issuer": issuer, "token_endpoint": "https://as/t"});
        assert_eq!(
            validated_issuer(&doc("https://as.example"), "https://as.example").unwrap(),
            "https://as.example"
        );
        assert_eq!(
            validated_issuer(&doc("https://as.example/"), "https://as.example").unwrap(),
            "https://as.example/",
            "discovery strips the trailing slash to build the well-known URL"
        );
        assert_eq!(
            validated_issuer(&Value::Null, "https://as.example").unwrap(),
            "https://as.example"
        );
        let err = validated_issuer(&doc("https://honest.example"), "https://attacker.example")
            .unwrap_err();
        assert!(matches!(err, Error::Auth(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "authorization server metadata for https://attacker.example names \
             https://honest.example as its issuer; refusing to use it"
        );
    }

    #[test]
    fn a_client_id_stays_with_the_authorization_server_that_granted_it() {
        let saved = |issuer: Option<&str>, registration: &str| Credential {
            client_id: Some("client-abc".into()),
            registration: Some(registration.into()),
            issuer: issuer.map(str::to_string),
            ..Default::default()
        };
        let here = "https://as.example";
        let elsewhere = "https://other.example";
        assert_eq!(bound_elsewhere(&saved(Some(here), "dynamic"), here), None);
        assert_eq!(
            bound_elsewhere(&saved(Some(elsewhere), "dynamic"), here),
            Some(elsewhere)
        );
        assert_eq!(
            bound_elsewhere(&saved(Some(elsewhere), "pre-registered"), here),
            Some(elsewhere)
        );
        // A URL the authorization server resolves itself is the same URL anywhere.
        assert_eq!(
            bound_elsewhere(&saved(Some(elsewhere), "client_metadata_document"), here),
            None
        );
        // Saved before mcpdial recorded issuers: adopted, not thrown away.
        assert_eq!(bound_elsewhere(&saved(None, "dynamic"), here), None);
        // Nothing was registered, so there is nothing bound to anywhere.
        let token_only = Credential {
            issuer: Some(elsewhere.into()),
            ..Default::default()
        };
        assert_eq!(bound_elsewhere(&token_only, here), None);
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
