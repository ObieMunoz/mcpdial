mod common;

use common::{
    echo_server, mcpdial, run, start, temp_home, Mode, CONFIDENTIAL_ID,
    CONFIDENTIAL_SECRET as SECRET,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn stateful_http_handshake_session_and_call() {
    let s = start(Mode::Stateful);
    let home = temp_home("stateful");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(o.stdout.contains("capabilities: tools"));

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
        run(mcpdial(&home).args(["add", "paged", "--http", &s.url])).code,
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

    let o = run(mcpdial(&home).args(["info", "no-such-server"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("unknown server"));

    let o = run(mcpdial(&home).args(["--timeout", "2", "info", "http://127.0.0.1:1/mcp"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("could not reach"), "{}", o.stderr);
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
    let target = format!("stdio:{}", echo_server().display());

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
    let o = run(mcpdial(&home).args([
        "info",
        "stdio:/bin/sh -c 'echo npm error 404 Not Found >&2; exit 3'",
    ]));
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
fn saved_servers_and_status_listing() {
    let http = start(Mode::Stateful);
    let auth = start(Mode::Auth {
        tokens: vec!["secret".into()],
    });
    let blocked = start(Mode::Blocked);
    let home = temp_home("ls");
    let echo = echo_server().display().to_string();

    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &http.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "local", "--stdio", &echo])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "locked", "--http", &auth.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "wall", "--http", &blocked.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "dead", "--http", "http://127.0.0.1:1/mcp"])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args([
            "add",
            "envtok",
            "--http",
            &auth.url,
            "--token-env",
            "FAKE_TOKEN"
        ]))
        .code,
        0
    );
    let o = run(mcpdial(&home).args(["add", "bad name", "--http", "x"]));
    assert_eq!(o.code, 2);
    let o = run(mcpdial(&home).args(["add", "both", "--http", "x", "--stdio", "y"]));
    assert_ne!(o.code, 0);

    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("web") && o.stdout.contains("local") && o.stdout.contains("stdio"));
    assert!(o.stdout.contains("$FAKE_TOKEN"));

    let o =
        run(mcpdial(&home)
            .env("FAKE_TOKEN", "secret")
            .args(["--timeout", "3", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let by_name = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap().clone();
    assert_eq!(by_name("web")["status"]["state"], "connected");
    assert_eq!(by_name("web")["server"], "fake-mcp 1.0");
    assert_eq!(by_name("web")["tools"], 2);
    assert_eq!(by_name("local")["status"]["state"], "connected");
    assert_eq!(by_name("local")["server"], "echo-server 0.0.1");
    assert_eq!(by_name("locked")["status"]["state"], "auth_required");
    assert_eq!(by_name("wall")["status"]["state"], "blocked");
    assert_eq!(by_name("dead")["status"]["state"], "unreachable");
    assert_eq!(by_name("envtok")["status"]["state"], "connected");
    assert_eq!(by_name("envtok")["auth"], "env");

    let o = run(mcpdial(&home)
        .env("FAKE_TOKEN", "wrong")
        .args(["--timeout", "3", "ls"]));
    assert!(o.stdout.contains("token rejected"), "{}", o.stdout);
    assert!(o.stdout.contains("auth required"));
    assert!(o.stdout.contains("blocked (403)"));
    assert!(o.stdout.contains("unreachable"));

    // Every server's tools at once.
    let o = run(mcpdial(&home)
        .env("FAKE_TOKEN", "secret")
        .args(["--timeout", "3", "tools"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("## web  fake-mcp 1.0  (2 tools)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("## local  echo-server 0.0.1  (4 tools)"));
    assert!(o.stdout.contains("## locked  auth required"));
    assert!(o.stdout.contains("## dead  unreachable:"));

    let o = run(mcpdial(&home).args(["tools", "web", "--long"]));
    assert!(
        o.stdout.contains("Second line"),
        "long listing shows the full description"
    );
    assert!(
        o.stdout
            .contains("message: string (required) - What to echo"),
        "{}",
        o.stdout
    );

    assert_eq!(run(mcpdial(&home).args(["rm", "dead"])).code, 0);
    assert_eq!(run(mcpdial(&home).args(["rm", "dead"])).code, 2);
    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert!(!o.stdout.contains("dead"));
}

/// Run `login` with no browser, scrape the auth URL from stderr, and play the browser
/// ourselves: the fake authorization server 302s straight back to the loopback callback.
fn drive_login(home: &std::path::Path, target: &str, extra: &[&str]) -> String {
    let mut child = mcpdial(home)
        .args(["login", target, "--no-browser"])
        .args(extra)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep draining stderr for the life of the process, or its next progress line
    // hits a closed pipe. Hand the auth URL over as soon as it appears.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.starts_with("http://127.0.0.1:") && t.contains("/authorize?") {
                let _ = tx.send(t.to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let auth_url = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(u) => u,
        Err(_) => {
            let _ = child.kill();
            panic!(
                "login printed no authorization URL:\n{}",
                drain.join().unwrap()
            );
        }
    };
    assert!(auth_url.contains("code_challenge_method=S256"));
    assert!(
        auth_url.contains("scope=mcp"),
        "scope from discovery: {auth_url}"
    );
    // Follows the 302 to the loopback callback, which hands mcpdial the code.
    let mut resp = ureq::get(&auth_url).call().expect("authorize -> callback");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp
        .body_mut()
        .read_to_string()
        .unwrap()
        .contains("close this tab"));
    let status = child.wait().unwrap();
    let log = drain.join().unwrap();
    assert!(status.success(), "login exited {status}:\n{log}");
    log
}

#[test]
fn redirects_are_refused_not_followed() {
    let s = start(Mode::Stateless);
    let home = temp_home("redirect");
    let moved = format!("{}/moved", s.base);
    let o = run(mcpdial(&home).args(["info", &moved]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("redirected (301) to"), "{}", o.stderr);
    assert!(
        o.stderr.contains(&s.url),
        "names the final URL: {}",
        o.stderr
    );
    assert_eq!(s.requests.lock().unwrap().len(), 1, "did not follow");

    let o = run(mcpdial(&home).args(["login", &moved, "--no-browser"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("redirected (301) to"), "{}", o.stderr);
}

#[test]
fn login_falls_back_to_localhost_when_127_is_refused() {
    let s = start(Mode::AuthLocalhostOnly);
    let home = temp_home("localhost");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("refused http://127.0.0.1"), "{log}");
    assert!(log.contains("retrying with http://localhost"), "{log}");
    assert!(log.contains("registered client client-abc"), "{log}");
    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(
        creds.contains("\"redirect_host\": \"localhost\""),
        "{creds}"
    );
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    // The saved client id is reused on a second login, with the host it was registered for.
    let log = drive_login(&home, "work", &[]);
    assert!(
        !log.contains("registered client"),
        "should reuse the client id:\n{log}"
    );
    assert!(!log.contains("refused"), "{log}");

    // Forcing 127.0.0.1 gets the server-side hint instead of a silent retry.
    let o = run(mcpdial(&home).args(["logout", "work"]));
    assert_eq!(o.code, 0);
    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--no-browser",
        "--redirect-host",
        "127.0.0.1",
    ]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("refused the loopback redirect URI"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains("force_ssl_in_redirect_uri"),
        "{}",
        o.stderr
    );
}

#[test]
fn oauth_login_saves_a_token_and_refreshes_it() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("oauth");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"x"}"#]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("HTTP 401") && o.stderr.contains("mcpdial login"),
        "{}",
        o.stderr
    );

    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("registered client client-abc"), "{log}");
    assert!(log.contains("saved token for work (expires in"), "{log}");

    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(creds.contains("\"access_token\": \"tok-1\""), "{creds}");
    assert!(creds.contains("\"refresh_token\": \"ref-1\""));
    assert!(creds.contains("\"client_id\": \"client-abc\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("credentials.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // The saved token is used, and the browser is never needed again.
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"with saved token"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: with saved token");
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-1"));

    let o = run(mcpdial(&home).args(["ls"]));
    assert!(
        o.stdout.contains("connected") && o.stdout.contains("saved"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("access token:  present"));
    assert!(o.stdout.contains("refresh token: present"));
    assert!(!o.stdout.contains("tok-1"), "never print the secret");

    // Clock says the token is stale: refresh happens before the call.
    let mut v: Value = serde_json::from_str(&creds).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(home.join("credentials.json"), v.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"after refresh"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-2"));
    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(creds.contains("\"access_token\": \"tok-2\""));
    assert!(
        creds.contains("\"refresh_token\": \"ref-2\""),
        "rotated refresh token is kept: {creds}"
    );

    // Clock says fine but the server disagrees: one refresh and retry, transparently.
    let mut v: Value = serde_json::from_str(&creds).unwrap();
    v["credentials"]["work"]["access_token"] = Value::from("revoked");
    v["credentials"]["work"]["expires_at"] = Value::from(4_000_000_000u64);
    std::fs::write(home.join("credentials.json"), v.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"after 401"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: after 401");
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-3"));

    // Logout forgets it.
    assert_eq!(run(mcpdial(&home).args(["logout", "work"])).code, 0);
    let o = run(mcpdial(&home).args(["ls"]));
    assert!(o.stdout.contains("auth required"), "{}", o.stdout);
    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 1);
}

#[test]
fn manual_token_from_stdin_or_env() {
    let s = start(Mode::Auth {
        tokens: vec!["pasted".into(), "from-env".into()],
    });
    let home = temp_home("manual");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let mut child = mcpdial(&home)
        .args(["token", "set", "work"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"pasted\n").unwrap();
    assert!(child.wait().unwrap().success());

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer pasted")
    );

    let o = run(mcpdial(&home)
        .env("T", "from-env")
        .args(["token", "set", "work", "--env", "T"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer from-env")
    );

    // --token-env on the command line beats the saved credential.
    let o = run(mcpdial(&home).env("OVERRIDE", "pasted").args([
        "--token-env",
        "OVERRIDE",
        "call",
        "work",
        "echo",
        "{}",
    ]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer pasted")
    );

    let o = run(mcpdial(&home).args(["--token-env", "UNSET_VAR_X", "call", "work", "echo", "{}"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("$UNSET_VAR_X is unset"));

    assert_eq!(run(mcpdial(&home).args(["token", "rm", "work"])).code, 0);
    assert_eq!(
        run(mcpdial(&home).args(["call", "work", "echo", "{}"])).code,
        1
    );
}

#[test]
fn extra_headers_are_sent_and_saved() {
    let s = start(Mode::Stateless);
    let home = temp_home("headers");
    let o = run(mcpdial(&home).args(["-H", "X-Team: blue", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(s.requests.lock().unwrap()[0].header("x-team"), Some("blue"));

    assert_eq!(
        run(mcpdial(&home).args(["add", "t", "--http", &s.url, "-H", "X-Saved: yes"])).code,
        0
    );
    let o = run(mcpdial(&home).args(["info", "t"]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests.lock().unwrap().last().unwrap().header("x-saved"),
        Some("yes")
    );

    let o = run(mcpdial(&home).args(["-H", "nocolon", "info", &s.url]));
    assert_eq!(o.code, 2);
}

#[test]
fn stdio_env_and_cwd_are_passed_to_the_process() {
    let home = temp_home("env");
    let echo = echo_server().display().to_string();
    let o = run(mcpdial(&home).args([
        "add",
        "tagged",
        "--stdio",
        &echo,
        "--env",
        "ECHO_SERVER_TAG=hello",
        "--cwd",
        "/",
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
fn shell_keeps_one_session_alive() {
    let home = temp_home("shell");
    let target = format!("stdio:{}", echo_server().display());

    // Separate invocations are separate processes: the counter never gets past 1.
    for _ in 0..2 {
        let o = run(mcpdial(&home).args(["call", &target, "count"]));
        assert_eq!(o.stdout.trim(), "count=1");
    }

    let script = "# a comment\ncall count\ncall count {}\ntools\ncall nope\nraw tools/list\ncall count\nquit\ncall count\n";
    let mut child = mcpdial(&home)
        .args(["shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("count=1\n"), "{stdout}");
    assert!(stdout.contains("count=2\n"), "{stdout}");
    assert!(
        stdout.contains("count=3\n"),
        "same process throughout: {stdout}"
    );
    assert!(!stdout.contains("count=4"), "quit stops reading: {stdout}");
    assert!(stdout.contains("4 tool(s):"), "{stdout}");
    assert!(
        stdout.contains("\"name\": \"count\""),
        "raw output: {stdout}"
    );
    assert!(
        stderr.contains("MCP error -32602"),
        "errors go to stderr and do not end the session: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed command in a script is reported in the exit code"
    );

    let mut child = mcpdial(&home)
        .args(["--json", "shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call count\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["content"][0]["text"], "count=1");
}

/// Every way a line can be wrong should answer with the shape that was wanted.
#[test]
fn shell_explains_the_shape_it_expected() {
    let home = temp_home("shell-hints");
    let target = format!("stdio:{}", echo_server().display());
    let script = concat!(
        "echo\n",                            // a tool name typed as if it were a command
        "call echo\n",                       // a required argument left out
        "call echo [www.x.com](http://x)\n", // arguments that are not JSON at all
        "call ech {\"message\":\"x\"}\n",    // a tool name with a typo
        "tolls\n",                           // a command with a typo
        "schema echo\n",
        "help echo\n",
        "quit\n",
    );
    let (stdout, stderr, code) = shell(&home, &target, false, script);

    // A bare tool name is the commonest mistake, and it names the fix.
    assert!(stderr.contains("echo is a tool, not a command"), "{stderr}");
    // Every failed call answers with a line that would have worked.
    assert_eq!(
        stderr
            .matches(r#"usage: call echo {"message": "<string>"}"#)
            .count(),
        4,
        "bare name, missing argument, unparseable argument and `help echo`: {stderr}"
    );
    assert!(
        stderr.contains("message: string (required)"),
        "and the parameter list: {stderr}"
    );
    // Unparseable arguments quote what actually arrived.
    assert!(
        stderr.contains(r#""[www.x.com](http://x)" is not JSON"#),
        "{stderr}"
    );
    // Near misses are named, for tools and for commands.
    assert!(stderr.contains("did you mean echo?"), "{stderr}");
    assert!(stderr.contains("did you mean tools?"), "{stderr}");
    // schema prints the tool's own schema; help prints the readable form.
    assert!(
        stdout.contains(r#""required": ["#) && stdout.contains(r#""message""#),
        "{stdout}"
    );
    assert_eq!(code, Some(1), "a script with failures still exits 1");

    // A schema complaint that arrives as a failed *result* rather than a JSON-RPC
    // error is the same mistake, and gets the same answer.
    let (stdout, stderr, _) = shell(&home, &target, false, "call strict {}\n");
    assert!(stdout.contains("Required at pageId"), "{stdout}");
    assert!(
        stderr.contains(r#"usage: call strict {"pageId": <number>}"#),
        "a failed result still explains itself: {stderr}"
    );
    // A tool that just failed does not get a schema dumped under it.
    let (_, stderr, _) = shell(&home, &target, false, "call fail {}\n");
    assert!(!stderr.contains("usage:"), "{stderr}");

    // In --json the hint rides along on the error object, one line per command.
    let (stdout, _, _) = shell(&home, &target, true, "call echo\ncall nope {}\n");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(
        lines[0]["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with(r#"usage: call echo {"message": "<string>"}"#),
        "{stdout}"
    );
    assert!(
        lines[1]["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("lists all 4"),
        "{stdout}"
    );
}

/// Pipe `script` into `mcpdial shell` and collect everything it said.
fn shell(
    home: &std::path::Path,
    target: &str,
    json: bool,
    script: &str,
) -> (String, String, Option<i32>) {
    let mut cmd = mcpdial(home);
    if json {
        cmd.arg("--json");
    }
    let mut child = cmd
        .args(["shell", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
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
fn import_reads_host_configs() {
    let home = temp_home("import");
    let echo = echo_server().display().to_string();
    let s = start(Mode::Stateless);
    let cfg = home.join("hostconfig.json");
    std::fs::write(
        &cfg,
        serde_json::json!({
            "mcpServers": {
                "local": {"type": "stdio", "command": echo, "args": [], "env": {"ECHO_SERVER_TAG": "imported"}},
                "web": {"type": "http", "url": s.url, "headers": {"X-From": "import"}}
            },
            "projects": {"/some/project": {"mcpServers": {"spaced": {"command": "npx", "args": ["-y", "pkg", "/tmp/a b"]}}}},
            "oauthAccount": {"accessToken": "never-read"}
        })
        .to_string(),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 3 server(s)"), "{}", o.stderr);
    assert!(o.stderr.contains("projects./some/project"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["info", "local"]));
    assert!(
        o.stdout.contains("tag=imported"),
        "env came through: {}",
        o.stdout
    );
    let o = run(mcpdial(&home).args(["info", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        s.requests.lock().unwrap()[0].header("x-from"),
        Some("import")
    );
    let saved = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert!(saved.contains("npx -y pkg '/tmp/a b'"), "{saved}");
    assert!(!saved.contains("never-read"));

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert!(
        o.stderr.contains("skip") && o.stderr.contains("imported 0 server(s)"),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap(), "--force"]));
    assert!(o.stderr.contains("imported 3 server(s)"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["import", "/nonexistent/file.json"]));
    assert_eq!(o.code, 2);
}

#[test]
fn agent_surface_json_errors_file_args_schema_and_guide() {
    let s = start(Mode::Stateless);
    let auth = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("agent");

    // Errors are one JSON object on stderr under --json, with a kind to branch on.
    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "rpc");
    assert_eq!(e["error"]["code"], -32602);
    assert!(o.stdout.is_empty());

    let o = run(mcpdial(&home).args(["--json", "info", &auth.url]));
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "http");
    assert_eq!(e["error"]["status"], 401);
    assert!(e["error"]["www_authenticate"]
        .as_str()
        .unwrap()
        .contains("resource_metadata"));

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "echo", "{bad"]));
    assert_eq!(o.code, 2);
    assert_eq!(
        serde_json::from_str::<Value>(o.stderr.trim()).unwrap()["error"]["kind"],
        "usage"
    );

    // Arguments from a file and from stdin.
    let f = home.join("args.json");
    std::fs::write(&f, r#"{"message":"from a file"}"#).unwrap();
    let at = format!("@{}", f.display());
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", &at]));
    assert_eq!(o.stdout.trim(), "Echo: from a file", "{}", o.stderr);
    let mut child = mcpdial(&home)
        .args(["--json", "call", &s.url, "echo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"message":"from stdin"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], "Echo: from stdin");
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "@/nonexistent.json"]));
    assert_eq!(o.code, 2);

    // One tool's schema, and a helpful miss.
    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["name"], "add");
    assert_eq!(v["inputSchema"]["required"][0], "a");
    let o = run(mcpdial(&home).args(["schema", &s.url, "nah"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("available: echo, add"), "{}", o.stderr);

    // The guide is embedded in the binary.
    let o = run(mcpdial(&home).args(["guide"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("# mcpdial for agents"));
    assert!(o.stdout.contains("Exit codes"));

    // Shell in JSON mode keeps errors on stdout, in order.
    let mut child = mcpdial(&home)
        .args(["--json", "shell", &s.url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call echo {\"message\":\"one\"}\ncall nope\ncall echo {\"message\":\"two\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines[0]["content"][0]["text"], "Echo: one");
    assert_eq!(lines[1]["error"]["kind"], "rpc");
    assert_eq!(lines[2]["content"][0]["text"], "Echo: two");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn the_protocol_version_header_rides_every_request_after_initialize() {
    let s = start(Mode::Stateless);
    let home = temp_home("protocol-version");

    let o = run(mcpdial(&home).args(["tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let reqs = s.requests.lock().unwrap();
    assert!(
        reqs.iter().all(|r| r.header("mcp-session-id").is_none()),
        "a stateless server hands out no session id"
    );
    for r in reqs.iter() {
        let msg = r.json();
        let method = msg["method"].as_str().unwrap_or_default();
        let sent = r.header("mcp-protocol-version");
        if method == "initialize" {
            assert_eq!(sent, None, "nothing is negotiated yet on initialize");
        } else {
            assert_eq!(sent, Some("2025-06-18"), "missing on {method}");
        }
    }
    for method in ["tools/list", "tools/call"] {
        assert!(
            reqs.iter().any(|r| r.json()["method"] == method),
            "{method} never reached the server"
        );
    }
}

#[test]
fn the_version_on_the_wire_is_the_one_the_server_agreed_to() {
    let s = start(Mode::OlderProtocol);
    let home = temp_home("older-protocol");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let reqs = s.requests.lock().unwrap();
    let after_the_handshake = &reqs[1..];
    assert!(!after_the_handshake.is_empty());
    for r in after_the_handshake {
        assert_eq!(
            r.header("mcp-protocol-version"),
            Some("2024-11-05"),
            "{}",
            r.body
        );
    }
}

#[test]
fn stdio_answers_what_the_server_asks_mid_call() {
    let home = temp_home("ping");
    let target = format!("stdio:{}", echo_server().display());

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
fn completion_scripts_for_every_shell() {
    let home = temp_home("completions");

    for (shell, registration_line) in [
        ("bash", "complete -F _mcpdial"),
        ("zsh", "#compdef mcpdial"),
        ("fish", "complete -c mcpdial"),
        ("elvish", "edit:completion:arg-completer[mcpdial]"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let o = run(mcpdial(&home).args(["completions", shell]));
        assert_eq!(o.code, 0, "{shell}: {}", o.stderr);
        assert!(!o.stdout.is_empty(), "{shell}: empty script");
        assert!(
            o.stdout.contains(registration_line),
            "{shell}: no {registration_line:?}"
        );
        assert!(o.stdout.contains("token-env"), "{shell}: no global flags");
        assert!(
            o.stdout.contains("no-probe"),
            "{shell}: no per-command flags"
        );
    }

    let o = run(mcpdial(&home).args(["completions", "csh"]));
    assert_eq!(o.code, 2);
    assert!(o.stdout.is_empty());

    let o = run(mcpdial(&home).args(["--help"]));
    assert_eq!(o.code, 0);
    assert!(!o.stdout.contains("completions"), "{}", o.stdout);
}

#[test]
fn a_structured_only_result_still_prints() {
    let s = start(Mode::Stateless);
    let home = temp_home("structured");

    let o = run(mcpdial(&home).args(["call", &s.url, "reading"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["celsius"], 20, "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "reading"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["structuredContent"]["celsius"], 20);
}

#[test]
fn content_blocks_win_over_structured_content() {
    let s = start(Mode::Stateless);
    let home = temp_home("both-shapes");

    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
}

#[test]
fn schema_shows_an_output_schema_and_names_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-schema");

    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["outputSchema"]["required"][0], "sum");
    assert!(o.stderr.contains("structuredContent"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["schema", &s.url, "echo"]));
    assert!(o.stderr.is_empty(), "{}", o.stderr);
}

/// `drive_login` for a login that has a secret to supply: the process needs stdin,
/// or an environment, and a refused login has to be observable rather than fatal.
fn drive_secret_login(cmd: &mut Command, stdin_secret: Option<&str>) -> (bool, String) {
    let mut child = cmd
        .stdin(match stdin_secret {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(secret) = stdin_secret {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(secret.as_bytes()).unwrap();
    }
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.starts_with("http://127.0.0.1:") && t.contains("/authorize?") {
                let _ = tx.send(t.to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let auth_url = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(u) => u,
        Err(_) => {
            let _ = child.kill();
            panic!(
                "login printed no authorization URL:\n{}",
                drain.join().unwrap()
            );
        }
    };
    assert!(auth_url.contains("client_id=conf-client"), "{auth_url}");
    ureq::get(&auth_url).call().expect("authorize -> callback");
    let ok = child.wait().unwrap().success();
    (ok, drain.join().unwrap())
}

fn confidential_login(home: &std::path::Path, extra: &[&str]) -> Command {
    let mut cmd = mcpdial(home);
    cmd.args([
        "login",
        "work",
        "--no-browser",
        "--client-id",
        CONFIDENTIAL_ID,
    ])
    .args(extra);
    cmd
}

fn expire_saved_token(home: &std::path::Path) {
    let path = home.join("credentials.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(path, v.to_string()).unwrap();
}

fn last_token_request(s: &common::FakeServer) -> common::Recorded {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.path == "/token")
        .expect("a token request")
        .clone()
}

#[test]
fn a_confidential_client_posts_its_secret_and_refreshes_with_it() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("confidential-post");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let (ok, log) = drive_secret_login(&mut confidential_login(&home, &[]), None);
    assert!(
        !ok,
        "a confidential client cannot log in without its secret"
    );
    assert!(log.contains("token exchange failed: HTTP 401"), "{log}");

    let (ok, log) = drive_secret_login(
        &mut confidential_login(&home, &["--client-secret"]),
        Some(SECRET),
    );
    assert!(ok, "{log}");
    assert!(log.contains("saved token for work (expires in"), "{log}");

    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(
        creds.contains(&format!("\"client_secret\": \"{SECRET}\"")),
        "{creds}"
    );
    assert!(
        creds.contains("\"token_endpoint_auth_method\": \"client_secret_post\""),
        "{creds}"
    );

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert!(o
        .stdout
        .contains("client secret: present (client_secret_post)"));
    assert!(!o.stdout.contains(SECRET), "never print the secret");
    let o = run(mcpdial(&home).args(["token", "show", "work", "--json"]));
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap()["has_client_secret"],
        Value::Bool(true)
    );
    assert!(!o.stdout.contains(SECRET), "never print the secret");

    // Unattended from here on: the saved secret is what makes the refresh succeed.
    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"refreshed"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: refreshed");
    let refresh = last_token_request(&s);
    assert!(
        refresh.body.contains("grant_type=refresh_token"),
        "{refresh:?}"
    );
    assert!(refresh
        .body
        .contains("client_secret=c0nf%26s3cr3t%3D%2F%3Ax"));
}

#[test]
fn a_confidential_client_uses_basic_auth_when_that_is_all_the_server_offers() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_basic".into(),
    });
    let home = temp_home("confidential-basic");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let mut cmd = confidential_login(&home, &["--client-secret-env", "MCPDIAL_TEST_SECRET"]);
    cmd.env("MCPDIAL_TEST_SECRET", SECRET);
    let (ok, log) = drive_secret_login(&mut cmd, None);
    assert!(ok, "{log}");

    let exchange = last_token_request(&s);
    assert!(!exchange.body.contains("client_secret"), "{exchange:?}");
    assert_eq!(
        exchange.header("authorization"),
        Some("Basic Y29uZi1jbGllbnQ6YzBuZiUyNnMzY3IzdCUzRCUyRiUzQXg=")
    );

    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"basic"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let refresh = last_token_request(&s);
    assert!(
        refresh.body.contains("grant_type=refresh_token"),
        "{refresh:?}"
    );
    assert!(refresh.header("authorization").is_some(), "{refresh:?}");
}

#[test]
fn a_client_secret_reaches_neither_an_argument_nor_the_trace_output() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("confidential-quiet");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--client-id",
        "x",
        "--client-secret",
        SECRET,
    ]));
    assert_eq!(o.code, 2, "the flag must take no value: {}", o.stderr);
    let o = run(mcpdial(&home).args(["login", "work", "--client-secret"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("--client-id"), "{}", o.stderr);

    let (ok, log) = drive_secret_login(
        &mut confidential_login(&home, &["--client-secret", "-v"]),
        Some(SECRET),
    );
    assert!(ok, "{log}");
    assert!(
        log.contains("authorization server:"),
        "the log is real: {log}"
    );
    assert!(!log.contains(SECRET), "the secret leaked into -v:\n{log}");

    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"x"}"#, "-v"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("-> POST"),
        "the trace is on: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains(SECRET) && !o.stdout.contains(SECRET),
        "the secret leaked into -v:\n{}",
        o.stderr
    );
}

#[test]
fn a_2024_11_05_server_is_named_rather_than_reported_as_a_bare_status() {
    let s = start(Mode::LegacySse);
    let home = temp_home("legacy-sse");
    assert_eq!(
        run(mcpdial(&home).args(["add", "old", "--http", &s.url])).code,
        0
    );

    // No --timeout: a probe that read the stream to its end would sit here for the
    // default 60s instead, and the server never ends it.
    let started = std::time::Instant::now();
    let o = run(mcpdial(&home).args(["info", "old"]));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the probe waited out a stream that never ends"
    );
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("HTTP+SSE"), "{}", o.stderr);
    assert!(o.stderr.contains("2024-11-05"), "{}", o.stderr);
    assert!(!o.stderr.contains("HTTP 405"), "{}", o.stderr);

    let probed = s.requests.lock().unwrap().iter().any(|r| r.method == "GET");
    assert!(probed, "the failed POST should have been followed by a GET");

    let o = run(mcpdial(&home).args(["ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("legacy sse"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "legacy_sse");
}

#[test]
fn a_server_that_answers_is_never_probed_for_the_older_transport() {
    let s = start(Mode::Stateful);
    let home = temp_home("no-legacy-probe");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let requests = s.requests.lock().unwrap();
    assert!(
        requests.iter().all(|r| r.method != "GET"),
        "a healthy server costs no extra round trip: {:?}",
        requests.iter().map(|r| &r.method).collect::<Vec<_>>()
    );
}

#[test]
fn a_public_credential_saved_before_secrets_existed_still_refreshes() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("public-refresh");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );
    std::fs::write(
        home.join("credentials.json"),
        json!({"credentials": {"work": {
            "access_token": "stale", "refresh_token": "ref-1", "expires_at": 1_000_000u64,
            "token_endpoint": format!("{}/token", s.base), "client_id": "client-abc",
            "resource": s.url, "source": "oauth"}}})
        .to_string(),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"public"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let refresh = last_token_request(&s);
    assert!(!refresh.body.contains("client_secret"), "{refresh:?}");
    assert!(refresh.header("authorization").is_none(), "{refresh:?}");

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert!(!o.stdout.contains("client secret"), "{}", o.stdout);
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
