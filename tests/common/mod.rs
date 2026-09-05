//! A fake Streamable HTTP MCP server, with a fake OAuth authorization server on the
//! same origin, so the whole client can be exercised without the network.

#![allow(dead_code)]

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

#[derive(Clone, Debug)]
pub enum Mode {
    /// SSE-framed replies, a session id, and `notifications/initialized` is mandatory.
    Stateful,
    /// Plain JSON replies, no session.
    Stateless,
    /// Stateless, and the initialize result insists on 2025-06-18 whatever was offered.
    OlderProtocol,
    /// Stateless, and the initialize result echoes whatever version was offered.
    EchoProtocol,
    /// 401 with a challenge unless a valid bearer token is presented.
    Auth { tokens: Vec<String> },
    /// Like `Auth`, but registration refuses http://127.0.0.1 (Doorkeeper's default
    /// allowlist admits only `localhost` once it is opened up at all).
    AuthLocalhostOnly,
    /// Like `Auth`, and the authorization server advertises client ID metadata
    /// documents: `authorize` expects this URL as the client id, unless the client
    /// registered dynamically after all.
    AuthClientMetadata { client_id: String },
    /// 403 with no challenge, like a WAF.
    Blocked,
    /// Hands back the same `nextCursor` on every `tools/list`.
    StuckCursor,
    /// Stateful, but answers `405` to the session-terminating `DELETE`.
    StatefulNoDelete,
    /// No dynamic registration, and a confidential client an administrator issued by
    /// hand: the token endpoint refuses any request that does not prove the secret,
    /// in the one placement named here.
    Confidential { auth_method: String },
    /// Protocol 2024-11-05: the URL serves `GET` alone, streaming an `endpoint` event
    /// that names where requests are POSTed.
    LegacySse,
}

/// The client the administrator registered out of band. The secret carries the
/// characters that have to survive form encoding in either placement.
pub const CONFIDENTIAL_ID: &str = "conf-client";
pub const CONFIDENTIAL_SECRET: &str = "c0nf&s3cr3t=/:x";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct State {
    initialized: bool,
    valid_tokens: Vec<String>,
    code_challenge: Option<String>,
    registered: bool,
    issued: u32,
    stuck_cursor_pages_served: u32,
}

pub struct FakeServer {
    pub base: String,
    pub url: String,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn start(mode: Mode) -> FakeServer {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let port = server.server_addr().to_ip().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let url = format!("{base}/mcp");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let state = Arc::new(Mutex::new(State {
        valid_tokens: match &mode {
            Mode::Auth { tokens } => tokens.clone(),
            _ => Vec::new(),
        },
        ..Default::default()
    }));

    let handle = {
        let requests = requests.clone();
        let stop = stop.clone();
        let base = base.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let rec = Recorded {
                    method: req.method().as_str().to_string(),
                    path: req.url().to_string(),
                    headers: req
                        .headers()
                        .iter()
                        .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
                        .collect(),
                    body,
                };
                requests.lock().unwrap().push(rec.clone());
                let resp = route(&mode, &base, &rec, &state);
                let _ = req.respond(resp);
            }
        })
    };

    FakeServer {
        base,
        url,
        requests,
        stop,
        handle: Some(handle),
    }
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn with_headers(resp: Resp, headers: &[(&str, &str)]) -> Resp {
    headers.iter().fold(resp, |r, (k, v)| {
        r.with_header(Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap())
    })
}

fn json_resp(status: u16, v: &Value) -> Resp {
    with_headers(
        Response::from_string(v.to_string()).with_status_code(status),
        &[("Content-Type", "application/json")],
    )
}

fn route(mode: &Mode, base: &str, rec: &Recorded, state: &Mutex<State>) -> Resp {
    let (path, query) = rec.path.split_once('?').unwrap_or((&rec.path, ""));
    match path {
        "/mcp" => mcp(mode, base, rec, state),
        "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp" => {
            json_resp(
                200,
                &json!({
                    "resource": format!("{base}/mcp"),
                    "authorization_servers": [base],
                    "scopes_supported": ["mcp"],
                    "bearer_methods_supported": ["header"],
                }),
            )
        }
        "/.well-known/oauth-authorization-server" => {
            let mut meta = json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["mcp"],
            });
            if let Mode::Confidential { auth_method } = mode {
                meta["token_endpoint_auth_methods_supported"] = json!([auth_method]);
                meta["grant_types_supported"] =
                    json!(["authorization_code", "refresh_token", "client_credentials"]);
                meta.as_object_mut()
                    .unwrap()
                    .remove("registration_endpoint");
            }
            if let Mode::AuthClientMetadata { .. } = mode {
                meta["client_id_metadata_document_supported"] = json!(true);
            }
            json_resp(200, &meta)
        }
        "/moved" => with_headers(
            Response::from_string("").with_status_code(301),
            &[("Location", &format!("{base}/mcp"))],
        ),
        "/register" => {
            let body = rec.json();
            let redirect = body["redirect_uris"][0].as_str().unwrap_or("");
            if matches!(mode, Mode::AuthLocalhostOnly) && !redirect.starts_with("http://localhost:")
            {
                return json_resp(
                    400,
                    &json!({"error": "invalid_client_metadata",
                    "error_description": "Redirect URI must be an HTTPS/SSL URI."}),
                );
            }
            assert_eq!(
                body["token_endpoint_auth_method"], "none",
                "must register as a public client"
            );
            let redirect = body["redirect_uris"][0].as_str().unwrap_or("").to_string();
            state.lock().unwrap().registered = true;
            json_resp(
                201,
                &json!({"client_id": "client-abc", "redirect_uris": [redirect]}),
            )
        }
        "/authorize" => {
            let q = form(query);
            let get = |k: &str| {
                q.iter()
                    .find(|(a, _)| a == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            assert_eq!(get("response_type"), "code");
            let registered = state.lock().unwrap().registered;
            assert_eq!(get("client_id"), expected_client_id(mode, registered));
            assert_eq!(get("code_challenge_method"), "S256");
            assert_eq!(
                get("resource"),
                format!("{base}/mcp"),
                "must send the resource indicator"
            );
            state.lock().unwrap().code_challenge = Some(get("code_challenge"));
            let location = format!(
                "{}?code=code-123&state={}",
                get("redirect_uri"),
                get("state")
            );
            with_headers(
                Response::from_string("").with_status_code(302),
                &[("Location", &location)],
            )
        }
        "/token" => {
            let f = form(&rec.body);
            let get = |k: &str| {
                f.iter()
                    .find(|(a, _)| a == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            if let Mode::Confidential { auth_method } = mode {
                let presented = if auth_method == "client_secret_basic" {
                    assert_eq!(get("client_secret"), "", "basic auth, not both");
                    basic_secret(rec.header("authorization").unwrap_or_default())
                } else {
                    assert_eq!(rec.header("authorization"), None, "post auth, not both");
                    get("client_secret")
                };
                if presented != CONFIDENTIAL_SECRET {
                    return json_resp(401, &json!({"error": "invalid_client"}));
                }
            }
            let mut st = state.lock().unwrap();
            match get("grant_type").as_str() {
                "authorization_code" => {
                    if get("code") != "code-123" {
                        return json_resp(400, &json!({"error": "invalid_grant"}));
                    }
                    let expected = st.code_challenge.clone().unwrap_or_default();
                    let got =
                        URL_SAFE_NO_PAD.encode(Sha256::digest(get("code_verifier").as_bytes()));
                    if got != expected {
                        return json_resp(
                            400,
                            &json!({"error": "invalid_grant", "error_description": "pkce mismatch"}),
                        );
                    }
                    st.issued += 1;
                    let tok = format!("tok-{}", st.issued);
                    st.valid_tokens.push(tok.clone());
                    json_resp(
                        200,
                        &json!({"access_token": tok, "token_type": "Bearer",
                        "expires_in": 3600, "refresh_token": "ref-1", "scope": "mcp"}),
                    )
                }
                // Short-lived and without a refresh token, as RFC 6749 section 4.4.3
                // has it, so a client that wants to stay signed in must run the grant
                // again.
                "client_credentials" if matches!(mode, Mode::Confidential { .. }) => {
                    assert_eq!(get("client_id"), CONFIDENTIAL_ID);
                    assert_eq!(get("resource"), format!("{base}/mcp"));
                    assert_eq!(get("code"), "", "no code in a client-credentials request");
                    st.issued += 1;
                    let tok = format!("tok-{}", st.issued);
                    st.valid_tokens.push(tok.clone());
                    json_resp(
                        200,
                        &json!({"access_token": tok, "token_type": "Bearer",
                        "expires_in": 60, "scope": get("scope")}),
                    )
                }
                "refresh_token" => {
                    if !get("refresh_token").starts_with("ref-") {
                        return json_resp(400, &json!({"error": "invalid_grant"}));
                    }
                    st.issued += 1;
                    let tok = format!("tok-{}", st.issued);
                    st.valid_tokens.push(tok.clone());
                    json_resp(
                        200,
                        &json!({"access_token": tok, "token_type": "Bearer",
                        "expires_in": 3600, "refresh_token": format!("ref-{}", st.issued)}),
                    )
                }
                other => json_resp(
                    400,
                    &json!({"error": "unsupported_grant_type", "got": other}),
                ),
            }
        }
        p if p.starts_with("/v0.1/servers") => registry(base, &percent_decode(p), query),
        "/catalog.json" => json_resp(200, &catalog_entries(base)),
        _ => Response::from_string("not found").with_status_code(404),
    }
}

/// What the fake registry lists: one entry per shape `add --registry` handles. The
/// remotes point at this server's own `/mcp`, so a server added from here dials.
pub fn registry_entries(base: &str) -> Vec<Value> {
    let mcp = format!("{base}/mcp");
    vec![
        json!({
            "name": "io.github.acme/files",
            "description": "Serve one directory over MCP. A long enough description that the search table has to cut it short somewhere.",
            "version": "1.2.0",
            "packages": [{
                "registryType": "npm", "identifier": "@acme/files", "version": "1.2.0",
                "runtimeHint": "npx", "transport": {"type": "stdio"},
                "runtimeArguments": [{"value": "-y", "type": "positional"}],
                "packageArguments": [{"type": "positional", "valueHint": "directory", "isRequired": true,
                                      "format": "filepath", "description": "Directory to serve"}],
                "environmentVariables": [
                    {"name": "ACME_TOKEN", "isRequired": true, "isSecret": true, "description": "API token"},
                    {"name": "ACME_LOG", "description": "Log level"}
                ]
            }]
        }),
        json!({
            "name": "io.github.acme/remote",
            "description": "The fake server, reached over HTTP.",
            "version": "2.0.0",
            "remotes": [{"type": "streamable-http", "url": mcp}, {"type": "sse", "url": mcp}]
        }),
        json!({
            "name": "io.github.acme/legacy",
            "description": "The fake server, listed as SSE only.",
            "version": "0.9.0",
            "remotes": [{"type": "sse", "url": mcp}]
        }),
        json!({
            "name": "io.github.acme/box",
            "description": "A sandbox, as a Python package or a container.",
            "version": "1.0",
            "packages": [
                {"registryType": "pypi", "identifier": "acme-box", "version": "1.0", "transport": {"type": "stdio"}},
                {"registryType": "oci", "identifier": "ghcr.io/acme/box:1.0", "transport": {"type": "stdio"},
                 "environmentVariables": [{"name": "BOX_KEY", "isRequired": true}]}
            ]
        }),
    ]
}

/// A catalog over the fake registry's entries plus one `config` entry, served at
/// `/catalog.json` and usable as a fixture file. The registry entries point at
/// this server's registry, so `add --catalog` resolves without the network.
pub fn catalog_entries(base: &str) -> Value {
    json!([
        {"id": "remote", "name": "Acme Remote", "category": "Docs and search",
         "summary": "The fake server, from the registry",
         "registry": "io.github.acme/remote", "transport": "http", "auth": "none"},
        {"id": "box", "name": "Acme Box", "category": "Databases",
         "summary": "A sandbox, as a Python package",
         "registry": "io.github.acme/box", "transport": "stdio", "auth": "env"},
        {"id": "files", "name": "Acme Files", "category": "Local files",
         "summary": "Serve one directory; the entry leaves the directory to the user",
         "registry": "io.github.acme/files", "transport": "stdio", "auth": "env"},
        {"id": "fake", "name": "Fake", "category": "Local files",
         "summary": "The fake server, by its URL",
         "config": {"http": format!("{base}/mcp")}, "transport": "http", "auth": "none"}
    ])
}

/// When the fake registry's listings were published, and when the one that
/// changes afterwards did.
pub const REGISTRY_UPDATED: &str = "2026-09-01T00:00:00Z";
pub const REGISTRY_UPDATED_LATER: &str = "2026-09-02T00:00:00Z";

/// How many listings a page of the whole list holds, whatever `limit` asks,
/// so a client that stops after one page is caught.
const REGISTRY_PAGE: usize = 3;

fn registry_meta(updated_at: &str) -> Value {
    json!({"io.modelcontextprotocol.registry/official": {
        "status": "active", "isLatest": true,
        "publishedAt": updated_at, "updatedAt": updated_at}})
}

/// The registry routes mcpdial uses: the list with `search`, the whole list
/// page by page with `cursor`, the changes since a watermark with
/// `updated_since`, and one server's latest version by percent-encoded name.
fn registry(base: &str, path: &str, query: &str) -> Resp {
    let meta = registry_meta(REGISTRY_UPDATED);
    let entries = registry_entries(base);
    if path == "/v0.1/servers" {
        let params = form(query);
        let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if param("updated_since").is_some() {
            // One listing changed since any watermark a client could hold.
            let mut files = entries
                .into_iter()
                .find(|e| e["name"] == "io.github.acme/files")
                .unwrap();
            files["version"] = json!("1.3.0");
            files["description"] = json!("Serve one directory over MCP, now with search.");
            let servers =
                vec![json!({"server": files, "_meta": registry_meta(REGISTRY_UPDATED_LATER)})];
            return json_resp(200, &json!({"servers": servers, "metadata": {"count": 1}}));
        }
        if let Some(needle) = param("search") {
            let needle = needle.to_lowercase();
            let limit: usize = param("limit").and_then(|l| l.parse().ok()).unwrap_or(30);
            let servers: Vec<Value> = entries
                .into_iter()
                .filter(|e| {
                    let text = format!("{} {}", e["name"], e["description"]).to_lowercase();
                    text.contains(&needle)
                })
                .take(limit)
                .map(|e| json!({"server": e, "_meta": meta}))
                .collect();
            let count = servers.len();
            return json_resp(
                200,
                &json!({"servers": servers, "metadata": {"count": count}}),
            );
        }
        let start = param("cursor")
            .and_then(|c| entries.iter().position(|e| e["name"] == c))
            .unwrap_or(0);
        let page: Vec<Value> = entries[start..]
            .iter()
            .take(REGISTRY_PAGE)
            .map(|e| json!({"server": e, "_meta": meta}))
            .collect();
        let mut metadata = json!({"count": page.len()});
        if let Some(next) = entries.get(start + REGISTRY_PAGE) {
            metadata["nextCursor"] = next["name"].clone();
        }
        return json_resp(200, &json!({"servers": page, "metadata": metadata}));
    }
    let name = path
        .strip_prefix("/v0.1/servers/")
        .and_then(|rest| rest.strip_suffix("/versions/latest"));
    match name.and_then(|n| entries.into_iter().find(|e| e["name"] == n)) {
        Some(e) => json_resp(200, &json!({"server": e, "_meta": meta})),
        None => json_resp(
            404,
            &json!({"title": "Not Found", "status": 404, "detail": "Server not found"}),
        ),
    }
}

fn mcp(mode: &Mode, base: &str, rec: &Recorded, state: &Mutex<State>) -> Resp {
    if let Mode::Blocked = mode {
        return Response::from_string("error code: 1010").with_status_code(403);
    }
    if let Mode::LegacySse = mode {
        return legacy_sse(base, rec);
    }
    if matches!(
        mode,
        Mode::Auth { .. }
            | Mode::AuthLocalhostOnly
            | Mode::AuthClientMetadata { .. }
            | Mode::Confidential { .. }
    ) {
        let bearer = rec
            .header("authorization")
            .and_then(|a| a.strip_prefix("Bearer "))
            .unwrap_or("");
        if !state
            .lock()
            .unwrap()
            .valid_tokens
            .iter()
            .any(|t| t == bearer)
        {
            let www = format!(
                "Bearer realm=\"fake\", error=\"invalid_token\", resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
            );
            return with_headers(
                Response::from_string("").with_status_code(401),
                &[("WWW-Authenticate", &www)],
            );
        }
    }

    let stateful = matches!(mode, Mode::Stateful | Mode::StatefulNoDelete);

    if rec.method == "DELETE" {
        if matches!(mode, Mode::StatefulNoDelete) {
            return Response::from_string("").with_status_code(405);
        }
        if stateful && rec.header("mcp-session-id") != Some("sess-1") {
            return json_resp(
                400,
                &json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: No valid session ID provided"},"id":null}),
            );
        }
        state.lock().unwrap().initialized = false;
        return Response::from_string("").with_status_code(204);
    }

    let msg = rec.json();
    let Some(id) = msg.get("id").cloned() else {
        // A notification. Remember that the client finished the handshake.
        if msg["method"] == "notifications/initialized" {
            state.lock().unwrap().initialized = true;
        }
        return Response::from_string("").with_status_code(202);
    };
    let method = msg["method"].as_str().unwrap_or("");

    if stateful && method != "initialize" {
        if rec.header("mcp-session-id") != Some("sess-1") {
            return json_resp(
                400,
                &json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: No valid session ID provided"},"id":null}),
            );
        }
        if !state.lock().unwrap().initialized {
            return json_resp(
                200,
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Server not initialized"}}),
            );
        }
    }

    let params = &msg["params"];
    let agreed_version = match mode {
        Mode::EchoProtocol => params["protocolVersion"].as_str().unwrap_or("?"),
        _ => "2025-06-18",
    };
    let reply = match method {
        "initialize" => json!({"jsonrpc":"2.0","id":id,"result":{
            "protocolVersion":agreed_version,
            "capabilities":{"tools":{},"resources":{},"prompts":{}},
            "serverInfo":{"name":"fake-mcp","version":"1.0"}}}),
        "tools/list" if matches!(mode, Mode::StuckCursor) => stuck_page(&id, state),
        "tools/list" => tools_list(&id, params["cursor"].as_str()),
        "resources/list" => resources_list(&id, params["cursor"].as_str()),
        "resources/templates/list" => json!({"jsonrpc":"2.0","id":id,"result":{
            "resourceTemplates":[{"uriTemplate":"file:///notes/{name}.md","name":"note",
                "description":"One note, by name.","mimeType":"text/markdown"}]}}),
        "resources/read" => resources_read(&id, params["uri"].as_str().unwrap_or("")),
        "prompts/list" => prompts_list(&id, params["cursor"].as_str()),
        "prompts/get" => prompts_get(&id, params),
        "tools/call" => match params["name"].as_str().unwrap_or("") {
            "echo" => json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text",
                "text":format!("Echo: {}", params["arguments"]["message"].as_str().unwrap_or(""))}]}}),
            "add" => {
                let (a, b) = (
                    params["arguments"]["a"].as_f64().unwrap_or(0.0),
                    params["arguments"]["b"].as_f64().unwrap_or(0.0),
                );
                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("The sum of {a} and {b} is {}.", a + b)}],"structuredContent":{"sum": a + b}}})
            }
            // Left out of tools/list on purpose: a third tool renumbers every listing assertion.
            "reading" => {
                json!({"jsonrpc":"2.0","id":id,"result":{"structuredContent":{"celsius":20},"isError":false}})
            }
            "fail" => {
                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"it failed"}],"isError":true}})
            }
            other => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":format!("Tool {other} not found")}})
            }
        },
        other => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Method not found: {other}")}})
        }
    };

    if stateful {
        let server_noise_before_the_answer = if method == "tools/call" {
            ": keep-alive\n\n\
             event: message\n\
             data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\
             \"params\":{\"level\":\"info\",\"data\":\"working\"}}\n\n"
        } else {
            ""
        };
        let sse = format!("{server_noise_before_the_answer}event: message\ndata: {reply}\n\n");
        let mut r = with_headers(
            Response::from_string(sse),
            &[("Content-Type", "text/event-stream")],
        );
        if method == "initialize" {
            r = with_headers(r, &[("Mcp-Session-Id", "sess-1")]);
        }
        r
    } else {
        json_resp(200, &reply)
    }
}

/// The 2024-11-05 handshake, which is a different transport rather than a broken
/// server: the endpoint is registered for `GET`, so a POST finds no route.
///
/// The stream promises a `Content-Length` it never finishes sending, so the client
/// sees what a real one gives it - an opening `endpoint` event and then a
/// connection that stays open indefinitely.
fn legacy_sse(base: &str, rec: &Recorded) -> Resp {
    if rec.method != "GET" {
        return Response::from_string("Method Not Allowed").with_status_code(405);
    }
    let opening_frame = format!("event: endpoint\ndata: {base}/messages?sessionId=sess-1\n\n");
    let never_finished = 4096;
    with_headers(
        Response::new(
            tiny_http::StatusCode(200),
            Vec::new(),
            std::io::Cursor::new(opening_frame.into_bytes()),
            Some(never_finished),
            None,
        ),
        &[("Content-Type", "text/event-stream")],
    )
}

/// One tool per page. An unrecognised cursor is an error rather than an empty
/// page, so a client that mangles the cursor fails loudly.
fn tools_list(id: &Value, cursor: Option<&str>) -> Value {
    let echo = json!({"name":"echo","description":"Echo a message back.\nSecond line.",
        "inputSchema":{"type":"object","properties":{"message":{"type":"string","description":"What to echo"}},"required":["message"]}});
    let add = json!({"name":"add","description":"Add two numbers.",
        "inputSchema":{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]},
        "outputSchema":{"type":"object","properties":{"sum":{"type":"number"}},"required":["sum"]}});
    match cursor {
        None => json!({"jsonrpc":"2.0","id":id,"result":{"tools":[echo],"nextCursor":"page-2"}}),
        Some("page-2") => json!({"jsonrpc":"2.0","id":id,"result":{"tools":[add]}}),
        Some(other) => json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32602,"message":format!("Invalid cursor: {other}")}}),
    }
}

/// The first eight bytes of every PNG, and not valid UTF-8, so a test can prove
/// the bytes reached stdout rather than their base64.
pub const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// One resource per page, paginated exactly as `tools/list` is.
fn resources_list(id: &Value, cursor: Option<&str>) -> Value {
    let readme = json!({"uri":"file:///readme.md","name":"readme",
        "description":"The project readme.\nSecond line.","mimeType":"text/markdown"});
    let logo = json!({"uri":"file:///logo.png","name":"logo",
        "description":"A tiny picture.","mimeType":"image/png"});
    match cursor {
        None => {
            json!({"jsonrpc":"2.0","id":id,"result":{"resources":[readme],"nextCursor":"res-2"}})
        }
        Some("res-2") => json!({"jsonrpc":"2.0","id":id,"result":{"resources":[logo]}}),
        Some(other) => json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32602,"message":format!("Invalid cursor: {other}")}}),
    }
}

fn resources_read(id: &Value, uri: &str) -> Value {
    let contents = match uri {
        "file:///readme.md" => {
            json!([{"uri":uri,"mimeType":"text/markdown","text":"# fake-mcp\nA readme.\n"}])
        }
        "file:///logo.png" => {
            json!([{"uri":uri,"mimeType":"image/png","blob":STANDARD.encode(PNG_MAGIC)}])
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,
                "error":{"code":-32002,"message":format!("Resource not found: {uri}")}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":{"contents":contents}})
}

/// One prompt per page, paginated exactly as `tools/list` is.
fn prompts_list(id: &Value, cursor: Option<&str>) -> Value {
    let summarize = json!({"name":"summarize","description":"Summarize a document.",
        "arguments":[{"name":"text","description":"What to summarize","required":true},
                     {"name":"style","description":"terse or thorough"}]});
    let greet = json!({"name":"greet","description":"Greet someone."});
    match cursor {
        None => {
            json!({"jsonrpc":"2.0","id":id,"result":{"prompts":[summarize],"nextCursor":"prompt-2"}})
        }
        Some("prompt-2") => json!({"jsonrpc":"2.0","id":id,"result":{"prompts":[greet]}}),
        Some(other) => json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32602,"message":format!("Invalid cursor: {other}")}}),
    }
}

fn prompts_get(id: &Value, params: &Value) -> Value {
    let args = &params["arguments"];
    match params["name"].as_str().unwrap_or("") {
        "summarize" => json!({"jsonrpc":"2.0","id":id,"result":{
            "description":"Summarize a document.",
            "messages":[
                {"role":"user","content":{"type":"text",
                    "text":format!("Summarize this: {}", args["text"].as_str().unwrap_or(""))}},
                {"role":"assistant","content":{"type":"text","text":"Sure."}}]}}),
        "greet" => json!({"jsonrpc":"2.0","id":id,"result":{
            "messages":[{"role":"user","content":{"type":"text","text":"Hello."}}]}}),
        other => json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32602,"message":format!("Prompt {other} not found")}}),
    }
}

/// Errors after a few pages so a client that never stops fails the test rather
/// than hanging it.
fn stuck_page(id: &Value, state: &Mutex<State>) -> Value {
    let mut st = state.lock().unwrap();
    st.stuck_cursor_pages_served += 1;
    if st.stuck_cursor_pages_served > 5 {
        return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,
            "message":format!("cursor loop: {} pages asked for", st.stuck_cursor_pages_served)}});
    }
    json!({"jsonrpc":"2.0","id":id,"result":{
        "tools":[{"name":"loop","description":"Served on every page.",
                  "inputSchema":{"type":"object"}}],
        "nextCursor":"stuck"}})
}

/// The client id `authorize` insists on: the administrator's, the metadata URL, or
/// the one `/register` handed out.
fn expected_client_id(mode: &Mode, registered: bool) -> String {
    match mode {
        Mode::Confidential { .. } => CONFIDENTIAL_ID.to_string(),
        Mode::AuthClientMetadata { client_id } if !registered => client_id.clone(),
        _ => "client-abc".to_string(),
    }
}

/// The other half of RFC 6749 section 2.3.1: both halves are form-encoded before
/// they are base64'd, so the separating colon is the only unescaped one.
fn basic_secret(header: &str) -> String {
    let raw = header.strip_prefix("Basic ").expect("Basic credentials");
    let decoded = String::from_utf8(STANDARD.decode(raw).expect("base64")).expect("utf-8");
    let (id, secret) = decoded.split_once(':').expect("id:secret");
    assert_eq!(percent_decode(id), CONFIDENTIAL_ID);
    percent_decode(secret)
}

fn form(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// -- helpers for driving the binary -------------------------------------------

pub fn temp_home(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mcpdial-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn mcpdial(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mcpdial"));
    c.env("MCPDIAL_HOME", home).env_remove("XDG_CONFIG_HOME");
    c
}

/// `cargo test` builds examples alongside the tests; find the one we spawn.
pub fn echo_server() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let candidates = [
        debug.join("examples/echo_server"),
        debug.join("examples/echo_server.exe"),
    ];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .expect("examples/echo_server not built; run `cargo build --examples` first")
}

/// The same path as a command line for `--stdio` or a `stdio:` target. Those go
/// through POSIX word splitting on every platform, so a Windows path spends its
/// backslashes as escapes unless it is quoted.
pub fn echo_command() -> String {
    format!("'{}'", echo_server().display())
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(cmd: &mut Command) -> Out {
    let o = cmd.output().expect("spawn mcpdial");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}
