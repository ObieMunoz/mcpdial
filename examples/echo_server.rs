//! The smallest MCP server that is still a real one: newline-delimited JSON-RPC on
//! stdio, three methods, three tools. Used by the integration tests and handy as a
//! reference for what a stdio server has to do.
//!
//!     cargo run --example echo_server
//!     mcpdial call 'stdio:target/debug/examples/echo_server' echo '{"message":"hi"}'
//!
//! Set `ECHO_SERVER_HANG=1` to make it swallow every request, for timeout tests.
//! Set `ECHO_SERVER_TAG` to have it echoed back as the server's `instructions`.
//! The `count` tool returns how many times it has been called in this process,
//! which is how the tests tell one long session from several short ones.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn main() {
    let hang = std::env::var_os("ECHO_SERVER_HANG").is_some();
    let tag = std::env::var("ECHO_SERVER_TAG").ok();
    let mut count = 0u32;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Real servers do this too: a non-protocol line on stdout that a client must skip.
    writeln!(out, "echo_server ready").unwrap();
    out.flush().unwrap();
    eprintln!("echo_server: this is stderr noise");

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
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
                ]}),
            ),
            "tools/call" => match params["name"].as_str().unwrap_or("") {
                "echo" => ok(
                    id,
                    json!({"content": [{"type": "text",
                        "text": format!("Echo: {}", params["arguments"]["message"].as_str().unwrap_or(""))}]}),
                ),
                "fail" => ok(
                    id,
                    json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
                ),
                "count" => {
                    count += 1;
                    ok(
                        id,
                        json!({"content": [{"type": "text", "text": format!("count={count}")}]}),
                    )
                }
                other => err(id, -32602, &format!("Tool {other} not found")),
            },
            other => err(id, -32601, &format!("Method not found: {other}")),
        };
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
