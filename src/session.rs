//! Request id allocation, bringing a session up on either era of the protocol,
//! and the methods that matter.

use crate::notify::{self, Notice};
use crate::protocol::{
    check, classify, input_required, is_modern_error, negotiate, notification, request, rpc_error,
    with_client_meta, with_input_responses, with_progress_token, Error, Incoming, InputRequired,
    KnownVersion, Responder, Result, CLIENT_NAME, CLIENT_VERSION, META_SERVER_INFO, PING,
    UNSUPPORTED_PROTOCOL_VERSION,
};
use crate::transport::Transport;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Backstop for a server that mints a fresh cursor forever, which the
/// repeated-cursor check below cannot catch.
const MAX_PAGES: usize = 1000;

/// The most suggestions one `completion/complete` result is read for, which is
/// the cap the spec puts on a server. Anything past it is dropped rather than
/// handed to a line editor, and the result says there was more.
const MOST_COMPLETIONS: usize = 100;

/// How many times one request may go again carrying more input before mcpdial
/// stops sending it.
///
/// 2026-07-28 lets a server answer a request with a demand for input as often as
/// it likes, and one that always does would keep a client going round for as
/// long as it had patience. Two rounds cover every flow the spec draws; the rest
/// is headroom for a server that asks for one thing at a time.
const MAX_INPUT_ROUNDS: usize = 4;

/// What a caller does with the notifications a server sends while its request
/// is still in flight.
///
/// This is the hook the presentation hangs off: the session routes, the
/// watcher decides what any of it looks like, and a run with nobody watching
/// pays for none of it.
pub trait Watcher {
    /// Whether the request should carry a `progressToken` at all. Answering
    /// `true` is what invites the server to report as it goes, so a watcher
    /// with nowhere to put the answer says `false` and the server is spared
    /// the work.
    fn wants_progress(&self) -> bool {
        false
    }

    fn notice(&mut self, notice: &Notice<'_>);

    /// The server has asked us something - mid-request, or in a 2026-07-28
    /// result demanding input before it will answer - and whatever the client
    /// installed to answer it may need the screen: an elicitation puts its
    /// question and its prompts on stderr, where a watcher's updating line is.
    /// A watcher drawing one finishes it here, so the question starts clean.
    ///
    /// `ping` is not one of these: [`crate::protocol::answer_with`] replies to
    /// it before any responder sees it, so it interrupts nobody.
    fn interrupted(&mut self) {}
}

/// The watcher for a request nobody is watching: no token goes out, and
/// anything that arrives anyway is dropped.
pub struct Unwatched;

impl Watcher for Unwatched {
    fn notice(&mut self, _notice: &Notice<'_>) {}
}

pub struct Session<T: Transport> {
    pub transport: T,
    next_id: u64,
    /// What the server said about itself: the `server/discover` result, or the
    /// `initialize` one, once [`Session::open`] has run.
    pub server_info: Value,
    /// The version the session runs on; until it is up, the one it will try.
    version: KnownVersion,
    /// Set by [`Session::offering`]: the caller named a revision, so the era is
    /// not worked out from how the server answers and not changed behind them.
    pinned: bool,
    /// What `initialize` declares this client can do. Empty until something is
    /// installed that can actually serve a request the server makes back.
    capabilities: Value,
    /// What answers a 2026-07-28 server's demand for input, and the capabilities
    /// every request of that era declares to invite the demand. `None` declares
    /// nothing, which is the only honest thing to send when nothing here can
    /// answer.
    answering: Option<Answering>,
}

/// What serves an [`InputRequired`] demand, kept beside the capabilities that
/// invite it so that neither can be installed without the other.
struct Answering {
    capabilities: Value,
    respond: Responder,
}

impl<T: Transport> Session<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 0,
            server_info: Value::Null,
            version: KnownVersion::LATEST,
            pinned: false,
            capabilities: json!({}),
            answering: None,
        }
    }

    /// Declare these client capabilities at `initialize` instead of none. A
    /// capability declared without a handler behind it is a promise to a server
    /// that then blocks on an answer nobody sends, so this belongs beside
    /// [`crate::Transport::answer_requests`] and nowhere else.
    ///
    /// This is the older era's declaration only. 2026-07-28 has no
    /// server-initiated request to declare for, and declares [`Self::answering`]
    /// instead.
    pub fn declaring(mut self, capabilities: Value) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Declare `capabilities` on every 2026-07-28 request, with `respond`
    /// serving what a server asks for under them.
    ///
    /// Before that revision a server's question is a request it sends down the
    /// connection mid-call, so what answers it goes to the transport and a
    /// transport with no way to send a reply refuses to take it - which is why
    /// Streamable HTTP declared nothing. From 2026-07-28 the question comes back
    /// as a *result* instead, answered by sending the request again, so what
    /// answers it lives here and every transport can carry the answer.
    pub fn answering(mut self, capabilities: Value, respond: Responder) -> Self {
        self.answering = Some(Answering {
            capabilities,
            respond,
        });
        self
    }

    /// Speak `version` rather than working out what the server wants, for a
    /// server that misbehaves when offered something it has never heard of, or
    /// one that serves both eras and should be held to the older.
    pub fn offering(mut self, version: KnownVersion) -> Self {
        self.version = version;
        self.pinned = true;
        self
    }

    /// The protocol version this session runs on, for gating what is asked of the
    /// server: `session.version() >= KnownVersion::V2025_11_25`.
    pub fn version(&self) -> KnownVersion {
        self.version
    }

    /// What every 2026-07-28 request declares this client can be asked for:
    /// exactly what [`Self::answering`] installed something to serve, and
    /// nothing when it installed nothing.
    fn declares(&self) -> Value {
        match &self.answering {
            Some(a) => a.capabilities.clone(),
            None => json!({}),
        }
    }

    /// Send a request and return its `result`. JSON-RPC errors become [`crate::Error::Rpc`].
    ///
    /// From 2026-07-28 on there is no handshake standing behind a request, so
    /// each one carries the protocol version, the client's identity and its
    /// capabilities itself.
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.request_watching(method, params, &mut Unwatched)
    }

    /// A request with somebody listening to what the server says while it runs,
    /// sent again for as long as the server answers with a demand for input
    /// rather than a result.
    ///
    /// Each round is a request in its own right, with its own id, as
    /// 2026-07-28 requires: the demand carries everything the retry needs, which
    /// is what lets a server ask a question without holding a connection or a
    /// session open to hear the answer.
    pub fn request_watching(
        &mut self,
        method: &str,
        params: Option<Value>,
        watch: &mut dyn Watcher,
    ) -> Result<Value> {
        let mut result = self.send(method, params.clone(), watch)?;
        for _ in 0..MAX_INPUT_ROUNDS {
            let Some(asked) = self.asked_for_input(&result) else {
                return Ok(result);
            };
            let answers = self.answer(method, &asked, watch)?;
            let again = with_input_responses(params.clone(), answers, asked.state.as_deref());
            result = self.send(method, Some(again), watch)?;
        }
        match self.asked_for_input(&result) {
            None => Ok(result),
            Some(_) => Err(Error::transport(format!(
                "{method} asked for input {MAX_INPUT_ROUNDS} times running and is still asking; \
                 mcpdial stopped rather than go round again"
            ))),
        }
    }

    /// One request and the result it answered with, whatever kind of result that
    /// is.
    ///
    /// `raw` is what this is for: it was asked to send one request, and a result
    /// demanding input is a fact about the server worth seeing rather than
    /// something to answer behind the caller's back.
    pub fn request_once(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.send(method, params, &mut Unwatched)
    }

    /// A demand for input, if that is what the server answered with. Only
    /// 2026-07-28 has one to send, and the spec confines it to `tools/call`,
    /// `resources/read` and `prompts/get`; an older server's result is a result
    /// whatever `resultType` it happens to carry.
    fn asked_for_input(&self, result: &Value) -> Option<InputRequired> {
        match self.version.is_modern() {
            true => input_required(result),
            false => None,
        }
    }

    /// Serve every request a demand carries, under the keys it named them by.
    ///
    /// The spec has a server ask only for what the request declared, so anything
    /// else is a server asking for something that was never promised, and saying
    /// so beats sending a retry it will only refuse.
    fn answer(
        &mut self,
        method: &str,
        asked: &InputRequired,
        watch: &mut dyn Watcher,
    ) -> Result<Map<String, Value>> {
        if asked.requests.is_empty() {
            return Ok(Map::new());
        }
        let Some(answering) = self.answering.as_mut() else {
            return Err(Error::transport(format!(
                "{method} came back asking for input, which mcpdial had declared it could not \
                 supply"
            )));
        };
        // Whatever answers is about to write to stderr, where a watcher's
        // updating line is.
        watch.interrupted();
        let mut answers = Map::new();
        for (key, asking) in &asked.requests {
            let wants = asking["method"].as_str().unwrap_or_default();
            let answer = (answering.respond)(wants, &asking["params"]).ok_or_else(|| {
                Error::transport(format!(
                    "{method} asked for {wants:?} first, which mcpdial does not answer"
                ))
            })?;
            answers.insert(key.clone(), answer);
        }
        Ok(answers)
    }

    /// One round trip: the request as the era wants it sent, and whatever the
    /// server answered.
    ///
    /// The `progressToken` rides only when `watch` says it wants progress: a
    /// server handed one is entitled to stream notifications at us, and asking
    /// for a stream nobody will read is asking a server to do work for nothing.
    /// Whatever else arrives - a stale token, a `ping`, a notification of a kind
    /// nothing here renders - is passed over, never raised as an error, and
    /// never allowed to stand in for the result.
    fn send(
        &mut self,
        method: &str,
        params: Option<Value>,
        watch: &mut dyn Watcher,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let token = watch.wants_progress().then(|| json!(id));
        let params = match token.is_some() {
            true => Some(with_progress_token(params, id)),
            false => params,
        };
        let params = match self.version.is_modern() {
            true => Some(with_client_meta(params, self.version, &self.declares())),
            false => params,
        };
        let sent = request(method, id, params);
        let reply = self.transport.send_watching(&sent, &mut |from_server| {
            // `awaiting` only sorts a response from a stranger's, and neither is
            // what is being looked for here.
            match classify(from_server, None) {
                Incoming::ServerRequest { method, .. } if method != PING => watch.interrupted(),
                _ => {
                    if let Some(notice) = notify::read(from_server, token.as_ref()) {
                        watch.notice(&notice);
                    }
                }
            }
        })?;
        let msg = check(reply)?;
        Ok(msg
            .and_then(|mut m| m.get_mut("result").map(Value::take))
            .unwrap_or_else(|| json!({})))
    }

    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        self.transport.send(&notification(method, params))?;
        Ok(())
    }

    /// Bring the session up and return what the server said about itself.
    ///
    /// 2026-07-28 removed the handshake: `server/discover` is all there is to ask,
    /// and what `initialize` used to settle once now rides on every request. Which
    /// era a server speaks cannot be asked in the abstract, so it is read off how
    /// it answers the newer request - which is also the request a server that
    /// speaks the newer revision wanted first anyway.
    pub fn open(&mut self) -> Result<&Value> {
        if !self.version.is_modern() {
            return self.initialize();
        }
        let refused = match self.discover() {
            Ok(()) => return Ok(&self.server_info),
            Err(e) => e,
        };
        match self.instead_of_discovering(&refused)? {
            Some(version) => {
                self.version = version;
                self.initialize()
            }
            None => Err(refused),
        }
    }

    /// Which revision to try `initialize` with after `server/discover` was
    /// refused, or `None` to let the refusal stand.
    ///
    /// A `-32022` is a server that does speak 2026-07-28 saying it will not speak
    /// it with us, and naming what it will; the newest of those with a handshake
    /// is the one to fall back to, because the only modern revision mcpdial has
    /// is the one just refused. The other two codes that revision defines are a
    /// server complaining about the request, which falling back would only hide.
    /// Anything else that reads as "no such method here" - a JSON-RPC error of
    /// any other code, or the status an endpoint gives a request it has no route
    /// for - is a server from before `server/discover` existed. A refused
    /// credential, an unreachable host, or a transport mcpdial does not speak is
    /// none of these and stays the answer.
    fn instead_of_discovering(&self, refused: &Error) -> Result<Option<KnownVersion>> {
        let answered = rpc_error(refused);
        let code = answered.as_ref().and_then(|e| e["code"].as_i64());
        if code == Some(UNSUPPORTED_PROTOCOL_VERSION) {
            let named = answered.map_or(Value::Null, |e| e["data"]["supported"].clone());
            let usable = (!self.pinned)
                .then(|| newest_with_a_handshake(&named))
                .flatten();
            return usable
                .ok_or_else(|| speaks_neither(&named, self.version))
                .map(Some);
        }
        if self.pinned || code.is_some_and(is_modern_error) {
            return Ok(None);
        }
        let reads_as_no_such_method = match refused {
            Error::Rpc { .. } => true,
            Error::Http { status, .. } => matches!(status, 400 | 404 | 405),
            _ => false,
        };
        Ok(reads_as_no_such_method.then_some(KnownVersion::LATEST_LEGACY))
    }

    /// `server/discover`: identity, capabilities and supported versions in one
    /// request, and the whole of what 2026-07-28 has instead of a handshake.
    ///
    /// What it answers is kept in the shape the `initialize` result had, with the
    /// agreed `protocolVersion` and a top-level `serverInfo`, so that everything
    /// reading it keeps reading it. What the revision added stays where the
    /// server put it, under `supportedVersions`, `resultType` and `_meta`.
    fn discover(&mut self) -> Result<()> {
        let mut found = self.request("server/discover", None)?;
        let Some(fields) = found.as_object_mut() else {
            return Err(Error::transport(format!(
                "server/discover answered with {found}, which is not a discovery result"
            )));
        };
        let named_itself = fields
            .get("_meta")
            .map(|meta| meta[META_SERVER_INFO].clone())
            .filter(Value::is_object);
        if let Some(info) = named_itself {
            fields.entry("serverInfo").or_insert(info);
        }
        fields.insert("protocolVersion".into(), json!(self.version.as_str()));
        self.server_info = found;
        Ok(())
    }

    /// The handshake, for a server from 2025-11-25 or earlier. Stateless servers
    /// ignore `notifications/initialized`; stateful ones refuse everything that
    /// follows if it is missing.
    ///
    /// A server that answers with a version we do not speak is an error here, and
    /// the notification is withheld: the spec has the client disconnect instead.
    pub fn initialize(&mut self) -> Result<&Value> {
        // A revision with no `initialize` cannot be offered at one, and settling
        // the version before the request is also what keeps the per-request
        // metadata of the newer era off a handshake that has no place for it.
        let offered = self.version.min(KnownVersion::LATEST_LEGACY);
        self.version = offered;
        let info = self.request(
            "initialize",
            Some(json!({
                "protocolVersion": offered.as_str(),
                "capabilities": self.capabilities.clone(),
                "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
            })),
        )?;
        self.version = negotiate(offered, &info["protocolVersion"])?;
        self.server_info = info;
        self.transport.negotiated(self.version);
        self.notify("notifications/initialized", None)?;
        Ok(&self.server_info)
    }

    /// Every `*/list` method answers with items under `key` and an optional
    /// `nextCursor`, handed back as `{"cursor": "..."}`. Stopping at the first page
    /// loses the rest with no error: a tool that is really there reads as unknown.
    pub fn list_paginated(&mut self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut items: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let params = cursor.take().map(|c| json!({ "cursor": c }));
            let mut res = self.request(method, params)?;
            if let Some(page) = res.get_mut(key).and_then(Value::as_array_mut) {
                items.append(page);
            }
            let cursor_to_a_further_page = res
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty());
            match cursor_to_a_further_page {
                Some(c) if seen.insert(c.to_string()) => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(items)
    }

    pub fn list_tools(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("tools/list", "tools")
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.call_tool_watching(name, arguments, &mut Unwatched)
    }

    /// The three methods that can take long enough to be worth reporting on are
    /// the three that take a watcher: a tool call, a prompt, and a resource
    /// read. Everything else is a listing or a handshake and answers at once.
    pub fn call_tool_watching(
        &mut self,
        name: &str,
        arguments: Value,
        watch: &mut dyn Watcher,
    ) -> Result<Value> {
        self.request_watching(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments })),
            watch,
        )
    }

    /// The same call, run by the server in the background: the result is a task
    /// object carrying an id, not the tool's own result, and
    /// [`Self::task_result`] fetches that once the task is terminal.
    ///
    /// `ttl` is how long the server is asked to keep the task after it ends;
    /// what it agreed to is in the `ttl` of the object it answers with.
    pub fn call_tool_as_task(
        &mut self,
        name: &str,
        arguments: Value,
        ttl: Duration,
    ) -> Result<Value> {
        let ttl = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        self.request(
            "tools/call",
            Some(json!({ "name": name, "arguments": arguments, "task": { "ttl": ttl } })),
        )
    }

    /// One task's status, without waiting for it to change.
    pub fn get_task(&mut self, id: &str) -> Result<Value> {
        self.request("tasks/get", Some(json!({ "taskId": id })))
    }

    /// The result of the operation the task was started for, once it is
    /// terminal. The server holds this request open until then, which is why it
    /// takes a watcher: whatever it reports while we wait is worth showing.
    pub fn task_result(&mut self, id: &str, watch: &mut dyn Watcher) -> Result<Value> {
        self.request_watching("tasks/result", Some(json!({ "taskId": id })), watch)
    }

    pub fn list_tasks(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("tasks/list", "tasks")
    }

    pub fn cancel_task(&mut self, id: &str) -> Result<Value> {
        self.request("tasks/cancel", Some(json!({ "taskId": id })))
    }

    pub fn list_resources(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("resources/list", "resources")
    }

    /// Templates are RFC 6570 URI templates rather than URIs, and arrive from their
    /// own method: expanding one is the caller's job before [`Self::read_resource`].
    pub fn list_resource_templates(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("resources/templates/list", "resourceTemplates")
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.read_resource_watching(uri, &mut Unwatched)
    }

    pub fn read_resource_watching(&mut self, uri: &str, watch: &mut dyn Watcher) -> Result<Value> {
        self.request_watching("resources/read", Some(json!({ "uri": uri })), watch)
    }

    pub fn list_prompts(&mut self) -> Result<Vec<Value>> {
        self.list_paginated("prompts/list", "prompts")
    }

    /// `completion/complete`: what this server suggests for `argument` of
    /// `reference`, given whatever has been settled already.
    ///
    /// `reference` is `{"type": "ref/prompt", "name": NAME}` or
    /// `{"type": "ref/resource", "uri": TEMPLATE}`, and `argument` is
    /// `{"name": NAME, "value": what has been typed so far}`. `context` carries
    /// `{"arguments": {...}}`, the arguments already given, which a server may
    /// narrow its answer by; the field is 2025-11-25's and older servers ignore
    /// it.
    ///
    /// Only a server that declares `completions` has this method. Which era
    /// [`Self::open`] settled on makes no difference to that - both put the
    /// capability in `server_info["capabilities"]` - so ask there before asking
    /// here.
    pub fn complete(
        &mut self,
        reference: Value,
        argument: Value,
        context: Option<Value>,
    ) -> Result<Completion> {
        let mut params = json!({ "ref": reference, "argument": argument });
        if let Some(context) = context {
            params["context"] = context;
        }
        let result = self.request("completion/complete", Some(params))?;
        Ok(Completion::read(&result))
    }

    /// [`Self::complete`], abandoned once `within` is up, with the transport's
    /// own timeout put back however it ends.
    ///
    /// This is the form Tab uses. A completion is one keystroke's worth of
    /// help, and a server that is slow, wedged or gone must cost the person
    /// typing that much and no more.
    pub fn complete_within(
        &mut self,
        within: Duration,
        reference: Value,
        argument: Value,
        context: Option<Value>,
    ) -> Result<Completion> {
        let restore = self.transport.wait_at_most(Some(within));
        let found = self.complete(reference, argument, context);
        self.transport.wait_at_most(restore);
        found
    }

    pub fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.get_prompt_watching(name, arguments, &mut Unwatched)
    }

    pub fn get_prompt_watching(
        &mut self,
        name: &str,
        arguments: Value,
        watch: &mut dyn Watcher,
    ) -> Result<Value> {
        self.request_watching(
            "prompts/get",
            Some(json!({ "name": name, "arguments": arguments })),
            watch,
        )
    }

    pub fn close(&mut self) {
        self.transport.close();
    }
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        self.transport.close();
    }
}

/// The newest revision mcpdial speaks out of the ones an `UnsupportedProtocolVersion`
/// error named, ignoring any that has no handshake to reach it by.
fn newest_with_a_handshake(supported: &Value) -> Option<KnownVersion> {
    supported
        .as_array()?
        .iter()
        .filter_map(|v| KnownVersion::parse(v.as_str()?))
        .filter(|v| !v.is_modern())
        .max()
}

/// A server and a client with no revision in common, named from both sides so
/// that the fix is on the screen rather than a code to look up.
fn speaks_neither(supported: &Value, tried: KnownVersion) -> Error {
    let named = match supported.as_array() {
        Some(versions) if !versions.is_empty() => versions
            .iter()
            .map(|v| v.as_str().unwrap_or("?").to_string())
            .collect::<Vec<_>>()
            .join(", "),
        _ => "nothing".to_string(),
    };
    Error::transport(format!(
        "the server refused protocol version {tried} and speaks {named}, which mcpdial does \
         not.\n\
         Hint: offer one it accepts with --protocol-version VERSION."
    ))
}

/// The bytes behind an `image` or `audio` block, an embedded `resource` block with
/// a `blob`, or a `resources/read` entry with one: decoded, with what the server
/// called them.
#[derive(Debug)]
pub struct Media {
    /// The block's `type`; a `resources/read` entry counts as a `resource`.
    pub kind: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Where a media block's bytes go. `Some(path)` means the sink put them there and
/// the path is what gets named; `None` leaves a placeholder in the block's place.
pub type MediaSink<'a> = dyn FnMut(&Media) -> Result<Option<PathBuf>> + 'a;

/// Flatten a `tools/call` result to text, one content block per line. A media
/// block becomes the line [`describe`] makes of it; any other non-text block is
/// emitted as its JSON.
///
/// A tool that declares an `outputSchema` may answer with `structuredContent` and no
/// content blocks at all (2025-06-18). Without the fallback such a result prints as
/// the empty string, indistinguishable from a tool that legitimately returned nothing.
/// What a server suggests for one half-typed value: the values themselves,
/// ranked by the server, and what it says about the ones it did not send.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub values: Vec<String>,
    /// How many there are in all, when the server says.
    pub total: Option<u64>,
    pub has_more: bool,
}

impl Completion {
    /// The `completion` object of a result. Anything missing is nothing rather
    /// than a guess, and a server that sends past [`MOST_COMPLETIONS`] has the
    /// rest dropped and `has_more` set, whatever it claimed.
    fn read(result: &Value) -> Self {
        let completion = &result["completion"];
        let served = completion["values"].as_array().map_or(0, Vec::len);
        let values = completion["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .take(MOST_COMPLETIONS)
            .collect();
        Self {
            values,
            total: completion["total"].as_u64(),
            has_more: completion["hasMore"].as_bool().unwrap_or(false) || served > MOST_COMPLETIONS,
        }
    }
}

pub fn render_content(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let blocks = match result.get("content").and_then(Value::as_array) {
        Some(blocks) => render_blocks(blocks, sink)?,
        None => String::new(),
    };
    Ok(match result.get("structuredContent") {
        Some(structured) if blocks.is_empty() => serde_json::to_string_pretty(structured).unwrap(),
        _ => blocks,
    })
}

/// Flatten a `prompts/get` result to one `role: text` line per message. Content
/// that is not text is treated as in [`render_content`].
pub fn render_messages(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let Some(messages) = result.get("messages").and_then(Value::as_array) else {
        return Ok(String::new());
    };
    let lines = messages
        .iter()
        .map(|m| {
            Ok(format!(
                "{}: {}",
                m["role"].as_str().unwrap_or("?"),
                render_blocks(std::slice::from_ref(&m["content"]), sink)?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(lines.join("\n"))
}

/// The `contents[]` of a `resources/read` result for a reader that will not take
/// bytes: text as it is, and each blob as the line [`describe`] makes of it.
pub fn render_resource(result: &Value, sink: &mut MediaSink) -> Result<String> {
    let mut out = String::new();
    for entry in result["contents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if let Value::String(text) = &entry["text"] {
            out.push_str(text);
        } else if let Some(found) = media(entry)? {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&describe(&found, sink(&found)?.as_deref()));
            out.push('\n');
        }
    }
    Ok(out)
}

/// One entry of a `resources/read` result.
#[derive(Debug)]
pub enum ResourceBody {
    Text(String),
    Bytes(Vec<u8>),
}

/// The `contents[]` of a `resources/read` result, with every base64 `blob` decoded
/// to the bytes the server actually holds. An entry carrying neither `text` nor
/// `blob` has nothing to hand back and is dropped.
pub fn resource_bodies(result: &Value) -> Result<Vec<ResourceBody>> {
    result["contents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| match (&entry["text"], &entry["blob"]) {
            (Value::String(text), _) => Some(Ok(ResourceBody::Text(text.clone()))),
            (_, Value::String(blob)) => Some(
                decode_blob(blob, entry["uri"].as_str().unwrap_or("resource"))
                    .map(ResourceBody::Bytes),
            ),
            _ => None,
        })
        .collect()
}

fn decode_blob(blob: &str, what: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(blob)
        .map_err(|e| Error::transport(format!("{what}: blob is not valid base64 ({e})")))
}

/// Which object holds a block's base64, and under which key.
fn payload_holder(block: &Value) -> Option<(Option<&'static str>, &'static str)> {
    match block["type"].as_str() {
        Some("image") | Some("audio") => Some((None, "data")),
        Some("resource") => Some((Some("resource"), "blob")),
        // A `resources/read` entry has no type and keeps its blob at the top.
        None => Some((None, "blob")),
        _ => None,
    }
}

/// The media in a block, if it is one. A text block, a resource link, or a block
/// with no payload is `None`.
pub fn media(block: &Value) -> Result<Option<Media>> {
    let Some((inner, key)) = payload_holder(block) else {
        return Ok(None);
    };
    let holder = inner.map_or(block, |k| &block[k]);
    let Value::String(encoded) = &holder[key] else {
        return Ok(None);
    };
    let kind = block["type"].as_str().unwrap_or("resource");
    let bytes = decode_blob(encoded, holder["uri"].as_str().unwrap_or(kind))?;
    Ok(Some(Media {
        kind: kind.to_string(),
        mime_type: holder["mimeType"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string(),
        bytes,
    }))
}

/// Hand every media block of a `tools/call`, `prompts/get` or `resources/read`
/// result to `sink`, and where the bytes land in a file, put `path` and `bytes`
/// in the block in place of the base64. Everything else stays the server's.
pub fn save_media(result: &mut Value, sink: &mut MediaSink) -> Result<()> {
    for block in media_blocks_mut(result) {
        let Some(found) = media(block)? else {
            continue;
        };
        let Some(path) = sink(&found)? else {
            continue;
        };
        let (inner, key) = payload_holder(block).expect("a media block has a payload");
        let holder = match inner {
            Some(k) => &mut block[k],
            None => block,
        };
        if let Some(fields) = holder.as_object_mut() {
            fields.remove(key);
            fields.insert("path".into(), json!(path.to_string_lossy()));
            fields.insert("bytes".into(), json!(found.bytes.len()));
        }
    }
    Ok(())
}

/// Every block a result can carry media in, for a reader that is only looking:
/// see [`media_blocks_mut`], which is the same walk for a writer.
pub fn media_blocks(result: &Value) -> Vec<&Value> {
    let Some(fields) = result.as_object() else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("content" | "contents", Value::Array(items)) => blocks.extend(items.iter()),
            ("messages", Value::Array(items)) => {
                blocks.extend(items.iter().filter_map(|m| m.get("content")))
            }
            _ => {}
        }
    }
    blocks
}

/// Every block a result can carry media in: `content[]` of `tools/call`,
/// `messages[].content` of `prompts/get`, and `contents[]` of `resources/read`.
fn media_blocks_mut(result: &mut Value) -> Vec<&mut Value> {
    let Some(fields) = result.as_object_mut() else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for (key, value) in fields.iter_mut() {
        match (key.as_str(), value) {
            ("content" | "contents", Value::Array(items)) => blocks.extend(items.iter_mut()),
            ("messages", Value::Array(items)) => {
                blocks.extend(items.iter_mut().filter_map(|m| m.get_mut("content")))
            }
            _ => {}
        }
    }
    blocks
}

/// The one line that stands in for a media block on stdout.
pub fn describe(media: &Media, saved_to: Option<&Path>) -> String {
    let size = human_size(media.bytes.len());
    match saved_to {
        Some(path) => format!("[{} saved to {}, {size}]", media.kind, path.display()),
        None => format!("[{} {}, {size}]", media.kind, media.mime_type),
    }
}

fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / KB / KB)
    }
}

/// A file extension for a mime type: the usual one where the subtype is not it
/// already, the subtype where that is a plain word, and `bin` otherwise.
pub fn extension_for(mime_type: &str) -> &str {
    let essence = mime_type.split(';').next().unwrap_or("").trim();
    match essence {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" => "m4a",
        "text/plain" => "txt",
        "application/octet-stream" => "bin",
        _ => match essence.split_once('/') {
            Some((_, sub)) if !sub.is_empty() && sub.bytes().all(|c| c.is_ascii_alphanumeric()) => {
                sub
            }
            _ => "bin",
        },
    }
}

fn render_blocks(blocks: &[Value], sink: &mut MediaSink) -> Result<String> {
    let lines = blocks
        .iter()
        .map(|b| match (b["type"].as_str(), b.get("text")) {
            (Some("text"), Some(Value::String(t))) => Ok(t.clone()),
            _ => match media(b)? {
                Some(found) => Ok(describe(&found, sink(&found)?.as_deref())),
                None => Ok(b.to_string()),
            },
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        Error, HEADER_MISMATCH, META_CLIENT_CAPABILITIES, META_CLIENT_INFO, META_PROTOCOL_VERSION,
        METHOD_NOT_FOUND, MISSING_REQUIRED_CLIENT_CAPABILITY,
    };
    use std::collections::VecDeque;

    /// A transport that replays canned replies and records what was sent.
    struct Fake {
        sent: Vec<Value>,
        replies: VecDeque<Option<Value>>,
        told: Option<KnownVersion>,
    }

    impl Transport for Fake {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            self.sent.push(payload.clone());
            Ok(self.replies.pop_front().unwrap_or(None))
        }
        fn negotiated(&mut self, version: KnownVersion) {
            self.told = Some(version);
        }
    }

    fn fake(replies: Vec<Option<Value>>) -> Session<Fake> {
        Session::new(Fake {
            sent: Vec::new(),
            replies: replies.into(),
            told: None,
        })
    }

    /// A session already settled on a revision from before 2026-07-28, for the
    /// wire shapes that have no per-request metadata in them.
    fn legacy(replies: Vec<Option<Value>>) -> Session<Fake> {
        fake(replies).offering(KnownVersion::LATEST_LEGACY)
    }

    #[test]
    fn initialize_sends_the_mandatory_notification() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"x"}}}),
        )]);
        let info = s.initialize().unwrap().clone();
        assert_eq!(info["serverInfo"]["name"], "x");
        let sent = &s.transport.sent;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["id"], 1);
        assert_eq!(sent[1]["method"], "notifications/initialized");
        assert!(
            sent[1].get("id").is_none(),
            "a notification must not carry an id"
        );
    }

    #[test]
    fn initialize_offers_the_newest_version_and_keeps_what_the_server_answers() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}),
        )]);
        assert_eq!(
            s.version(),
            KnownVersion::V2026_07_28,
            "the newest, until a server says otherwise"
        );
        s.initialize().unwrap();
        assert_eq!(
            s.transport.sent[0]["params"]["protocolVersion"],
            "2025-11-25"
        );
        assert_eq!(s.version(), KnownVersion::V2025_06_18);
        assert_eq!(
            s.transport.told,
            Some(KnownVersion::V2025_06_18),
            "the transport hears the agreed version, not the offer"
        );
        assert!(
            s.version() < KnownVersion::V2025_11_25,
            "so gating reads naturally"
        );
    }

    #[test]
    fn a_pinned_version_is_the_one_offered() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}),
        )])
        .offering(KnownVersion::V2025_03_26);
        s.initialize().unwrap();
        assert_eq!(
            s.transport.sent[0]["params"]["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(s.version(), KnownVersion::V2025_03_26);
    }

    #[test]
    fn a_version_we_do_not_speak_ends_the_handshake() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1999-01-01"}}),
        )]);
        let e = s.initialize().unwrap_err();
        assert!(matches!(e, Error::Transport(_)), "{e:?}");
        let text = e.to_string();
        assert!(
            text.contains("1999-01-01") && text.contains("2025-11-25"),
            "{text}"
        );
        assert!(text.contains("--protocol-version"), "{text}");
        assert_eq!(
            s.transport.sent.len(),
            1,
            "no notifications/initialized: the spec says disconnect"
        );
        assert_eq!(s.transport.told, None);
    }

    fn discovered(result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": result})
    }

    fn refused(code: i64, message: &str, data: Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"error":{"code":code,"message":message,"data":data}})
    }

    #[test]
    fn a_server_that_answers_discover_needs_no_handshake() {
        let mut s = fake(vec![Some(discovered(json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "instructions": "Weather, mostly.",
            "_meta": {META_SERVER_INFO: {"name": "modern-mcp", "version": "2.0"}},
        })))]);

        let info = s.open().unwrap().clone();
        assert_eq!(s.version(), KnownVersion::V2026_07_28);
        assert_eq!(s.transport.sent.len(), 1, "no handshake, no notification");
        assert_eq!(s.transport.sent[0]["method"], "server/discover");

        // Read where `initialize` used to answer, so that nothing downstream cares.
        assert_eq!(info["protocolVersion"], "2026-07-28");
        assert_eq!(info["serverInfo"]["name"], "modern-mcp");
        assert_eq!(info["capabilities"]["tools"], json!({}));
        // And what the revision added is left where the server put it.
        assert_eq!(info["supportedVersions"][0], "2026-07-28");
        assert_eq!(info["resultType"], "complete");
    }

    #[test]
    fn every_request_on_the_newest_revision_carries_its_own_metadata() {
        let mut s = fake(vec![
            Some(discovered(json!({"capabilities": {}}))),
            Some(json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}})),
        ]);
        s.open().unwrap();
        s.list_tools().unwrap();

        for sent in &s.transport.sent {
            let meta = &sent["params"]["_meta"];
            assert_eq!(meta[META_PROTOCOL_VERSION], "2026-07-28", "{sent}");
            assert_eq!(meta[META_CLIENT_INFO]["name"], CLIENT_NAME, "{sent}");
            assert_eq!(meta[META_CLIENT_CAPABILITIES], json!({}), "{sent}");
        }
    }

    #[test]
    fn a_server_that_never_heard_of_discover_gets_the_handshake_instead() {
        let mut s = fake(vec![
            Some(
                json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}),
            ),
            Some(
                json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"old"}}}),
            ),
        ]);
        let info = s.open().unwrap().clone();
        assert_eq!(info["serverInfo"]["name"], "old");
        assert_eq!(s.version(), KnownVersion::V2025_06_18);

        let sent = &s.transport.sent;
        assert_eq!(sent[0]["method"], "server/discover");
        assert_eq!(sent[1]["method"], "initialize");
        assert_eq!(sent[1]["params"]["protocolVersion"], "2025-11-25");
        assert!(
            sent[1]["params"]["_meta"].is_null(),
            "the handshake has nowhere to put per-request metadata"
        );
        assert_eq!(sent[2]["method"], "notifications/initialized");
    }

    #[test]
    fn a_server_that_names_the_revisions_it_speaks_is_taken_at_its_word() {
        let mut s = fake(vec![
            Some(refused(
                UNSUPPORTED_PROTOCOL_VERSION,
                "Unsupported protocol version",
                json!({"supported": ["2025-06-18", "2025-11-25"], "requested": "2026-07-28"}),
            )),
            Some(json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25"}})),
        ]);
        s.open().unwrap();
        assert_eq!(s.version(), KnownVersion::V2025_11_25, "the newest of them");
        assert_eq!(
            s.transport.sent[1]["params"]["protocolVersion"],
            "2025-11-25"
        );
    }

    #[test]
    fn a_server_speaking_only_revisions_we_do_not_have_is_told_so() {
        let mut s = fake(vec![Some(refused(
            UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version",
            json!({"supported": ["2027-01-01"], "requested": "2026-07-28"}),
        ))]);
        let e = s.open().unwrap_err().to_string();
        assert!(e.contains("2027-01-01") && e.contains("2026-07-28"), "{e}");
        assert!(e.contains("--protocol-version"), "{e}");
        assert_eq!(s.transport.sent.len(), 1, "no handshake was attempted");
    }

    #[test]
    fn the_other_errors_of_the_newest_revision_are_not_a_reason_to_fall_back() {
        for code in [HEADER_MISMATCH, MISSING_REQUIRED_CLIENT_CAPABILITY] {
            let mut s = fake(vec![Some(refused(code, "no", Value::Null))]);
            let e = s.open().unwrap_err();
            assert!(
                matches!(e, Error::Rpc { code: c, .. } if c == code),
                "{e:?}"
            );
            assert_eq!(s.transport.sent.len(), 1, "the server said it speaks this");
        }
    }

    #[test]
    fn a_pinned_revision_is_the_one_spoken_and_nothing_falls_back_behind_it() {
        let mut old = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}),
        )])
        .offering(KnownVersion::LATEST_LEGACY);
        old.open().unwrap();
        assert_eq!(old.transport.sent[0]["method"], "initialize", "no probing");

        let mut new = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}),
        )])
        .offering(KnownVersion::V2026_07_28);
        assert!(new.open().is_err());
        assert_eq!(new.transport.sent.len(), 1, "no handshake behind the pin");
    }

    #[test]
    fn only_a_refusal_that_reads_as_no_such_method_reaches_for_the_handshake() {
        let s = fake(vec![]);
        let http = |status, body: &str| Error::Http {
            status,
            body: body.to_string(),
            www_authenticate: None,
        };
        let means = |e| s.instead_of_discovering(&e).unwrap();

        let no_such_method = Error::Rpc {
            code: METHOD_NOT_FOUND,
            message: "Method not found".into(),
            data: None,
        };
        assert_eq!(means(no_such_method), Some(KnownVersion::LATEST_LEGACY));
        let no_session = r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"No valid session ID provided"},"id":null}"#;
        assert_eq!(
            means(http(400, no_session)),
            Some(KnownVersion::LATEST_LEGACY)
        );
        assert_eq!(means(http(405, "")), Some(KnownVersion::LATEST_LEGACY));

        // A credential, a policy block, a socket: none of them is an era.
        assert_eq!(means(http(401, "")), None);
        assert_eq!(means(http(403, "")), None);
        assert_eq!(means(http(500, "")), None);
        assert_eq!(means(Error::transport("could not reach it")), None);
    }

    #[test]
    fn a_discovery_result_that_is_not_an_object_is_a_transport_error() {
        let mut s = fake(vec![Some(json!({"jsonrpc":"2.0","id":1,"result":"fine"}))]);
        let e = s.open().unwrap_err();
        assert!(matches!(e, Error::Transport(_)), "{e:?}");
        assert!(e.to_string().contains("discovery result"), "{e}");
    }

    #[test]
    fn ids_increase_and_results_are_unwrapped() {
        let mut s = fake(vec![
            Some(json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}]}})),
            Some(
                json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hi"}]}}),
            ),
        ]);
        assert_eq!(s.list_tools().unwrap()[0]["name"], "a");
        let r = s.call_tool("a", json!({})).unwrap();
        assert_eq!(render_content(&r, &mut placeholders).unwrap(), "hi");
        assert_eq!(s.transport.sent[1]["id"], 2);
        assert_eq!(s.transport.sent[1]["params"]["name"], "a");
    }

    #[test]
    fn a_paginated_list_is_read_to_the_end() {
        let mut s = legacy(vec![
            Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":"c1"}}),
            ),
            Some(
                json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"b"}],"nextCursor":"c2"}}),
            ),
            Some(json!({"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"c"}]}})),
        ]);
        let names: Vec<String> = s
            .list_tools()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a", "b", "c"], "pages arrive in order");

        let sent = &s.transport.sent;
        assert_eq!(sent.len(), 3);
        assert!(
            sent[0].get("params").is_none(),
            "no cursor on the first page"
        );
        assert_eq!(sent[1]["params"]["cursor"], "c1");
        assert_eq!(sent[2]["params"]["cursor"], "c2");
    }

    #[test]
    fn the_helper_works_for_any_list_method() {
        let mut s = fake(vec![
            Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"prompts":[{"name":"a"}],"nextCursor":"c1"}}),
            ),
            Some(json!({"jsonrpc":"2.0","id":2,"result":{"prompts":[{"name":"b"}]}})),
        ]);
        let got = s.list_paginated("prompts/list", "prompts").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(s.transport.sent[1]["method"], "prompts/list");
    }

    #[test]
    fn an_empty_or_null_cursor_ends_the_list() {
        for last in [json!(""), Value::Null] {
            let mut s = fake(vec![Some(
                json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":last}}),
            )]);
            assert_eq!(s.list_tools().unwrap().len(), 1);
            assert_eq!(s.transport.sent.len(), 1, "asked once and stopped");
        }
    }

    #[test]
    fn a_repeated_cursor_stops_the_loop() {
        let stuck =
            json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}],"nextCursor":"same"}});
        let mut s = fake(vec![Some(stuck); 10]);
        assert_eq!(s.list_tools().unwrap().len(), 2);
        assert_eq!(
            s.transport.sent.len(),
            2,
            "the second page repeats a cursor we have already followed"
        );
    }

    #[test]
    fn endless_fresh_cursors_stop_at_the_page_cap() {
        struct Endless(u64);
        impl Transport for Endless {
            fn send(&mut self, _payload: &Value) -> Result<Option<Value>> {
                self.0 += 1;
                Ok(Some(json!({"jsonrpc":"2.0","id":self.0,"result":{
                    "tools":[{"name":format!("t{}", self.0)}],
                    "nextCursor":format!("c{}", self.0)}})))
            }
        }
        let mut s = Session::new(Endless(0));
        assert_eq!(s.list_tools().unwrap().len(), MAX_PAGES);
    }

    #[test]
    fn rpc_errors_surface() {
        let mut s = fake(vec![Some(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}),
        )]);
        assert!(matches!(
            s.request("x", None),
            Err(Error::Rpc { code: -32601, .. })
        ));
    }

    /// The sink of a reader with nowhere to put bytes.
    fn placeholders(_: &Media) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// A sink that records what it was handed and claims to have filed it.
    fn filing(
        seen: &mut Vec<(String, String, usize)>,
    ) -> impl FnMut(&Media) -> Result<Option<PathBuf>> + '_ {
        move |m| {
            seen.push((m.kind.clone(), m.mime_type.clone(), m.bytes.len()));
            Ok(Some(PathBuf::from(format!(
                "out/{}.{}",
                seen.len(),
                extension_for(&m.mime_type)
            ))))
        }
    }

    const PNG: &str = "iVBORw0KGgo=";

    #[test]
    fn render_content_stands_a_placeholder_in_for_an_image() {
        let r = json!({"content":[
            {"type":"text","text":"a"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
            {"type":"audio","data":PNG,"mimeType":"audio/wav"},
            {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","blob":PNG}},
        ]});
        let out = render_content(&r, &mut placeholders).unwrap();
        assert_eq!(
            out,
            "a\n[image image/png, 8 B]\n[audio audio/wav, 8 B]\n[resource application/pdf, 8 B]"
        );
        assert!(!out.contains(PNG), "no base64 on stdout");
        assert_eq!(render_content(&json!({}), &mut placeholders).unwrap(), "");
    }

    #[test]
    fn render_content_names_the_file_the_sink_wrote() {
        let r = json!({"content":[
            {"type":"text","text":"done"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
        ]});
        let mut seen = Vec::new();
        let out = render_content(&r, &mut filing(&mut seen)).unwrap();
        assert_eq!(out, "done\n[image saved to out/1.png, 8 B]");
        assert_eq!(seen, [("image".to_string(), "image/png".to_string(), 8)]);
    }

    #[test]
    fn render_content_still_emits_other_blocks_as_json() {
        let r = json!({"content":[
            {"type":"resource_link","uri":"file:///x","name":"x"},
            {"type":"resource","resource":{"uri":"file:///t.txt","text":"inline"}},
            {"type":"image","mimeType":"image/png"},
        ]});
        let out = render_content(&r, &mut placeholders).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"resource_link\""), "{out}");
        assert!(lines[1].contains("\"inline\""), "{out}");
        assert!(
            lines[2].contains("\"image\""),
            "a block with no data is not media"
        );

        let mangled =
            json!({"content":[{"type":"image","data":"not base64!","mimeType":"image/png"}]});
        let e = render_content(&mangled, &mut placeholders)
            .unwrap_err()
            .to_string();
        assert!(e.contains("image") && e.contains("base64"), "{e}");
    }

    #[test]
    fn render_content_prints_structured_content_when_no_block_renders() {
        let only = json!({"structuredContent":{"temp":20},"isError":false});
        assert_eq!(
            render_content(&only, &mut placeholders).unwrap(),
            "{\n  \"temp\": 20\n}"
        );

        let empty_array = json!({"content":[],"structuredContent":{"temp":20}});
        assert_eq!(
            render_content(&empty_array, &mut placeholders).unwrap(),
            "{\n  \"temp\": 20\n}"
        );

        assert_eq!(
            render_content(&json!({"content":[]}), &mut placeholders).unwrap(),
            ""
        );
    }

    #[test]
    fn render_content_leaves_a_result_with_blocks_untouched() {
        let both = json!({
            "content":[{"type":"text","text":"20 degrees"}],
            "structuredContent":{"temp":20},
        });
        assert_eq!(
            render_content(&both, &mut placeholders).unwrap(),
            "20 degrees"
        );
    }

    #[test]
    fn render_messages_prefixes_each_message_with_its_role() {
        let got = render_messages(
            &json!({"messages":[
                {"role":"user","content":{"type":"text","text":"summarize this"}},
                {"role":"assistant","content":{"type":"text","text":"sure"}},
            ]}),
            &mut placeholders,
        )
        .unwrap();
        assert_eq!(got, "user: summarize this\nassistant: sure");

        let image = render_messages(
            &json!({"messages":[
                {"role":"user","content":{"type":"image","data":PNG,"mimeType":"image/png"}},
            ]}),
            &mut placeholders,
        )
        .unwrap();
        assert_eq!(image, "user: [image image/png, 8 B]");

        assert_eq!(render_messages(&json!({}), &mut placeholders).unwrap(), "");
    }

    #[test]
    fn render_resource_keeps_text_and_files_blobs() {
        let read = json!({"contents":[
            {"uri":"file:///a.txt","text":"hello"},
            {"uri":"file:///b.png","mimeType":"image/png","blob":PNG},
            {"uri":"file:///c.empty"},
        ]});
        let mut seen = Vec::new();
        let out = render_resource(&read, &mut filing(&mut seen)).unwrap();
        assert_eq!(out, "hello\n[resource saved to out/1.png, 8 B]\n");
        assert_eq!(seen[0].0, "resource");
        assert_eq!(
            render_resource(&read, &mut placeholders).unwrap(),
            "hello\n[resource image/png, 8 B]\n"
        );
    }

    #[test]
    fn save_media_swaps_the_base64_for_the_path_and_size() {
        let mut call = json!({"content":[
            {"type":"text","text":"done"},
            {"type":"image","data":PNG,"mimeType":"image/png"},
            {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","blob":PNG}},
        ],"isError":false});
        let mut seen = Vec::new();
        save_media(&mut call, &mut filing(&mut seen)).unwrap();
        assert_eq!(
            call,
            json!({"content":[
                {"type":"text","text":"done"},
                {"type":"image","path":"out/1.png","bytes":8,"mimeType":"image/png"},
                {"type":"resource","resource":{"uri":"file:///x.pdf","mimeType":"application/pdf","path":"out/2.pdf","bytes":8}},
            ],"isError":false})
        );

        let mut prompt = json!({"messages":[
            {"role":"user","content":{"type":"image","data":PNG,"mimeType":"image/png"}},
            {"role":"assistant","content":{"type":"text","text":"sure"}},
        ]});
        save_media(&mut prompt, &mut filing(&mut Vec::new())).unwrap();
        assert_eq!(prompt["messages"][0]["content"]["path"], "out/1.png");
        assert!(prompt["messages"][0]["content"].get("data").is_none());
        assert_eq!(prompt["messages"][1]["content"]["text"], "sure");

        let mut read =
            json!({"contents":[{"uri":"file:///b.png","mimeType":"image/png","blob":PNG}]});
        save_media(&mut read, &mut filing(&mut Vec::new())).unwrap();
        assert_eq!(
            read,
            json!({"contents":[{"uri":"file:///b.png","mimeType":"image/png","path":"out/1.png","bytes":8}]})
        );

        let mut untouched = json!({"content":[{"type":"image","data":PNG,"mimeType":"image/png"}]});
        let before = untouched.clone();
        save_media(&mut untouched, &mut placeholders).unwrap();
        assert_eq!(
            untouched, before,
            "a sink that files nothing changes nothing"
        );
    }

    #[test]
    fn describe_sizes_like_a_person_would() {
        let media = |n: usize| Media {
            kind: "image".into(),
            mime_type: "image/png".into(),
            bytes: vec![0; n],
        };
        assert_eq!(describe(&media(4096), None), "[image image/png, 4 KB]");
        assert_eq!(describe(&media(1023), None), "[image image/png, 1023 B]");
        assert_eq!(describe(&media(1536), None), "[image image/png, 2 KB]");
        assert_eq!(
            describe(&media(3 * 1024 * 1024 / 2), None),
            "[image image/png, 1.5 MB]"
        );
        assert_eq!(
            describe(&media(4096), Some(Path::new("shots/take_screenshot-1.png"))),
            "[image saved to shots/take_screenshot-1.png, 4 KB]"
        );
    }

    #[test]
    fn extensions_follow_the_mime_type() {
        assert_eq!(extension_for("image/png"), "png");
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/svg+xml"), "svg");
        assert_eq!(extension_for("audio/mpeg"), "mp3");
        assert_eq!(extension_for("audio/x-wav"), "wav");
        assert_eq!(extension_for("application/pdf"), "pdf");
        assert_eq!(extension_for("text/plain; charset=utf-8"), "txt");
        assert_eq!(extension_for("text/html"), "html");
        assert_eq!(extension_for("application/octet-stream"), "bin");
        assert_eq!(extension_for("application/x-tar"), "bin");
        assert_eq!(extension_for(""), "bin");
        assert_eq!(extension_for("nonsense"), "bin");
    }

    #[test]
    fn resource_bodies_pass_text_through_and_decode_blobs() {
        let read = json!({"contents":[
            {"uri":"file:///a.txt","text":"hello\n"},
            {"uri":"file:///b.png","blob":"iVBORw0KGgo="},
            {"uri":"file:///c.empty"},
        ]});
        let bodies = resource_bodies(&read).unwrap();
        assert_eq!(bodies.len(), 2, "an entry with no payload is dropped");
        assert!(matches!(&bodies[0], ResourceBody::Text(t) if t == "hello\n"));
        assert!(matches!(&bodies[1], ResourceBody::Bytes(b) if b == b"\x89PNG\r\n\x1a\n"));

        let mangled = json!({"contents":[{"uri":"file:///b.png","blob":"not base64!"}]});
        let e = resource_bodies(&mangled).unwrap_err().to_string();
        assert!(e.contains("file:///b.png") && e.contains("base64"), "{e}");
    }

    /// A transport that talks on the way to its answer, the way a server
    /// running something slow does.
    struct Talkative {
        sent: Vec<Value>,
        says: Vec<Value>,
    }

    impl Transport for Talkative {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            self.send_watching(payload, &mut |_| {})
        }

        fn send_watching(
            &mut self,
            payload: &Value,
            watch: &mut dyn FnMut(&Value),
        ) -> Result<Option<Value>> {
            self.sent.push(payload.clone());
            for message in &self.says {
                watch(message);
            }
            Ok(Some(
                json!({"jsonrpc":"2.0","id":payload["id"],"result":{"ok":true}}),
            ))
        }
    }

    /// A watcher that keeps what it was told, and says it is listening.
    #[derive(Default)]
    struct Kept {
        listening: bool,
        heard: Vec<String>,
        interruptions: usize,
    }

    impl Watcher for Kept {
        fn wants_progress(&self) -> bool {
            self.listening
        }
        fn notice(&mut self, notice: &Notice<'_>) {
            self.heard.push(notice.summary());
        }
        fn interrupted(&mut self) {
            self.interruptions += 1;
        }
    }

    fn talkative(says: Vec<Value>) -> Session<Talkative> {
        Session::new(Talkative {
            sent: Vec::new(),
            says,
        })
        .offering(KnownVersion::LATEST_LEGACY)
    }

    #[test]
    fn a_progress_token_rides_only_when_the_watcher_is_listening() {
        let mut s = talkative(Vec::new());
        s.call_tool("echo", json!({})).unwrap();
        assert!(
            s.transport.sent[0]["params"]["_meta"].is_null(),
            "nobody was listening: {}",
            s.transport.sent[0]
        );

        let mut listening = Kept {
            listening: true,
            ..Kept::default()
        };
        s.call_tool_watching("echo", json!({}), &mut listening)
            .unwrap();
        let asked = &s.transport.sent[1];
        assert_eq!(asked["params"]["_meta"]["progressToken"], asked["id"]);
    }

    #[test]
    fn what_the_server_says_on_the_way_reaches_the_watcher_and_the_rest_is_dropped() {
        let says = vec![
            json!({"jsonrpc":"2.0","method":"notifications/progress",
                   "params":{"progressToken":1,"progress":1,"total":2,"message":"half"}}),
            json!({"jsonrpc":"2.0","method":"notifications/progress",
                   "params":{"progressToken":"someone else","progress":9}}),
            json!({"jsonrpc":"2.0","method":"notifications/message",
                   "params":{"level":"warning","data":"careful"}}),
            json!({"jsonrpc":"2.0","id":"srv","method":"ping"}),
        ];
        let mut s = talkative(says);
        let mut kept = Kept {
            listening: true,
            ..Kept::default()
        };
        let result = s.call_tool_watching("echo", json!({}), &mut kept).unwrap();
        assert_eq!(result["ok"], true, "the answer is untouched");
        assert_eq!(kept.heard, ["1/2 half", "server [warning] careful"]);
        assert_eq!(kept.interruptions, 0, "a ping is answered without a word");
    }

    /// A server that asks us something mid-call warns the watcher before the
    /// answer is worked out: whatever serves the request may put a question on
    /// the same stderr the updating line is on, and it has no other way to know.
    #[test]
    fn a_request_from_the_server_interrupts_the_watcher_and_a_ping_does_not() {
        let says = vec![
            json!({"jsonrpc":"2.0","method":"notifications/progress",
                   "params":{"progressToken":1,"progress":1,"total":2,"message":"half"}}),
            json!({"jsonrpc":"2.0","id":"srv-ping","method":"ping"}),
            json!({"jsonrpc":"2.0","id":"srv-elicit","method":"elicitation/create",
                   "params":{"message":"confirm before running"}}),
        ];
        let mut s = talkative(says);
        let mut kept = Kept {
            listening: true,
            ..Kept::default()
        };
        s.call_tool_watching("echo", json!({}), &mut kept).unwrap();
        assert_eq!(kept.interruptions, 1, "the elicitation, and not the ping");
        assert_eq!(kept.heard, ["1/2 half"], "neither request is a notice");
    }

    /// A 2026-07-28 server that will not answer until it has been told one more
    /// thing: it demands input on its first `rounds` requests and then answers
    /// with what it was told, so a test reads the retry off the result.
    struct Demanding {
        sent: Vec<Value>,
        rounds: usize,
        /// Whether the demand names a request to serve, or is the bare state a
        /// server shedding load sends.
        asks: bool,
    }

    impl Transport for Demanding {
        fn send(&mut self, payload: &Value) -> Result<Option<Value>> {
            self.sent.push(payload.clone());
            let id = payload["id"].clone();
            if self.sent.len() <= self.rounds {
                let mut demand = json!({
                    "resultType": "input_required",
                    "requestState": format!("state-{}", self.sent.len()),
                });
                if self.asks {
                    demand["inputRequests"] = json!({"confirm": {
                        "method": "elicitation/create",
                        "params": {"message": "confirm before running"}}});
                }
                return Ok(Some(json!({"jsonrpc":"2.0","id":id,"result":demand})));
            }
            Ok(Some(json!({"jsonrpc":"2.0","id":id,"result":{
                "resultType": "complete",
                "told": payload["params"]["inputResponses"].clone(),
                "state": payload["params"]["requestState"].clone(),
                "declared": payload["params"]["_meta"][META_CLIENT_CAPABILITIES].clone(),
            }})))
        }
    }

    fn accepting() -> Responder {
        Box::new(|method, params| {
            (method == "elicitation/create").then(
                || json!({"action": "accept", "content": {"asked": params["message"].clone()}}),
            )
        })
    }

    fn demanding(rounds: usize, asks: bool) -> Session<Demanding> {
        Session::new(Demanding {
            sent: Vec::new(),
            rounds,
            asks,
        })
        .offering(KnownVersion::V2026_07_28)
        .answering(json!({"elicitation": {"form": {}, "url": {}}}), accepting())
    }

    /// The whole of the Multi Round-Trip Request pattern: the demand is
    /// answered, the same call goes again carrying the answers under the keys
    /// the server named them by and its state back untouched, and the id is a
    /// new one because the two are independent requests.
    #[test]
    fn a_demand_for_input_is_answered_and_the_call_goes_again_carrying_it() {
        let mut s = demanding(1, true);
        let mut kept = Kept::default();
        let result = s
            .call_tool_watching("echo", json!({"message": "hi"}), &mut kept)
            .unwrap();

        assert_eq!(s.transport.sent.len(), 2);
        let (first, again) = (&s.transport.sent[0], &s.transport.sent[1]);
        assert_ne!(first["id"], again["id"], "two independent requests");
        assert_eq!(again["params"]["arguments"]["message"], "hi", "the call");
        assert_eq!(
            again["params"]["inputResponses"]["confirm"],
            json!({"action": "accept", "content": {"asked": "confirm before running"}})
        );
        assert_eq!(again["params"]["requestState"], "state-1");
        assert_eq!(result["told"]["confirm"]["action"], "accept");
        assert_eq!(
            result["declared"],
            json!({"elicitation": {"form": {}, "url": {}}}),
            "what the request declared is what invited the demand"
        );
        assert_eq!(kept.interruptions, 1, "stderr is about to be written to");
    }

    /// A server shedding load asks for nothing and only wants the request again.
    #[test]
    fn a_demand_that_asks_for_nothing_is_the_request_again_with_the_state() {
        let mut s = demanding(1, false);
        let mut kept = Kept::default();
        let result = s.call_tool_watching("echo", json!({}), &mut kept).unwrap();
        assert_eq!(s.transport.sent.len(), 2);
        assert_eq!(s.transport.sent[1]["params"]["requestState"], "state-1");
        assert!(s.transport.sent[1]["params"]["inputResponses"].is_null());
        assert!(result["told"].is_null());
        assert_eq!(kept.interruptions, 0, "nobody was asked anything");
    }

    /// A server may demand input as often as it likes, so something has to stop:
    /// a client that answered for ever would hang the caller just as surely as
    /// one that blocked.
    #[test]
    fn a_server_that_never_stops_asking_is_given_up_on() {
        let mut s = demanding(usize::MAX, true);
        let e = s.call_tool("echo", json!({})).unwrap_err().to_string();
        assert!(
            e.contains("tools/call") && e.contains("still asking"),
            "{e}"
        );
        assert_eq!(s.transport.sent.len(), MAX_INPUT_ROUNDS + 1);
    }

    /// The rule #118 fixed, carried into this era: a capability is declared only
    /// where something can honour it, and a demand that arrives anyway is
    /// refused at once rather than answered with a retry the server will only
    /// ask again about.
    #[test]
    fn nothing_is_declared_and_nothing_answered_where_nothing_can_answer() {
        let mut s = Session::new(Demanding {
            sent: Vec::new(),
            rounds: 1,
            asks: true,
        })
        .offering(KnownVersion::V2026_07_28);

        let e = s.call_tool("echo", json!({})).unwrap_err().to_string();
        assert!(e.contains("declared it could not supply"), "{e}");
        assert_eq!(s.transport.sent.len(), 1, "no retry it cannot answer");
        assert_eq!(
            s.transport.sent[0]["params"]["_meta"][META_CLIENT_CAPABILITIES],
            json!({}),
        );
    }

    /// Something the client was never promised for is named rather than sent
    /// back half-answered.
    #[test]
    fn a_kind_of_request_mcpdial_does_not_serve_is_named() {
        let mut s = Session::new(Demanding {
            sent: Vec::new(),
            rounds: 1,
            asks: true,
        })
        .offering(KnownVersion::V2026_07_28)
        .answering(json!({"roots": {}}), Box::new(|_, _| None));

        let e = s.call_tool("echo", json!({})).unwrap_err().to_string();
        assert!(e.contains("\"elicitation/create\""), "{e}");
    }

    /// Only 2026-07-28 has a demand for input to send. An older server's result
    /// is its answer, whatever `resultType` it happens to carry.
    #[test]
    fn an_older_servers_result_is_its_answer_whatever_it_calls_itself() {
        let mut s = Session::new(Demanding {
            sent: Vec::new(),
            rounds: 1,
            asks: true,
        })
        .offering(KnownVersion::LATEST_LEGACY)
        .answering(json!({"elicitation": {"form": {}}}), accepting());

        let result = s.call_tool("echo", json!({})).unwrap();
        assert_eq!(result["resultType"], "input_required");
        assert_eq!(s.transport.sent.len(), 1);
    }

    /// `raw` sends one request and shows what came back, demand and all.
    #[test]
    fn raw_sends_one_request_and_hands_back_what_it_answered() {
        let mut s = demanding(1, true);
        let result = s
            .request_once("tools/call", Some(json!({"name": "echo"})))
            .unwrap();
        assert_eq!(result["resultType"], "input_required");
        assert_eq!(s.transport.sent.len(), 1);
    }
    /// A transport that never answers and remembers every bound it was put
    /// under, for the one request that carries one of its own.
    #[derive(Default)]
    struct Timed {
        under: Vec<Option<Duration>>,
        wait: Option<Duration>,
    }

    impl Transport for Timed {
        fn send(&mut self, _payload: &Value) -> Result<Option<Value>> {
            Err(Error::transport("no reply in time"))
        }
        fn wait_at_most(&mut self, within: Option<Duration>) -> Option<Duration> {
            self.under.push(within);
            std::mem::replace(&mut self.wait, within)
        }
    }

    #[test]
    fn a_completion_asks_in_the_shape_the_spec_names_and_reads_the_answer() {
        let mut s = legacy(vec![Some(json!({"jsonrpc":"2.0","id":1,"result":{
            "completion":{"values":["terse","thorough"],"total":7,"hasMore":true}}}))]);
        let found = s
            .complete(
                json!({"type":"ref/prompt","name":"summarize"}),
                json!({"name":"style","value":"t"}),
                Some(json!({"arguments":{"document":"a.md"}})),
            )
            .unwrap();
        assert_eq!(
            found,
            Completion {
                values: vec!["terse".to_string(), "thorough".to_string()],
                total: Some(7),
                has_more: true,
            }
        );
        assert_eq!(s.transport.sent[0]["method"], "completion/complete");
        assert_eq!(
            s.transport.sent[0]["params"],
            json!({
                "ref": {"type":"ref/prompt","name":"summarize"},
                "argument": {"name":"style","value":"t"},
                "context": {"arguments":{"document":"a.md"}},
            })
        );
    }

    #[test]
    fn a_completion_with_nothing_settled_carries_no_context_at_all() {
        let mut s = legacy(vec![Some(json!({"jsonrpc":"2.0","id":1,"result":{}}))]);
        let found = s
            .complete(
                json!({"type":"ref/resource","uri":"file:///notes/{name}.md"}),
                json!({"name":"name","value":""}),
                None,
            )
            .unwrap();
        // A result with no completion in it is no values, not an error.
        assert_eq!(found, Completion::default());
        assert_eq!(s.transport.sent[0]["params"].get("context"), None);
    }

    #[test]
    fn a_server_that_sends_more_than_the_spec_allows_has_the_rest_dropped() {
        let flood: Vec<String> = (0..MOST_COMPLETIONS + 1).map(|n| n.to_string()).collect();
        let mut s = legacy(vec![Some(json!({"jsonrpc":"2.0","id":1,"result":{
            "completion":{"values":flood,"hasMore":false}}}))]);
        let found = s.complete(json!({}), json!({}), None).unwrap();
        assert_eq!(found.values.len(), MOST_COMPLETIONS);
        assert!(found.has_more, "what was dropped is still more");
    }

    #[test]
    fn a_bounded_completion_puts_the_transport_back_however_it_ends() {
        let bound = Duration::from_secs(2);
        let mut s = Session::new(Timed::default()).offering(KnownVersion::LATEST_LEGACY);
        let refused = s.complete_within(bound, json!({}), json!({}), None);
        assert!(refused.is_err(), "the transport answered nothing");
        assert_eq!(
            s.transport.under,
            [Some(bound), None],
            "the bound goes on for the request and comes off after it"
        );
    }
}
