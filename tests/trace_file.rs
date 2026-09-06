//! `--trace FILE`: the wire exchange as JSON Lines, secrets left out.

mod common;

use common::{
    echo_server, mcpdial, run, start, temp_home, Mode, CONFIDENTIAL_ID, CONFIDENTIAL_SECRET,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn records(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

/// The records with this `dir` whose message has this `method`.
fn messages<'a>(records: &'a [Value], dir: &str, method: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["dir"] == dir && r["message"]["method"] == method)
        .collect()
}

fn events<'a>(records: &'a [Value], event: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["dir"] == "event" && r["event"] == event)
        .collect()
}

/// What every line must carry, whatever else it says.
fn assert_well_formed(records: &[Value], transport: &str, target: &str) {
    assert!(!records.is_empty(), "an empty trace");
    for r in records {
        let t = r["t"]
            .as_str()
            .unwrap_or_else(|| panic!("no timestamp: {r}"));
        assert!(
            t.len() == 24 && t.as_bytes()[10] == b'T' && t.ends_with('Z'),
            "not an ISO 8601 UTC instant: {t}"
        );
        assert!(
            matches!(r["dir"].as_str(), Some("send" | "recv" | "event")),
            "{r}"
        );
        assert_eq!(r["transport"], transport, "{r}");
        assert_eq!(r["target"], target, "{r}");
        match r["dir"].as_str() {
            Some("event") => {
                assert!(r["event"].is_string(), "{r}");
                assert!(r["detail"].is_object(), "{r}");
                assert!(r.get("message").is_none(), "{r}");
            }
            _ => assert!(r["message"].is_object(), "{r}"),
        }
    }
}

fn trace_path(home: &Path) -> PathBuf {
    home.join("trace.jsonl")
}

fn stdio_target() -> String {
    format!("stdio:'{}'", echo_server().display())
}

#[test]
fn a_stdio_call_is_traced_from_spawn_to_exit() {
    let home = temp_home("trace-stdio");
    let trace = trace_path(&home);
    let target = stdio_target();

    let o = run(mcpdial(&home).args([
        "call",
        &target,
        "echo",
        r#"{"message":"hi"}"#,
        "--trace",
        trace.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");
    assert!(
        !o.stderr.contains("-> stdin"),
        "--trace alone leaves stderr quiet:\n{}",
        o.stderr
    );

    let records = records(&trace);
    assert_well_formed(&records, "stdio", &target);

    let calls = messages(&records, "send", "tools/call");
    assert_eq!(calls.len(), 1, "{records:?}");
    assert_eq!(calls[0]["message"]["params"]["name"], "echo");
    assert_eq!(calls[0]["message"]["params"]["arguments"]["message"], "hi");
    let id = &calls[0]["message"]["id"];
    let reply = records
        .iter()
        .find(|r| r["dir"] == "recv" && &r["message"]["id"] == id)
        .expect("the call's reply");
    assert_eq!(reply["message"]["result"]["content"][0]["text"], "Echo: hi");
    assert_eq!(messages(&records, "send", "initialize").len(), 1);
    assert_eq!(
        messages(&records, "send", "notifications/initialized").len(),
        1
    );

    let spawned = events(&records, "spawn");
    assert_eq!(spawned.len(), 1, "{records:?}");
    assert!(spawned[0]["detail"]["pid"].is_u64(), "{}", spawned[0]);
    assert_eq!(
        spawned[0]["detail"]["command"][0],
        echo_server().to_str().unwrap()
    );
    assert_eq!(records[0]["event"], "spawn", "the process comes first");

    let exited = events(&records, "exit");
    assert_eq!(exited.len(), 1, "{records:?}");
    assert_eq!(exited[0]["detail"]["status"], 0, "a clean exit on EOF");
    assert!(exited[0]["detail"]["stderr_tail"].is_array());
    assert_eq!(
        records.last().unwrap()["event"],
        "exit",
        "and goes last: {records:?}"
    );

    let noise = events(&records, "not_json");
    assert_eq!(noise.len(), 1, "{records:?}");
    assert_eq!(noise[0]["detail"]["line"], "echo_server ready");
}

#[test]
fn a_crashed_server_leaves_its_status_and_stderr_in_the_trace() {
    let home = temp_home("trace-crash");
    let trace = trace_path(&home);
    let target = stdio_target();

    let o = run(mcpdial(&home)
        .args([
            "call",
            &target,
            "echo",
            r#"{"message":"hi"}"#,
            "--trace",
            trace.to_str().unwrap(),
        ])
        .env("ECHO_SERVER_EXIT_ON_CALL", "1"));
    assert_eq!(o.code, 1, "{}", o.stderr);

    let records = records(&trace);
    let exited = events(&records, "exit");
    assert_eq!(exited.len(), 1, "{records:?}");
    assert_eq!(exited[0]["detail"]["status"], 9);
    let tail = exited[0]["detail"]["stderr_tail"].as_array().unwrap();
    assert!(
        tail.iter()
            .any(|l| l.as_str().unwrap_or("").contains("crashing on call")),
        "{tail:?}"
    );
}

#[test]
fn an_http_call_is_traced_without_its_bearer_token() {
    let token = "tok-do-not-write-me-down";
    let s = start(Mode::Auth {
        tokens: vec![token.into()],
    });
    let home = temp_home("trace-http");
    let trace = trace_path(&home);

    let o = run(mcpdial(&home)
        .args([
            "call",
            &s.url,
            "echo",
            r#"{"message":"hi"}"#,
            "--token-env",
            "MCP_TOKEN",
            "--trace",
            trace.to_str().unwrap(),
        ])
        .env("MCP_TOKEN", token));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let text = std::fs::read_to_string(&trace).unwrap();
    assert!(!text.contains("Bearer"), "{text}");
    assert!(!text.contains(token), "{text}");
    assert!(!text.contains("Authorization"), "{text}");

    let records = records(&trace);
    assert_well_formed(&records, "http", &s.url);
    assert_eq!(messages(&records, "send", "initialize").len(), 1);
    let calls = messages(&records, "send", "tools/call");
    assert_eq!(calls.len(), 1);
    let id = &calls[0]["message"]["id"];
    let reply = records
        .iter()
        .find(|r| r["dir"] == "recv" && &r["message"]["id"] == id)
        .expect("the call's reply");
    assert_eq!(reply["message"]["result"]["content"][0]["text"], "Echo: hi");

    let http = events(&records, "http");
    assert!(http.len() >= 2, "one per request: {records:?}");
    for e in &http {
        assert_eq!(e["detail"]["method"], "POST", "{e}");
        assert_eq!(e["detail"]["url"], s.url, "{e}");
        // 200 for a request, 202 for the `initialized` notification.
        assert!(
            matches!(e["detail"]["status"].as_u64(), Some(200 | 202)),
            "{e}"
        );
        assert!(
            e["detail"]["content_type"]
                .as_str()
                .is_some_and(|ct| !ct.is_empty()),
            "{e}"
        );
        assert!(e["detail"]["elapsed_ms"].is_u64(), "{e}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&trace).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a trace can hold private tool output");
    }
}

#[test]
fn each_attempt_of_a_retried_request_is_traced() {
    let s = start(Mode::UnavailableOnce);
    let home = temp_home("trace-retry");
    let trace = trace_path(&home);

    let o = run(mcpdial(&home).args(["info", &s.url, "--trace", trace.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let records = records(&trace);
    let retries = events(&records, "retry");
    assert_eq!(retries.len(), 1, "{records:?}");
    assert_eq!(retries[0]["detail"]["after"], "HTTP 503");

    // Whatever opens the session is what the 503 lands on: the failed attempt,
    // the decision, the second attempt and its reply, in that order.
    let at = records.iter().position(|r| r["event"] == "retry").unwrap();
    let (first, refused, again, served) = (
        &records[at - 2],
        &records[at - 1],
        &records[at + 1],
        &records[at + 2],
    );
    assert_eq!(first["dir"], "send", "{records:?}");
    assert_eq!(refused["event"], "http", "{records:?}");
    assert_eq!(refused["detail"]["status"], 503);
    assert_eq!(again["dir"], "send", "{records:?}");
    assert_eq!(
        again["message"], first["message"],
        "the same request went again"
    );
    assert_eq!(served["event"], "http", "{records:?}");
    assert_eq!(served["detail"]["status"], 200);
}

#[test]
fn a_login_traces_discovery_and_the_token_exchange_with_every_secret_masked() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("trace-login");
    let trace = trace_path(&home);
    let o = run(mcpdial(&home).args(["add", "work", "--http", &s.url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home)
        .args([
            "login",
            "work",
            "--grant",
            "client-credentials",
            "--client-id",
            CONFIDENTIAL_ID,
            "--client-secret-env",
            "WORK_SECRET",
            "--trace",
            trace.to_str().unwrap(),
        ])
        .env("WORK_SECRET", CONFIDENTIAL_SECRET)
        .stdin(Stdio::null()));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials.json")).unwrap())
            .unwrap();
    let issued = saved["credentials"]["work"]["access_token"]
        .as_str()
        .expect("a token was saved")
        .to_string();

    let text = std::fs::read_to_string(&trace).unwrap();
    assert!(!text.contains(CONFIDENTIAL_SECRET), "{text}");
    assert!(!text.contains(&issued), "{text}");
    assert!(!text.contains("Authorization"), "{text}");

    let records = records(&trace);
    assert_well_formed(&records, "http", "work");
    let discovery: Vec<&Value> = events(&records, "http")
        .into_iter()
        .filter(|e| e["detail"]["method"] == "GET")
        .collect();
    assert!(
        discovery.iter().any(|e| e["detail"]["url"]
            .as_str()
            .unwrap()
            .contains("/.well-known/")),
        "{records:?}"
    );
    let token_request = records
        .iter()
        .find(|r| r["dir"] == "send" && r["message"]["grant_type"] == "client_credentials")
        .expect("the token request");
    assert_eq!(token_request["message"]["client_secret"], "****");
    assert_eq!(token_request["message"]["client_id"], CONFIDENTIAL_ID);
    let token_reply = records
        .iter()
        .find(|r| r["dir"] == "recv" && r["message"].get("access_token").is_some())
        .expect("the token reply");
    assert_eq!(token_reply["message"]["access_token"], "****");
    assert_eq!(token_reply["message"]["token_type"], "Bearer");
}

#[test]
fn a_failed_login_still_traces_what_discovery_found() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_basic".into(),
    });
    let home = temp_home("trace-login-failed");
    let trace = trace_path(&home);
    let o = run(mcpdial(&home).args(["add", "work", "--http", &s.url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    // No client id, and the server registers nobody: over before a browser is wanted.
    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--no-browser",
        "--trace",
        trace.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 1, "{}", o.stderr);

    let records = records(&trace);
    assert_well_formed(&records, "http", "work");
    assert_eq!(
        messages(&records, "send", "initialize").len(),
        1,
        "the challenge: {records:?}"
    );
    let discovery: Vec<&Value> = events(&records, "http")
        .into_iter()
        .filter(|e| e["detail"]["method"] == "GET")
        .collect();
    assert!(!discovery.is_empty(), "{records:?}");
    assert!(records
        .iter()
        .any(|r| r["dir"] == "recv" && r["message"].get("token_endpoint").is_some()));
}

#[test]
fn the_env_var_names_the_file_and_verbose_is_independent_of_it() {
    let home = temp_home("trace-env");
    let trace = trace_path(&home);
    let target = stdio_target();
    let call = |home: &Path, extra: &[&str]| {
        run(mcpdial(home)
            .args(["call", &target, "echo", r#"{"message":"hi"}"#])
            .args(extra)
            .env("MCPDIAL_TRACE", &trace))
    };

    let o = call(&home, &["-v"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("-> stdin"),
        "-v still speaks: {}",
        o.stderr
    );
    let first = records(&trace);
    assert_well_formed(&first, "stdio", &target);

    let o = call(&home, &[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stderr.contains("-> stdin"), "{}", o.stderr);
    let both = records(&trace);
    assert_eq!(both.len(), first.len() * 2, "appended, not replaced");
    assert_eq!(both[..first.len()], first[..]);
}

#[test]
fn a_trace_file_that_cannot_be_opened_is_a_config_error() {
    let home = temp_home("trace-unwritable");
    let nowhere = home.join("no-such-dir").join("trace.jsonl");
    let o = run(mcpdial(&home).args([
        "--json",
        "--trace",
        nowhere.to_str().unwrap(),
        "call",
        &stdio_target(),
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(e["error"]["kind"], "config", "{}", o.stderr);
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-dir"),
        "{}",
        o.stderr
    );
}
