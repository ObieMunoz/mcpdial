//! Streamable HTTP: POST JSON-RPC to one URL, read back JSON or a one-shot SSE frame.

use super::{silent, Logger, Transport};
use crate::protocol::{decode_body, Error, KnownVersion, Result};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

/// Always send a real browser User-Agent. This is a correctness requirement, not
/// politeness: bot mitigation in front of a server routes default agent strings
/// differently and can hand back a challenge page or a 403. Verified live: GitMCP
/// rejects a default library UA with Cloudflare error 1010.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Deliberately not `--timeout`: this runs from a `Drop` on the way out, where a
/// server that will not answer promptly is not worth waiting for.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Also deliberately not `--timeout`: a 2024-11-05 endpoint names its POST URL in
/// the first frame it sends, so anything slower than this is not one, and `ls` has
/// no business stalling over a request whose answer it already has.
const LEGACY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How far into that stream to read before deciding the event is not coming.
const LEGACY_PROBE_LIMIT: usize = 4096;

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
    /// What the session settled on at `initialize`. Its presence is also the
    /// handshake flag: the spec puts `MCP-Protocol-Version` on every request after
    /// initialization and none on `initialize` itself, which has nothing negotiated yet.
    negotiated_version: Option<KnownVersion>,
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

    fn identify<A>(&self, mut req: ureq::RequestBuilder<A>) -> ureq::RequestBuilder<A> {
        req = req.header("User-Agent", &self.user_agent);
        if let Some(t) = &self.token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(version) = self.negotiated_version {
            req = req.header("MCP-Protocol-Version", version.as_str());
        }
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        req
    }

    /// Whether this URL is serving protocol 2024-11-05's transport: a `GET` that
    /// answers with a stream whose opening event names a separate POST endpoint.
    ///
    /// Streamable HTTP servers answer `GET` with a stream too - that is where a
    /// server puts messages it starts - so the event name, not the content type, is
    /// what tells the two transports apart.
    fn speaks_legacy_sse(&mut self) -> bool {
        let req = self
            .identify(
                self.agent
                    .get(&self.url)
                    .config()
                    .timeout_global(Some(LEGACY_PROBE_TIMEOUT))
                    .build(),
            )
            .header("Accept", "text/event-stream");

        (self.log)(&format!("-> GET {} (legacy transport probe)", self.url));
        let Ok(mut resp) = req.call() else {
            return false;
        };
        let content_type = header(&resp, "content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        (self.log)(&format!(
            "<- HTTP {} {content_type}",
            resp.status().as_u16()
        ));

        resp.status().is_success()
            && content_type.contains("text/event-stream")
            && names_a_post_endpoint(&mut resp.body_mut().as_reader())
    }
}

pub(crate) fn header<B>(resp: &ureq::http::Response<B>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
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
            // A redirect turns the POST into a GET and the reply into a web page.
            // Surface it so the user fixes the URL instead of chasing a phantom.
            .max_redirects(0)
            .build();
        HttpTransport {
            url: self.url,
            token: self.token,
            user_agent: self.user_agent,
            extra_headers: self.extra_headers,
            agent: ureq::Agent::new_with_config(config),
            log: self.log.unwrap_or_else(silent),
            session_id: None,
            negotiated_version: None,
        }
    }
}

/// MCP endpoints must be called at their final URL: a redirect would turn the POST
/// into a GET and the JSON-RPC reply into a web page.
pub fn redirect_error(url: &str, status: u16, location: Option<&str>) -> Error {
    match location {
        Some(to) => Error::transport(format!(
            "{url} redirected ({status}) to {to}\nHint: use that URL instead; MCP servers must be called at their final address."
        )),
        None => Error::transport(format!("{url} redirected ({status}) with no Location header")),
    }
}

/// The phrase [`is_legacy_sse_error`] keys off. `ls` classifies a server from the
/// error and nothing else, and by the time one reaches it a transport failure is a
/// sentence.
const LEGACY_SSE_TRANSPORT: &str = "the legacy HTTP+SSE transport (MCP 2024-11-05)";

/// A URL that turned out to be serving the transport MCP retired: replies arrive on
/// a long-lived `GET` stream instead of in the POST response, so no amount of
/// retrying the POST will ever get an answer out of it.
pub fn legacy_sse_error(url: &str) -> Error {
    Error::transport(format!(
        "{url} speaks {LEGACY_SSE_TRANSPORT}, which mcpdial does not.\n\
         Hint: its GET stream names a separate POST endpoint for requests. Ask for a \
         Streamable HTTP URL, or put a bridge such as mcp-remote in front of this one."
    ))
}

pub fn is_legacy_sse_error(detail: &str) -> bool {
    detail.contains(LEGACY_SSE_TRANSPORT)
}

/// Whether a failed POST has failed the way a 2024-11-05 endpoint fails, and is
/// worth one `GET` to find out.
///
/// Such an endpoint is registered for `GET` alone, so a POST to it lands on no
/// route at all: 404 or 405, or a 400 from a server that reads the body before it
/// notices there is no stream behind it. 401 and 403 are left out on purpose. Both
/// already say something true and actionable, and a wrong "this is a legacy server"
/// costs the user more than the generic message it would replace.
fn failed_like_a_legacy_endpoint(status: u16, www_authenticate: Option<&str>) -> bool {
    matches!(status, 400 | 404 | 405) && www_authenticate.is_none()
}

/// Read just far enough into an SSE stream to see whether it opens with the
/// `endpoint` event.
///
/// The legacy stream is long-lived by design - it names the POST endpoint and then
/// stays open for the rest of the session - so reading it to the end would never
/// return.
fn names_a_post_endpoint(stream: &mut impl Read) -> bool {
    let mut seen = String::new();
    let mut chunk = [0u8; 256];
    while seen.len() < LEGACY_PROBE_LIMIT {
        let Ok(n @ 1..) = stream.read(&mut chunk) else {
            break;
        };
        seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
        if seen.lines().any(is_endpoint_event) {
            return true;
        }
    }
    false
}

fn is_endpoint_event(line: &str) -> bool {
    line.strip_prefix("event:")
        .is_some_and(|name| name.trim() == "endpoint")
}

impl Transport for HttpTransport {
    fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
        let body = payload.to_string();
        (self.log)(&format!("-> POST {}\n   {}", self.url, body));

        let req = self.identify(
            self.agent
                .post(&self.url)
                .header("Content-Type", "application/json")
                // Advertise both: the server picks the framing.
                .header("Accept", "application/json, text/event-stream"),
        );

        let mut resp = req.send(&body).map_err(|e| match e {
            ureq::Error::Timeout(_) => {
                Error::transport(format!("no reply from {} in time", self.url))
            }
            other => Error::transport(format!("could not reach {}: {other}", self.url)),
        })?;

        let status = resp.status().as_u16();
        let content_type = header(&resp, "content-type").unwrap_or_default();
        let www_authenticate = header(&resp, "www-authenticate");
        let session_id = header(&resp, "mcp-session-id");
        let location = header(&resp, "location");

        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::transport(format!("could not read response body: {e}")))?;
        (self.log)(&format!(
            "<- HTTP {status} {content_type}\n   {}",
            text.trim()
        ));

        if (300..400).contains(&status) {
            return Err(redirect_error(&self.url, status, location.as_deref()));
        }
        if !(200..300).contains(&status) {
            let never_spoke_streamable_http = self.negotiated_version.is_none();
            let worth_probing = never_spoke_streamable_http
                && failed_like_a_legacy_endpoint(status, www_authenticate.as_deref());
            if worth_probing && self.speaks_legacy_sse() {
                return Err(legacy_sse_error(&self.url));
            }
            return Err(Error::Http {
                status,
                body: text,
                www_authenticate,
            });
        }
        if let Some(sid) = session_id {
            self.session_id = Some(sid);
        }
        decode_body(&text, &content_type)
    }

    fn negotiated(&mut self, version: KnownVersion) {
        self.negotiated_version = Some(version);
    }

    /// End the session server-side, as Streamable HTTP prescribes; without it the
    /// server holds it until its own timeout, long after the user has gone.
    ///
    /// Runs from `Session::drop`, so no outcome may reach the user: a `405` is the
    /// server declining client-side termination, which the spec allows, and any
    /// other error is a session we cannot tidy up on the way out anyway.
    fn close(&mut self) {
        let Some(sid) = self.session_id.clone() else {
            return;
        };
        let req = self.identify(
            self.agent
                .delete(&self.url)
                .config()
                .timeout_global(Some(CLOSE_TIMEOUT))
                .build(),
        );
        // After `identify` has read it into the header, before the request goes
        // out: a second close then finds no session and sends nothing.
        self.session_id = None;

        (self.log)(&format!("-> DELETE {} (session {sid})", self.url));
        match req.call() {
            Ok(resp) => (self.log)(&format!("<- HTTP {}", resp.status().as_u16())),
            Err(e) => (self.log)(&format!("<- session {sid} not terminated: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Repeats one frame and never reaches EOF, the way the legacy endpoint's own
    /// stream does not.
    struct Endless(&'static str);

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len());
            buf[..n].copy_from_slice(&self.0.as_bytes()[..n]);
            Ok(n)
        }
    }

    /// One byte per read, so the event name straddles as many reads as it has bytes.
    struct Dribble<R>(R);

    impl<R: Read> Read for Dribble<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(&mut buf[..1])
        }
    }

    #[test]
    fn the_endpoint_event_is_found_without_draining_the_stream() {
        let opening = "event: endpoint\ndata: /messages?sessionId=abc\n\n";
        let mut stream = Cursor::new(opening).chain(Endless(": keep-alive\n\n"));
        assert!(names_a_post_endpoint(&mut stream));
    }

    #[test]
    fn a_stream_that_never_names_an_endpoint_gives_up() {
        let mut server_started_messages =
            Endless("event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n");
        assert!(!names_a_post_endpoint(&mut server_started_messages));
    }

    #[test]
    fn the_event_name_is_found_when_it_straddles_reads() {
        let mut stream = Dribble(Cursor::new("event: endpoint\ndata: /messages\n\n"));
        assert!(names_a_post_endpoint(&mut stream));
    }

    #[test]
    fn an_empty_stream_names_no_endpoint() {
        assert!(!names_a_post_endpoint(&mut Cursor::new("")));
        assert!(!names_a_post_endpoint(&mut Cursor::new(
            "event: endpoints\n\n"
        )));
    }

    #[test]
    fn only_a_post_that_found_no_route_is_probed() {
        assert!(failed_like_a_legacy_endpoint(404, None));
        assert!(failed_like_a_legacy_endpoint(405, None));
        assert!(failed_like_a_legacy_endpoint(400, None));
        assert!(!failed_like_a_legacy_endpoint(401, None));
        assert!(!failed_like_a_legacy_endpoint(403, None));
        assert!(!failed_like_a_legacy_endpoint(500, None));
        assert!(!failed_like_a_legacy_endpoint(400, Some("Bearer realm=x")));
    }

    #[test]
    fn the_legacy_error_names_the_transport_and_the_url() {
        let e = legacy_sse_error("https://example.test/sse").to_string();
        assert!(e.contains("https://example.test/sse"), "{e}");
        assert!(e.contains("2024-11-05"), "{e}");
        assert!(is_legacy_sse_error(&e));
        assert!(!is_legacy_sse_error(
            "could not reach https://example.test/sse"
        ));
    }
}
