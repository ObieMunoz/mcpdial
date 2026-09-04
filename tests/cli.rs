mod common;

use common::{echo_server, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::Stdio;

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
    let last3 = &reqs[reqs.len() - 3..];
    assert_eq!(last3[0].json()["method"], "initialize");
    assert!(
        last3[0]
            .header("user-agent")
            .unwrap()
            .starts_with("Mozilla/5.0"),
        "browser UA is mandatory"
    );
    assert_eq!(
        last3[0].header("accept").unwrap(),
        "application/json, text/event-stream"
    );
    assert!(last3[0].header("mcp-session-id").is_none());
    assert_eq!(last3[1].json()["method"], "notifications/initialized");
    assert!(last3[1].json().get("id").is_none());
    assert_eq!(
        last3[1].header("mcp-session-id"),
        Some("sess-1"),
        "session id is echoed back"
    );
    assert_eq!(last3[2].json()["method"], "tools/call");
    assert_eq!(last3[2].header("mcp-protocol-version"), Some("2025-06-18"));
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
    assert!(o.stdout.contains("\"name\": \"add\""));
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
    assert!(o.stdout.contains("## local  echo-server 0.0.1  (2 tools)"));
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
