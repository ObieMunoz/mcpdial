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

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::io::{self, BufRead, Lines, StdinLock, Write};

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
    let tag = std::env::var("ECHO_SERVER_TAG").ok();
    let exit_on_call: Option<u32> = std::env::var("ECHO_SERVER_EXIT_ON_CALL")
        .ok()
        .and_then(|n| n.parse().ok());
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
            let pong = ask(&mut out, &mut lines, "srv-ping", "ping");
            let refusal = ask(&mut out, &mut lines, "srv-roots", "roots/list");
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
            "tools/list" => ok(
                id,
                json!({"tools": [
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
                ]}),
            ),
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

/// Write one message and make sure it is on its way.
fn tell(out: &mut impl Write, msg: &Value) {
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}

/// Send the client a request and block until the answer to it arrives, the way a
/// server waiting on a `ping` does. Anything else on the way is skipped, and a
/// client that hangs up gets `null` back rather than a panic.
fn ask(out: &mut impl Write, lines: &mut Lines<StdinLock<'_>>, id: &str, method: &str) -> Value {
    tell(out, &json!({"jsonrpc": "2.0", "id": id, "method": method}));
    while let Some(Ok(line)) = lines.next() {
        match serde_json::from_str::<Value>(&line) {
            Ok(reply) if reply["id"] == id => return reply,
            _ => continue,
        }
    }
    Value::Null
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
