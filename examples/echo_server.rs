//! The smallest MCP server that is still a real one: newline-delimited JSON-RPC on
//! stdio, five methods, five tools. Used by the integration tests and handy as a
//! reference for what a stdio server has to do.
//!
//!     cargo run --example echo_server
//!     mcpdial call 'stdio:target/debug/examples/echo_server' echo '{"message":"hi"}'
//!
//! Set `ECHO_SERVER_HANG=1` to make it swallow every request, for timeout tests.
//! Set `ECHO_SERVER_TAG` to have it echoed back as the server's `instructions`.
//! Set `ECHO_SERVER_PING=1` to make it talk first during `tools/call`: a
//! notification, a `ping`, and a request it knows the client cannot serve. The
//! call does not finish until both requests are answered, which is what a real
//! server checking a slow connection does to a client that only listens for its
//! own id.
//! Set `ECHO_SERVER_PROGRESS=N` to make it report N times during `tools/call`
//! before replying: a `notifications/progress` under the request's own
//! `_meta.progressToken`, or under a token nobody asked for when the request
//! carried none, and a log message at each end of the severity scale. That is
//! what a crawler or a build does, and it is behind a flag because a server
//! that talks during every call would change what every other test reads.
//! Set `ECHO_SERVER_PROGRESS_SHRINK=1` to make those reports carry a message
//! that gets shorter each time instead of longer, and none at all once it has
//! run out, the way a server moving off a status message onto a bare count
//! does. Behind its own flag so that the `step N` every other test reads stays
//! where it is.
//! Set `ECHO_SERVER_EXIT_ON_CALL=N` to make it exit with status 9 on its Nth
//! `tools/call`, before replying, the way a server that crashes mid-call does.
//! The `count` tool returns how many times it has been called in this process,
//! which is how the tests tell one long session from several short ones. The
//! `shot` tool answers with a 4 KB image block, the way a screenshot tool does.
//! Set `ECHO_SERVER_PROMPTS=1` to add a `poster` prompt that does the same; without
//! it the server implements no prompts, which other tests count on.
//! Set `ECHO_SERVER_UNKNOWN_TOOL_RESULT=1` to answer a tool name it does not have
//! with a failed *result*, `Unknown tool: NAME` under `isError`, the way DeepWiki
//! does, instead of the `-32602` error the rest of the world sends.
//! Set `ECHO_SERVER_TYPES=1` to add a `typed` tool declaring one property of every
//! JSON type and refusing any other, which answers with the arguments it was handed
//! so a test can see what type each one arrived as. It is behind a flag for the same
//! reason: a sixth tool would renumber every listing assertion.
//! Set `ECHO_SERVER_ANNOTATED=1` to add an `erase` tool that says what calling it
//! does: a `title`, `annotations` marking it destructive and open-world, and
//! `execution.taskSupport`. Behind a flag for the same reason `typed` is.
//! Set `ECHO_SERVER_TASKS=none` to speak 2025-11-25 and nothing else: the
//! revision has the tasks utility, this server does not offer it, and every
//! `tasks/*` is refused with `-32601`.
//! Set `ECHO_SERVER_TASKS=1` to speak 2025-11-25, declare the tasks utility, and
//! add a `slow` tool whose `execution.taskSupport` is `required`. A `tools/call`
//! carrying `params.task` is answered with a task object rather than a result;
//! `tasks/get` reports `working` twice and `completed` after that, and
//! `tasks/result` hands back what the tool would have returned. `slow` called
//! without a task is refused with `-32601`, as the spec has it.
//! Set `ECHO_SERVER_ELICIT=form` (or `url`) to make every `tools/call` ask the
//! client for one more fact first, the way a server missing a confirmation or a
//! region does, and answer with whatever the client replied. The call blocks on
//! that reply, so a client that neither answers nor refuses hangs the server.
//! Set `ECHO_SERVER_SUBSCRIBE=N` to add one resource, `counter://calls`, that a
//! client can subscribe to, and a `register_tool` tool. Each `count` call then
//! sends N `notifications/resources/updated` for that resource before replying,
//! and `register_tool` sends `notifications/tools/list_changed` and adds a tool
//! to what `tools/list` answers from then on. N above one is a server that
//! reports the same change over and over, which a client has to survive.
//! Behind a flag because the resource, the tool and the capability would each
//! renumber a listing every other test counts.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Lines, StdinLock, Write};

/// One task in flight: what the tool will say when it is done, and how many
/// times it has been asked about.
struct Task {
    text: String,
    asked: u32,
    cancelled: bool,
    /// Milliseconds this server agreed to hold the task for, which is what the
    /// call asked for unless that was longer than it is willing to keep one.
    ttl: u64,
}

/// How many polls a task stays `working` for before it finishes.
const POLLS_BEFORE_DONE: u32 = 2;

/// The longest this server holds a task, however long it was asked for.
const LONGEST_TTL: u64 = 60_000;

impl Task {
    fn status(&self) -> &'static str {
        match (self.cancelled, self.asked >= POLLS_BEFORE_DONE) {
            (true, _) => "cancelled",
            (_, true) => "completed",
            _ => "working",
        }
    }

    /// The task object every one of the four methods answers with. `pollInterval`
    /// is short because a test waits it out for real.
    fn described(&self, id: &str) -> Value {
        json!({"taskId": id, "status": self.status(),
               "statusMessage": format!("asked {} times", self.asked),
               "createdAt": "2026-01-01T00:00:00Z", "lastUpdatedAt": "2026-01-01T00:00:00Z",
               "ttl": self.ttl, "pollInterval": 10})
    }
}

/// A tool that will not run in the foreground at all: the shape a client has to
/// notice before it sends a call that would only be refused.
fn slow_tool() -> Value {
    json!({"name": "slow", "description": "Echo a message, eventually.",
    "execution": {"taskSupport": "required"},
    "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}})
}

/// One property of every type a schema can name, and nothing else allowed, so a
/// client coercing `key=value` text has something to coerce it against.
fn typed_tool() -> Value {
    json!({"name": "typed", "description": "Echo the arguments back as JSON.",
    "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
        "text": {"type": "string"},
        "count": {"type": "integer"},
        "ratio": {"type": "number"},
        "flag": {"type": "boolean"},
        "tags": {"type": "array"},
        "meta": {"type": "object"},
        "id": {"type": ["string", "number"]},
    }}})
}

/// A tool whose own metadata warns a caller off before they call it: what
/// `title`, `annotations` and `execution.taskSupport` look like on the wire.
fn annotated_tool() -> Value {
    json!({"name": "erase", "title": "Erase a file",
    "description": "Remove a file permanently.",
    "annotations": {"destructiveHint": true, "openWorldHint": true},
    "execution": {"taskSupport": "optional"},
    "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}},
                    "required": ["path"]}})
}

/// A PNG signature padded to 4096 bytes: enough to be a nuisance on stdout.
fn image_block() -> Value {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(4096, 0);
    json!({"type": "image", "data": STANDARD.encode(png), "mimeType": "image/png"})
}

fn main() {
    let hang = std::env::var_os("ECHO_SERVER_HANG").is_some();
    let ping = std::env::var_os("ECHO_SERVER_PING").is_some();
    let prompts = std::env::var_os("ECHO_SERVER_PROMPTS").is_some();
    let unknown_tool_is_a_result = std::env::var_os("ECHO_SERVER_UNKNOWN_TOOL_RESULT").is_some();
    let types = std::env::var_os("ECHO_SERVER_TYPES").is_some();
    let annotated = std::env::var_os("ECHO_SERVER_ANNOTATED").is_some();
    let elicit = std::env::var("ECHO_SERVER_ELICIT").ok();
    let asked_for_tasks = std::env::var("ECHO_SERVER_TASKS").ok();
    // The revision that has tasks at all, which is not the same as offering them.
    let modern = asked_for_tasks.is_some();
    let tasks = asked_for_tasks.as_deref().is_some_and(|how| how != "none");
    let mut running: BTreeMap<String, Task> = BTreeMap::new();
    let mut started = 0u32;
    let tag = std::env::var("ECHO_SERVER_TAG").ok();
    let exit_on_call: Option<u32> = std::env::var("ECHO_SERVER_EXIT_ON_CALL")
        .ok()
        .and_then(|n| n.parse().ok());
    let reports: u32 = std::env::var("ECHO_SERVER_PROGRESS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let shrinking_reports = std::env::var_os("ECHO_SERVER_PROGRESS_SHRINK").is_some();
    let updates_per_count: u32 = std::env::var("ECHO_SERVER_SUBSCRIBE")
        .ok()
        .map(|n| n.parse().unwrap_or(1))
        .unwrap_or(0);
    let subscribable = updates_per_count > 0;
    let mut subscribed: Vec<String> = Vec::new();
    let mut registered = false;
    let mut count = 0u32;
    let mut calls = 0u32;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    // Real servers do this too: a non-protocol line on stdout that a client must skip.
    writeln!(out, "echo_server ready").unwrap();
    out.flush().unwrap();
    eprintln!("echo_server: this is stderr noise");

    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification
        };
        if hang {
            continue;
        }

        let method = msg["method"].as_str().unwrap_or("");
        let params = &msg["params"];

        if method == "tools/call" {
            calls += 1;
            if exit_on_call == Some(calls) {
                eprintln!("echo_server: crashing on call {calls}");
                std::process::exit(9);
            }
        }

        if reports > 0 && method == "tools/call" {
            report(
                &mut out,
                reports,
                &params["_meta"]["progressToken"],
                shrinking_reports,
            );
        }

        // Interrupt the call the client is waiting on. A wrong answer is reported
        // through the call itself, so a test that only reads the tool's output
        // still catches it; no answer at all leaves us blocked here until the
        // client gives up, which is the bug this mode exists to catch.
        if ping && method == "tools/call" {
            tell(
                &mut out,
                &json!({"jsonrpc": "2.0", "method": "notifications/message",
                        "params": {"level": "info", "data": "working"}}),
            );
            let pong = ask(&mut out, &mut lines, "srv-ping", "ping", Value::Null);
            let refusal = ask(&mut out, &mut lines, "srv-roots", "roots/list", Value::Null);
            if pong.get("result").is_none() {
                let complaint = format!("ping answered {pong}");
                tell(&mut out, &err(id.clone(), -32001, &complaint));
                continue;
            }
            if refusal["error"]["code"] != -32601 {
                let complaint = format!("roots/list answered {refusal}");
                tell(&mut out, &err(id.clone(), -32001, &complaint));
                continue;
            }
        }

        // One fact is missing, so ask for it and block until the client answers.
        // The answer is echoed in the result: a test reads what the client
        // decided from the tool's own output, with no trace parsing.
        if let Some(mode) = elicit.as_deref() {
            if method == "tools/call" {
                let url_mode = mode == "url";
                let answered = ask(
                    &mut out,
                    &mut lines,
                    "srv-elicit",
                    "elicitation/create",
                    elicitation(url_mode),
                );
                if url_mode {
                    tell(
                        &mut out,
                        &json!({"jsonrpc": "2.0",
                        "method": "notifications/elicitation/complete",
                        "params": {"elicitationId": "e-1"}}),
                    );
                }
                let text = match answered.get("result") {
                    Some(result) => format!("elicited {result}"),
                    None => format!("refused {}", answered["error"]),
                };
                tell(
                    &mut out,
                    &ok(id, json!({"content": [{"type": "text", "text": text}]})),
                );
                continue;
            }
        }

        let reply = match method {
            "initialize" => {
                let mut result = json!({
                    "protocolVersion": if modern { "2025-11-25" } else { "2025-06-18" },
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-server", "version": "0.0.1"},
                });
                if tasks {
                    result["capabilities"]["tasks"] = json!({
                        "requests": {"tools": {"call": true}}, "list": true, "cancel": true});
                }
                if let Some(t) = &tag {
                    result["instructions"] = json!(format!("tag={t}"));
                }
                if prompts {
                    result["capabilities"]["prompts"] = json!({});
                }
                if subscribable {
                    result["capabilities"]["resources"] = json!({"subscribe": true});
                }
                ok(id, result)
            }
            "resources/list" if subscribable => ok(
                id,
                json!({"resources": [{"uri": "counter://calls", "name": "counter",
                                      "description": "How many times count has been called.",
                                      "mimeType": "text/plain"}]}),
            ),
            "resources/read" if subscribable => match params["uri"].as_str().unwrap_or("") {
                "counter://calls" => ok(
                    id,
                    json!({"contents": [{"uri": "counter://calls", "mimeType": "text/plain",
                                         "text": format!("count={count}")}]}),
                ),
                other => err(id, -32002, &format!("Resource not found: {other}")),
            },
            // The spec asks for an empty result; what matters to a client is
            // that the request was accepted at all. A URI it does not have is
            // accepted too, so that a test can follow something that is not
            // there and see what happens when the update for it arrives.
            "resources/subscribe" if subscribable => {
                let uri = params["uri"].as_str().unwrap_or("").to_string();
                if !subscribed.contains(&uri) {
                    subscribed.push(uri);
                }
                ok(id, json!({}))
            }
            "resources/unsubscribe" if subscribable => {
                subscribed.retain(|u| u != params["uri"].as_str().unwrap_or(""));
                ok(id, json!({}))
            }
            "tools/list" => {
                let mut listed = json!({"tools": [
                    {"name": "echo", "description": "Echo a message back.",
                     "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}},
                    {"name": "fail", "description": "Always returns a tool error.",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "count", "description": "How many times this process has been asked.",
                     "inputSchema": {"type": "object", "properties": {}}},
                    {"name": "strict", "description": "Rejects bad arguments in a failed result.",
                     "inputSchema": {"type": "object", "properties": {"pageId": {"type": "number"}}, "required": ["pageId"]}},
                    {"name": "shot", "description": "A text block, then a 4 KB image block.",
                     "inputSchema": {"type": "object", "properties": {}}},
                ]});
                if types {
                    listed["tools"].as_array_mut().unwrap().push(typed_tool());
                }
                if annotated {
                    listed["tools"]
                        .as_array_mut()
                        .unwrap()
                        .push(annotated_tool());
                }
                if tasks {
                    listed["tools"].as_array_mut().unwrap().push(slow_tool());
                }
                if subscribable {
                    listed["tools"].as_array_mut().unwrap().push(json!(
                        {"name": "register_tool", "description": "Add a tool to this server.",
                         "inputSchema": {"type": "object", "properties": {}}}));
                }
                if registered {
                    listed["tools"].as_array_mut().unwrap().push(json!(
                        {"name": "registered", "description": "The tool register_tool added.",
                         "inputSchema": {"type": "object", "properties": {}}}));
                }
                ok(id, listed)
            }
            "prompts/list" if prompts => ok(
                id,
                json!({"prompts": [
                    {"name": "poster", "description": "A caption and a 4 KB image."},
                ]}),
            ),
            "prompts/get" if prompts => match params["name"].as_str().unwrap_or("") {
                "poster" => ok(
                    id,
                    json!({"description": "A caption and a 4 KB image.", "messages": [
                        {"role": "user", "content": {"type": "text", "text": "Caption this."}},
                        {"role": "user", "content": image_block()},
                    ]}),
                ),
                other => err(id, -32602, &format!("Prompt {other} not found")),
            },
            // A call carrying a task is answered with the task, and the tool's
            // own answer is kept until `tasks/result` comes for it.
            "tools/call" if tasks && !params["task"].is_null() => {
                started += 1;
                let name = format!("t-{started}");
                let said = params["arguments"]["message"].as_str().unwrap_or("");
                running.insert(
                    name.clone(),
                    Task {
                        text: format!("Echo: {said}"),
                        asked: 0,
                        cancelled: false,
                        ttl: params["task"]["ttl"]
                            .as_u64()
                            .unwrap_or(LONGEST_TTL)
                            .min(LONGEST_TTL),
                    },
                );
                ok(id, running[&name].described(&name))
            }
            "tasks/get" if tasks => match running.get_mut(task_id(params)) {
                Some(task) => {
                    task.asked += 1;
                    ok(id, task.described(task_id(params)))
                }
                None => err(id, -32602, &format!("Task {} not found", task_id(params))),
            },
            "tasks/list" if tasks => ok(
                id,
                json!({"tasks": running.iter().map(|(name, task)| task.described(name))
                       .collect::<Vec<_>>()}),
            ),
            "tasks/cancel" if tasks => match running.get_mut(task_id(params)) {
                Some(task) => {
                    task.cancelled = true;
                    ok(id, task.described(task_id(params)))
                }
                None => err(id, -32602, &format!("Task {} not found", task_id(params))),
            },
            // The result outlives the task the way a real server's does: the
            // entry stays, so asking twice answers twice.
            "tasks/result" if tasks => match running.get(task_id(params)) {
                Some(task) => ok(
                    id,
                    json!({"content": [{"type": "text", "text": task.text}]}),
                ),
                None => err(id, -32602, &format!("Task {} not found", task_id(params))),
            },
            "tools/call" => match params["name"].as_str().unwrap_or("") {
                // Real servers validate against the schema before running anything.
                "echo" if params["arguments"]["message"].as_str().is_none() => err(
                    id,
                    -32602,
                    "Invalid arguments for tool echo: Required at message",
                ),
                "echo" => ok(
                    id,
                    json!({"content": [{"type": "text",
                        "text": format!("Echo: {}", params["arguments"]["message"].as_str().unwrap_or(""))}]}),
                ),
                // The other half of the world reports a schema violation as a failed
                // result carrying the -32602 text, rather than as a JSON-RPC error.
                "strict" if params["arguments"]["pageId"].as_f64().is_none() => ok(
                    id,
                    json!({"isError": true, "content": [{"type": "text", "text":
                        "MCP error -32602: Invalid arguments for tool strict: Required at pageId"}]}),
                ),
                "strict" => ok(
                    id,
                    json!({"content": [{"type": "text", "text": "strict ok"}]}),
                ),
                "fail" => ok(
                    id,
                    json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
                ),
                "shot" => ok(
                    id,
                    json!({"content": [{"type": "text", "text": "done"}, image_block()]}),
                ),
                // Handing the arguments straight back is the whole point: the test
                // reads the JSON types off the answer.
                "typed" if types => ok(
                    id,
                    json!({"content": [{"type": "text",
                        "text": params["arguments"].to_string()}]}),
                ),
                "count" => {
                    count += 1;
                    // Sent before the reply and on the same pipe, which is how a
                    // real server reports a change during a call the client is
                    // already waiting on.
                    for _ in 0..updates_per_count {
                        for uri in &subscribed {
                            tell(
                                &mut out,
                                &json!({"jsonrpc": "2.0",
                                        "method": "notifications/resources/updated",
                                        "params": {"uri": uri}}),
                            );
                        }
                    }
                    ok(
                        id,
                        json!({"content": [{"type": "text", "text": format!("count={count}")}]}),
                    )
                }
                // What the spec has a `required` tool answer a call with no task.
                "slow" if tasks => err(
                    id,
                    -32601,
                    "Tool slow must be called as a task: pass params.task",
                ),
                "register_tool" if subscribable => {
                    registered = true;
                    tell(
                        &mut out,
                        &json!({"jsonrpc": "2.0",
                                "method": "notifications/tools/list_changed"}),
                    );
                    ok(
                        id,
                        json!({"content": [{"type": "text", "text": "registered"}]}),
                    )
                }
                other if unknown_tool_is_a_result => ok(
                    id,
                    json!({"isError": true, "content": [{"type": "text",
                        "text": format!("Unknown tool: {other}")}]}),
                ),
                other => err(id, -32602, &format!("Tool {other} not found")),
            },
            other => err(id, -32601, &format!("Method not found: {other}")),
        };
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}

/// Talk while the client waits, the way a server doing something slow does:
/// `times` progress notifications, and a log message at each end of the scale
/// so a client's threshold has something to keep and something to drop.
///
/// One report always goes out under a token nobody asked for, and a request
/// that sent no `progressToken` gets all of them that way: real servers do
/// both, and a client has to drop them rather than report them.
///
/// `shrinking` swaps the growing `step N` for a message that gets shorter, for
/// a client that draws the reports over one line.
fn report(out: &mut impl Write, times: u32, token: &Value, shrinking: bool) {
    let token = match token.is_null() {
        true => json!("nobody-asked-for-this"),
        false => token.clone(),
    };
    tell(
        out,
        &json!({"jsonrpc": "2.0", "method": "notifications/message",
                "params": {"level": "debug", "logger": "echo", "data": "starting"}}),
    );
    tell(
        out,
        &json!({"jsonrpc": "2.0", "method": "notifications/progress",
                "params": {"progressToken": "stale", "progress": 9, "message": "not yours"}}),
    );
    for done in 1..=times {
        let mut params = json!({"progressToken": token, "progress": done, "total": times});
        let message = match shrinking {
            true => shrinking_message(done),
            false => Some(format!("step {done}")),
        };
        // Once it has run out the key is left out rather than sent as null: a
        // report carrying no message is what the spec allows, and the shortest
        // line a client redrawing them has to draw over a longer one.
        if let Some(message) = message {
            params["message"] = json!(message);
        }
        tell(
            out,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}),
        );
    }
    tell(
        out,
        &json!({"jsonrpc": "2.0", "method": "notifications/message",
                "params": {"level": "warning", "logger": "echo", "data": "nearly there"}}),
    );
}

/// What the `done`th report says under `ECHO_SERVER_PROGRESS_SHRINK`: a word
/// less of the same phrase every time, and nothing once it has run out.
fn shrinking_message(done: u32) -> Option<String> {
    const PHRASE: [&str; 4] = ["fetching", "the", "remote", "index"];
    let kept = PHRASE.len().saturating_sub(done.saturating_sub(1) as usize);
    (kept > 0).then(|| PHRASE[..kept].join(" "))
}

/// Write one message and make sure it is on its way.
fn tell(out: &mut impl Write, msg: &Value) {
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}

/// Send the client a request and block until the answer to it arrives, the way a
/// server waiting on a `ping` does. Anything else on the way is skipped, and a
/// client that hangs up gets `null` back rather than a panic.
fn ask(
    out: &mut impl Write,
    lines: &mut Lines<StdinLock<'_>>,
    id: &str,
    method: &str,
    params: Value,
) -> Value {
    let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if !params.is_null() {
        request["params"] = params;
    }
    tell(out, &request);
    while let Some(Ok(line)) = lines.next() {
        match serde_json::from_str::<Value>(&line) {
            Ok(reply) if reply["id"] == id => return reply,
            _ => continue,
        }
    }
    Value::Null
}

/// What the server asks for: a small form, or an address to finish at.
fn elicitation(url_mode: bool) -> Value {
    if url_mode {
        return json!({"mode": "url", "message": "finish this in your browser",
                      "url": "https://example.test/elicit/1", "elicitationId": "e-1"});
    }
    json!({"message": "confirm before running", "requestedSchema": {
        "type": "object",
        "properties": {
            "confirm": {"type": "boolean", "description": "Really run it?"},
            "count": {"type": "integer", "minimum": 1, "maximum": 3},
            "region": {"type": "string", "enum": ["us", "eu"]},
        },
        "required": ["confirm", "region"],
    }})
}

/// Which task a `tasks/*` request is about.
fn task_id(params: &Value) -> &str {
    params["taskId"].as_str().unwrap_or("")
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
