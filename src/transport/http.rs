//! Streamable HTTP: POST JSON-RPC to one URL, read back JSON or a one-shot SSE frame.

use super::{silent, Logger, Transport};
use crate::protocol::{decode_body, Error, Result, PROTOCOL_VERSION};
use serde_json::Value;
use std::time::Duration;

/// Always send a real browser User-Agent. This is a correctness requirement, not
/// politeness: bot mitigation in front of a server routes default agent strings
/// differently and can hand back a challenge page or a 403. Verified live: GitMCP
/// rejects a default library UA with Cloudflare error 1010.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

pub struct HttpTransport {
    url: String,
    token: Option<String>,
    user_agent: String,
    extra_headers: Vec<(String, String)>,
    agent: ureq::Agent,
    log: Logger,
    /// Set by stateful servers on `initialize`; echoed on every later request.
    /// Stateless servers never send one and we simply never echo one.
    pub session_id: Option<String>,
}

impl HttpTransport {
    pub fn new(url: impl Into<String>) -> Self {
        Self::builder(url).build()
    }

    pub fn builder(url: impl Into<String>) -> HttpTransportBuilder {
        HttpTransportBuilder {
            url: url.into(),
            token: None,
            timeout: Duration::from_secs(60),
            user_agent: USER_AGENT.to_string(),
            extra_headers: Vec::new(),
            log: None,
        }
    }
}

pub struct HttpTransportBuilder {
    url: String,
    token: Option<String>,
    timeout: Duration,
    user_agent: String,
    extra_headers: Vec<(String, String)>,
    log: Option<Logger>,
}

impl HttpTransportBuilder {
    pub fn token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }
    pub fn log(mut self, log: Logger) -> Self {
        self.log = Some(log);
        self
    }
    pub fn build(self) -> HttpTransport {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            // We want the response object for 4xx/5xx so we can read the body and
            // the WWW-Authenticate header, not a bare status error.
            .http_status_as_error(false)
            .build();
        HttpTransport {
            url: self.url,
            token: self.token,
            user_agent: self.user_agent,
            extra_headers: self.extra_headers,
            agent: ureq::Agent::new_with_config(config),
            log: self.log.unwrap_or_else(silent),
            session_id: None,
        }
    }
}

impl Transport for HttpTransport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        let body = payload.to_string();
        (self.log)(&format!("-> POST {}\n   {}", self.url, body));

        let mut req = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            // Advertise both: the server picks the framing.
            .header("Accept", "application/json, text/event-stream")
            .header("User-Agent", &self.user_agent);
        if let Some(t) = &self.token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        if let Some(sid) = &self.session_id {
            req = req
                .header("Mcp-Session-Id", sid)
                .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }

        let mut resp = req.send(&body).map_err(|e| match e {
            ureq::Error::Timeout(_) => Error::transport(format!("no reply from {} in time", self.url)),
            other => Error::transport(format!("could not reach {}: {other}", self.url)),
        })?;

        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header("content-type").unwrap_or_default();
        let www_authenticate = header("www-authenticate");
        let session_id = header("mcp-session-id");

        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::transport(format!("could not read response body: {e}")))?;
        (self.log)(&format!("<- HTTP {status} {content_type}\n   {}", text.trim()));

        if !(200..300).contains(&status) {
            return Err(Error::Http { status, body: text, www_authenticate });
        }
        if let Some(sid) = session_id {
            self.session_id = Some(sid);
        }
        decode_body(&text, &content_type)
    }
}
