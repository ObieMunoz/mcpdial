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
//! Set `ECHO_SERVER_ELICIT=form` (or `url`) to make every `tools/call` ask the
//! client for one more fact first, the way a server missing a confirmation or a
//! region does, and answer with whatever the client replied. The call blocks on
//! that reply, so a client that neither answers nor refuses hangs the server.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::io::{self, BufRead, Lines, StdinLock, Write};

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
    let tag = std::env::var("ECHO_SERVER_TAG").ok();
    let exit_on_call: Option<u32> = std::env::var("ECHO_SERVER_EXIT_ON_CALL")
        .ok()
        .and_then(|n| n.parse().ok());
    let reports: u32 = std::env::var("ECHO_SERVER_PROGRESS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
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
            report(&mut out, reports, &params["_meta"]["progressToken"]);
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
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo-server", "version": "0.0.1"},
                });
                if let Some(t) = &tag {
                    result["instructions"] = json!(format!("tag={t}"));
                }
                if prompts {
                    result["capabilities"]["prompts"] = json!({});
                }
                ok(id, result)
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
                    ok(
                        id,
                        json!({"content": [{"type": "text", "text": format!("count={count}")}]}),
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
fn report(out: &mut impl Write, times: u32, token: &Value) {
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
        tell(
            out,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress",
                    "params": {"progressToken": token, "progress": done, "total": times,
                               "message": format!("step {done}")}}),
        );
    }
    tell(
        out,
        &json!({"jsonrpc": "2.0", "method": "notifications/message",
                "params": {"level": "warning", "logger": "echo", "data": "nearly there"}}),
    );
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

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
