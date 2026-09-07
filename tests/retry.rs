//! One retry on a transient HTTP failure, and none where a second delivery could
//! do something the first already did.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, timed, Mode, Recorded};
use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

fn methods(reqs: &[Recorded]) -> Vec<String> {
    reqs.iter()
        .filter(|r| r.method == "POST")
        .map(|r| r.json()["method"].as_str().unwrap_or("").to_string())
        .collect()
}

fn tool_calls(reqs: &[Recorded]) -> usize {
    methods(reqs).iter().filter(|m| *m == "tools/call").count()
}

/// Run `shell --json` over `script` and hand back one parsed line per command.
fn shell_lines(
    home: &std::path::Path,
    target: &str,
    script: &str,
    env: &[(&str, &str)],
) -> (i32, String) {
    let mut cmd = mcpdial(home);
    cmd.args(["--json", "shell", target]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn parsed(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_503_before_any_session_is_retried_once_and_traced() {
    let s = start(Mode::UnavailableOnce);
    let home = temp_home("retry-503");

    let o = run(mcpdial(&home).args(["-v", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(
        o.stderr.contains("retrying after HTTP 503 (1 of 1)"),
        "{}",
        o.stderr
    );
    assert_eq!(
        methods(&s.requests.lock().unwrap()),
        [
            "server/discover",
            "server/discover",
            "initialize",
            "notifications/initialized"
        ],
        "the refused request went again, and only it"
    );
}

#[test]
fn a_connection_closed_without_a_reply_is_retried() {
    let s = start(Mode::HangUpOnce);
    let home = temp_home("retry-hangup");

    let o = run(mcpdial(&home).args(["-v", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(
        o.stderr.contains("retrying after connection") && o.stderr.contains("(1 of 1)"),
        "{}",
        o.stderr
    );
    // The dropped connection never reached the server; the retry did.
    assert_eq!(
        methods(&s.requests.lock().unwrap()),
        ["server/discover", "initialize", "notifications/initialized"]
    );

    let s = start(Mode::HangUpOnce);
    let o = run(mcpdial(&home).args(["--no-retry", "--json", "info", &s.url]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "transport", "{}", o.stderr);
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not reach"),
        "{}",
        o.stderr
    );
    assert!(s.requests.lock().unwrap().is_empty());
}

#[test]
fn a_server_that_stays_unavailable_fails_after_exactly_two_attempts() {
    let s = start(Mode::Unavailable);
    let home = temp_home("retry-twice");

    let (took, o) = timed(mcpdial(&home).args(["--json", "--timeout", "1", "info", &s.url]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "http", "{}", o.stderr);
    assert_eq!(e["error"]["status"], 503, "{}", o.stderr);
    // A 503 is the server being down, not one that never had `server/discover`,
    // so the handshake is never reached for.
    assert_eq!(
        methods(&s.requests.lock().unwrap()),
        ["server/discover", "server/discover"]
    );
    assert!(
        took.as_secs() < 30,
        "Retry-After: 3600 is capped at --timeout, took {took:?}"
    );

    let o = run(mcpdial(&home).args(["--no-retry", "--json", "info", &s.url]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["status"], 503, "{}", o.stderr);
    assert_eq!(
        methods(&s.requests.lock().unwrap()),
        ["server/discover", "server/discover", "server/discover"],
        "--no-retry: the first 503 is the answer"
    );
}

#[test]
fn a_tool_call_is_retried_only_where_the_server_cannot_have_run_it() {
    // No session was ever issued, so a gateway 503 proves nothing was processed.
    let s = start(Mode::CallUnavailableOnce { stateful: false });
    let home = temp_home("retry-call");
    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":40,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 40 and 2 is 42.");
    assert_eq!(tool_calls(&s.requests.lock().unwrap()), 2);

    // With a session, the same 503 may have come from behind a server that did
    // the work, and nothing says `add` is safe to run twice.
    let s = start(Mode::CallUnavailableOnce { stateful: true });
    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "add", r#"{"a":40,"b":2}"#]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["status"], 503, "{}", o.stderr);
    assert_eq!(tool_calls(&s.requests.lock().unwrap()), 1, "not retried");

    // Unless the tool list has said so: `add` carries `idempotentHint`.
    let s = start(Mode::CallUnavailableOnce { stateful: true });
    let (_, stdout) = shell_lines(
        &home,
        &s.url,
        "tools\ncall add {\"a\":1,\"b\":2}\ncall echo {\"message\":\"hi\"}\n",
        &[],
    );
    let lines = parsed(&stdout);
    assert_eq!(lines.len(), 3, "{stdout}");
    assert_eq!(
        lines[1]["structuredContent"]["sum"].as_f64(),
        Some(3.0),
        "{stdout}"
    );
    assert_eq!(lines[2]["content"][0]["text"], "Echo: hi", "{stdout}");
    assert_eq!(
        tool_calls(&s.requests.lock().unwrap()),
        3,
        "add went twice, echo once"
    );
}

#[test]
fn a_stdio_server_that_dies_mid_call_is_not_restarted() {
    let home = temp_home("retry-stdio");
    let target = format!("stdio:{}", echo_command());
    let (code, stdout) = shell_lines(
        &home,
        &target,
        "call count\ncall count\ncall count\n",
        &[("ECHO_SERVER_EXIT_ON_CALL", "2")],
    );
    assert_eq!(code, 1, "{stdout}");
    let lines = parsed(&stdout);
    assert_eq!(lines.len(), 3, "{stdout}");
    assert_eq!(lines[0]["content"][0]["text"], "count=1", "{stdout}");
    for died in &lines[1..] {
        let message = died["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("exited with status 9"),
            "the post-mortem, not a fresh process: {stdout}"
        );
        assert!(
            message.contains("| echo_server: crashing on call 2"),
            "{stdout}"
        );
    }
    assert_eq!(
        stdout.matches("count=1").count(),
        1,
        "a restarted server would have counted from 1 again: {stdout}"
    );
}
