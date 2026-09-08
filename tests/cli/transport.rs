//! Both transports end to end: the handshake, paging, exit codes, and what a
//! dropped socket or a stdio process that will not answer does to a command.

use crate::common::{echo_command, mcpdial, run, start, temp_home, Mode};
use crate::shell::shell;
use serde_json::Value;

#[test]
fn stateful_http_handshake_session_and_call() {
    let s = start(Mode::Stateful);
    let home = temp_home("stateful");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(
        o.stdout.contains("capabilities: prompts, resources, tools"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"));
    assert!(o.stdout.contains("echo") && o.stdout.contains("Echo a message back."));
    assert!(
        !o.stdout.contains("Second line"),
        "short listing shows the first line only"
    );

    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":40,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 40 and 2 is 42.");

    // What actually went over the wire for that last call.
    let reqs = s.requests.lock().unwrap();
    let last4 = &reqs[reqs.len() - 4..];
    assert_eq!(last4[0].json()["method"], "initialize");
    assert!(
        last4[0]
            .header("user-agent")
            .unwrap()
            .starts_with("Mozilla/5.0"),
        "browser UA is mandatory"
    );
    assert_eq!(
        last4[0].header("accept").unwrap(),
        "application/json, text/event-stream"
    );
    assert!(last4[0].header("mcp-session-id").is_none());
    assert_eq!(last4[1].json()["method"], "notifications/initialized");
    assert!(last4[1].json().get("id").is_none());
    assert_eq!(
        last4[1].header("mcp-session-id"),
        Some("sess-1"),
        "session id is echoed back"
    );
    assert_eq!(last4[2].json()["method"], "tools/call");
    assert_eq!(last4[2].header("mcp-protocol-version"), Some("2025-06-18"));
    assert_eq!(
        last4[3].method, "DELETE",
        "one-shot commands end the session"
    );
    assert_eq!(last4[3].header("mcp-session-id"), Some("sess-1"));
}

#[test]
fn stateless_http_and_json_output() {
    let s = start(Mode::Stateless);
    let home = temp_home("stateless");
    let o = run(mcpdial(&home).args([
        "--json",
        "call",
        &s.url,
        "echo",
        r#"{"message":"plain json"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], "Echo: plain json");

    let o = run(mcpdial(&home).args(["--json", "tools", &s.url]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);

    let o = run(mcpdial(&home).args(["raw", &s.url, "tools/list"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("\"name\": \"echo\""));
    assert!(o.stdout.contains("\"nextCursor\""), "{}", o.stdout);
}

#[test]
fn tool_lists_are_merged_across_pages() {
    let s = start(Mode::Stateless);
    let home = temp_home("paged");
    assert_eq!(
        run(mcpdial(&home).args(["add", "paged", "--http", &s.url, "--no-probe"])).code,
        0
    );

    let o = run(mcpdial(&home).args(["tools", "paged"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("echo") && o.stdout.contains("add"),
        "both pages: {}",
        o.stdout
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "tools/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "page-2");

    let o = run(mcpdial(&home).args(["--json", "tools", "paged"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo", "add"]);

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["tools"], 2, "{}", o.stdout);

    let tool_from_the_second_page = "add";
    let o = run(mcpdial(&home).args(["schema", "paged", tool_from_the_second_page]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args([
        "call",
        "paged",
        tool_from_the_second_page,
        r#"{"a":1,"b":2}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
}

#[test]
fn a_repeating_cursor_stops_instead_of_looping() {
    let s = start(Mode::StuckCursor);
    let home = temp_home("stuck");

    let o = run(mcpdial(&home).args(["--timeout", "5", "tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);

    let pages = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.json()["method"] == "tools/list")
        .count();
    assert_eq!(pages, 2, "stopped the moment the cursor repeated");
}

#[test]
fn errors_map_to_exit_codes() {
    let s = start(Mode::Stateless);
    let home = temp_home("errors");

    let o = run(mcpdial(&home).args(["call", &s.url, "fail"]));
    assert_eq!(o.code, 1, "isError result exits 1");
    assert_eq!(o.stdout.trim(), "it failed");

    let o = run(mcpdial(&home).args(["call", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("MCP error -32602: Tool nope not found"),
        "{}",
        o.stderr
    );

    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "{not json"]));
    assert_eq!(o.code, 2, "bad JSON is a usage error, nothing sent");
    assert_eq!(s.requests.lock().unwrap().len(), before, "nothing was sent");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "[1,2]"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("must be a JSON object"));

    // An unquoted {"message":"hi"} reaches us as {message:hi}: name the cause
    // and answer with the command line that would have worked.
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "{message:hi}"]));
    assert_eq!(o.code, 2);
    assert!(
        o.stderr.contains(r#"did you mean {"message": "hi"}?"#),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains(&format!(
            "mcpdial call {} echo '{{\"message\": \"hi\"}}'",
            s.url
        )),
        "{}",
        o.stderr
    );

    // With two keys the shell splits the object at the comma into two words,
    // which are not key=value pairs either.
    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "message:hi", "n:2"]));
    assert_eq!(o.code, 2);
    assert_eq!(s.requests.lock().unwrap().len(), before, "nothing was sent");
    assert!(
        o.stderr.contains(r#""message:hi" is neither"#)
            && o.stderr.contains("split a JSON object at its commas"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["info", "no-such-server"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("unknown server"));

    let o = run(mcpdial(&home).args(["--timeout", "2", "info", "http://127.0.0.1:1/mcp"]));
    assert_eq!(o.code, 1);
    // Refused on unix; a Windows host drops it instead, which arrives as a timeout.
    let nothing_answered =
        o.stderr.contains("could not reach") || o.stderr.contains("no reply from");
    assert!(nothing_answered, "{}", o.stderr);
}

#[test]
fn blocked_403_is_not_blamed_on_the_token() {
    let s = start(Mode::Blocked);
    let home = temp_home("blocked");
    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("HTTP 403"));
    assert!(o.stderr.contains("policy/edge block"), "{}", o.stderr);
    assert!(!o.stderr.contains("login"), "must not suggest a credential");
}

#[test]
fn stdio_adhoc_call_and_timeout() {
    let home = temp_home("stdio");
    let target = format!("stdio:{}", echo_command());

    let o = run(mcpdial(&home).args(["call", &target, "echo", r#"{"message":"over a pipe"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: over a pipe");

    let o = run(mcpdial(&home).args(["info", &target]));
    assert!(o.stdout.contains("echo-server 0.0.1"));

    let o = run(mcpdial(&home).args(["call", &target, "fail"]));
    assert_eq!(o.code, 1);

    let o =
        run(mcpdial(&home)
            .env("ECHO_SERVER_HANG", "1")
            .args(["--timeout", "1", "info", &target]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("no reply after"), "{}", o.stderr);

    // A server that dies on startup explains itself: exit status plus its stderr.
    let dies_on_startup = if cfg!(windows) {
        "stdio:cmd /C 'echo npm error 404 Not Found 1>&2 & exit 3'"
    } else {
        "stdio:/bin/sh -c 'echo npm error 404 Not Found >&2; exit 3'"
    };
    let o = run(mcpdial(&home).args(["info", dies_on_startup]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("exited with status 3"), "{}", o.stderr);
    assert!(
        o.stderr.contains("| npm error 404 Not Found"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "--timeout", "3", "ls"]));
    let _ = o; // ls is covered elsewhere; this just proves nothing hangs after a dead server

    let o = run(mcpdial(&home).args(["info", "stdio:/definitely/not/a/program"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("could not start"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["login", &target]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("stdio servers need no token"));
}

#[test]
fn stdio_env_and_cwd_are_passed_to_the_process() {
    let home = temp_home("env");
    let echo = echo_command();
    let o = run(mcpdial(&home).args([
        "add",
        "tagged",
        "--stdio",
        &echo,
        "--env",
        "ECHO_SERVER_TAG=hello",
        "--cwd",
        home.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["info", "tagged"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("tag=hello"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["add", "bad", "--stdio", &echo, "--env", "NOEQUALS"]));
    assert_eq!(o.code, 2);
    let o = run(mcpdial(&home).args(["add", "bad", "--http", "http://x/mcp", "--env", "A=1"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("only apply to --stdio"));
}

#[test]
fn stdio_answers_what_the_server_asks_mid_call() {
    let home = temp_home("ping");
    let target = format!("stdio:{}", echo_command());

    // In this mode the server interrupts `tools/call` with a notification, a
    // `ping`, and a request we do not serve, and finishes the call only once both
    // requests are answered correctly. A client that just waits for its own id
    // deadlocks here and reports the timeout as the server's fault.
    let o = run(mcpdial(&home).env("ECHO_SERVER_PING", "1").args([
        "-v",
        "--timeout",
        "20",
        "call",
        &target,
        "echo",
        r#"{"message":"mid-call"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: mid-call");

    // The trace shows all three: the notification skipped, the ping answered with
    // an empty result, and the unsupported method refused with -32601.
    assert!(
        o.stderr.contains("(other message)") && o.stderr.contains("notifications/message"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(r#"{"id":"srv-ping","jsonrpc":"2.0","result":{}}"#),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains(r#""code":-32601"#) && o.stderr.contains("srv-roots"),
        "{}",
        o.stderr
    );
}

#[test]
fn http_sessions_are_terminated_on_close() {
    let s = start(Mode::Stateful);
    let home = temp_home("session-delete");

    let (_, stderr, code) = shell(&home, &s.url, false, "call add {\"a\":1,\"b\":2}\nquit\n");
    assert_eq!(code, Some(0), "{stderr}");
    let deletes: Vec<_> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "DELETE")
        .cloned()
        .collect();
    assert_eq!(deletes.len(), 1, "the shell ends its session, exactly once");
    assert_eq!(deletes[0].path, "/mcp");
    assert_eq!(deletes[0].header("mcp-session-id"), Some("sess-1"));
    assert_eq!(
        deletes[0].header("mcp-protocol-version"),
        Some("2025-06-18")
    );
    assert!(
        deletes[0]
            .header("user-agent")
            .unwrap()
            .starts_with("Mozilla/5.0"),
        "the same client that sent the POSTs"
    );

    let o = run(mcpdial(&home).args(["-v", "call", &s.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("-> DELETE"), "{}", o.stderr);

    let refuses = start(Mode::StatefulNoDelete);
    let o = run(mcpdial(&home).args(["call", &refuses.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 1 is 2.");
    assert_eq!(o.stderr, "", "a 405 never reaches the user");
    assert!(refuses
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "DELETE"));

    let stateless = start(Mode::Stateless);
    let o = run(mcpdial(&home).args(["call", &stateless.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !stateless
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "DELETE"),
        "a stateless server never issued a session to end"
    );
}

#[test]
fn a_url_that_streams_no_endpoint_event_keeps_its_own_error() {
    let s = start(Mode::Stateless);
    let home = temp_home("not-legacy");

    let o = run(mcpdial(&home).args(["info", &format!("{}/nope", s.base)]));
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("HTTP 404"), "{}", o.stderr);
    assert!(!o.stderr.contains("HTTP+SSE"), "{}", o.stderr);
    assert!(
        s.requests.lock().unwrap().iter().any(|r| r.method == "GET"),
        "the 404 should still have been checked"
    );
}
