//! `serve`: the saved server on one side, a client that must not see its token on
//! the other, and the proof that nothing crosses the middle.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The token the upstream server accepts, which only the proxy ever holds.
const UPSTREAM: &str = "tok-upstream";
/// The token the proxy's own clients present, which the upstream never sees.
const SANDBOX: &str = "tok-sandbox";

struct Serving {
    child: Child,
    url: String,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `mcpdial serve` and read the port it chose off its `--json` receipt.
fn serve(home: &Path, args: &[&str]) -> Serving {
    let mut child = mcpdial(home)
        .env("SANDBOX_TOKEN", SANDBOX)
        .args(["--json", "serve"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcpdial serve");

    // On its own thread, so a proxy that never gets as far as the receipt is a
    // failed assertion rather than a hung test run.
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = stdout.read_line(&mut line);
        let _ = tx.send(line);
    });

    let receipt = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(line) if !line.trim().is_empty() => line,
        _ => {
            let _ = child.kill();
            let mut why = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut why);
            panic!("serve printed no receipt:\n{why}");
        }
    };
    let receipt: Value = serde_json::from_str(&receipt).expect("the receipt is one JSON object");
    Serving {
        child,
        url: receipt["serve"]["url"]
            .as_str()
            .expect("the receipt names a URL")
            .to_string(),
    }
}

/// A saved HTTP server whose token is in the store, exactly as `login` leaves it.
fn logged_in(tag: &str, url: &str) -> std::path::PathBuf {
    let home = temp_home(tag);
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", url, "--no-probe"])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).env("UPSTREAM_TOKEN", UPSTREAM).args([
            "token",
            "set",
            "work",
            "--env",
            "UPSTREAM_TOKEN"
        ]))
        .code,
        0
    );
    home
}

fn client(home: &Path) -> Command {
    let mut c = mcpdial(home);
    c.env("SANDBOX_TOKEN", SANDBOX);
    c
}

#[test]
fn the_client_gets_the_server_and_never_its_token() {
    let s = start(Mode::Auth {
        tokens: vec![UPSTREAM.to_string()],
    });
    let home = logged_in("serve-http", &s.url);
    let proxy = serve(
        &home,
        &[
            "work",
            "--listen",
            "127.0.0.1:0",
            "--bearer-env",
            "SANDBOX_TOKEN",
            "--deny",
            "ad*",
        ],
    );
    // A client of the proxy: its own home, so nothing of work's is within reach.
    let sandbox = temp_home("serve-sandbox");

    let o =
        run(client(&sandbox).args(["--token-env", "SANDBOX_TOKEN", "--json", "info", &proxy.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let info: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        info["serverInfo"]["name"], "mcpdial",
        "the proxy answers initialize with its own identity: {}",
        o.stdout
    );
    assert_eq!(info["protocolVersion"], "2025-06-18", "{}", o.stdout);

    let o = run(client(&sandbox).args([
        "--token-env",
        "SANDBOX_TOKEN",
        "call",
        &proxy.url,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");

    // The whole point: the upstream saw the credential the human logged in with,
    // and no request carried the one the client presented.
    let requests = s.requests.lock().unwrap();
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert!(!posts.is_empty(), "the upstream was never dialed");
    for r in &posts {
        assert_eq!(
            r.header("authorization"),
            Some(format!("Bearer {UPSTREAM}").as_str()),
            "upstream request without the saved token: {r:?}"
        );
    }
    for r in requests.iter() {
        let seen = format!("{:?} {}", r.headers, r.body);
        assert!(
            !seen.contains(SANDBOX),
            "the client's token reached the upstream server: {seen}"
        );
    }
    drop(requests);

    // A denied tool is hidden from the listing and refused if asked for anyway.
    let o = run(client(&sandbox).args([
        "--token-env",
        "SANDBOX_TOKEN",
        "--json",
        "tools",
        &proxy.url,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let tools: Value = serde_json::from_str(&o.stdout).unwrap();
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo"], "{}", o.stdout);

    let o = run(client(&sandbox).args([
        "--token-env",
        "SANDBOX_TOKEN",
        "--json",
        "call",
        &proxy.url,
        "add",
        r#"{"a":1,"b":2}"#,
    ]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    let e: Value = serde_json::from_str(o.stderr.lines().next().unwrap()).unwrap();
    assert_eq!(e["error"]["code"], -32602, "{}", o.stderr);
    assert!(e["error"]["message"].as_str().unwrap().contains("add"));
}

#[test]
fn a_client_without_the_shared_token_gets_nothing() {
    let s = start(Mode::Auth {
        tokens: vec![UPSTREAM.to_string()],
    });
    let home = logged_in("serve-401", &s.url);
    let proxy = serve(
        &home,
        &[
            "work",
            "--bearer-env",
            "SANDBOX_TOKEN",
            "--listen",
            "127.0.0.1:0",
        ],
    );
    let sandbox = temp_home("serve-401-client");

    let o = run(mcpdial(&sandbox).args(["--json", "info", &proxy.url]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    let e: Value = serde_json::from_str(o.stderr.lines().next().unwrap()).unwrap();
    assert_eq!(e["error"]["kind"], "http", "{}", o.stderr);
    assert_eq!(e["error"]["status"], 401, "{}", o.stderr);

    let o = run(mcpdial(&sandbox).env("WRONG", "not-the-token").args([
        "--token-env",
        "WRONG",
        "--json",
        "info",
        &proxy.url,
    ]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.stderr.contains("401"), "{}", o.stderr);

    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a refused client must not reach the upstream server at all"
    );
}

#[test]
fn stdio_mode_is_a_server_another_mcpdial_can_dial() {
    let s = start(Mode::Auth {
        tokens: vec![UPSTREAM.to_string()],
    });
    let home = logged_in("serve-stdio", &s.url);
    let target = format!(
        "stdio:'{}' serve work --stdio --deny 'add'",
        env!("CARGO_BIN_EXE_mcpdial")
    );

    let o = run(mcpdial(&home).args(["--json", "info", &target]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let info: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(info["serverInfo"]["name"], "mcpdial", "{}", o.stdout);

    let o = run(mcpdial(&home).args(["call", &target, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");

    let o = run(mcpdial(&home).args(["--json", "tools", &target]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let tools: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(tools["tools"].as_array().unwrap().len(), 1, "{}", o.stdout);

    for r in s.requests.lock().unwrap().iter() {
        assert_eq!(
            r.header("authorization"),
            Some(format!("Bearer {UPSTREAM}").as_str()),
            "upstream request without the saved token: {r:?}"
        );
    }
}

/// A proxy is one more way to reach a server, so what its own lists refuse stays
/// refused: `serve` filters by both, not just by its flags.
#[test]
fn the_servers_own_deny_list_holds_through_the_proxy() {
    let home = temp_home("serve-saved-deny");
    assert_eq!(
        run(mcpdial(&home).args([
            "add",
            "echo",
            "--stdio",
            &echo_command(),
            "--deny",
            "shot",
            "--no-probe"
        ]))
        .code,
        0
    );
    let target = format!(
        "stdio:'{}' serve echo --stdio --deny 'strict'",
        env!("CARGO_BIN_EXE_mcpdial")
    );

    let o = run(mcpdial(&home).args(["--json", "tools", &target]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let tools: Value = serde_json::from_str(&o.stdout).unwrap();
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo", "fail", "count"], "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "call", &target, "shot", "{}"]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    let e: Value = serde_json::from_str(o.stderr.lines().next().unwrap()).unwrap();
    assert_eq!(e["error"]["code"], -32602, "{}", o.stderr);
}

/// An interrupt is the way this command ends, so it ends successfully: whatever
/// upstream sessions are open are terminated on the way out, and the exit code is
/// 0 rather than a signal.
#[cfg(unix)]
#[test]
fn an_interrupt_ends_the_upstream_session_and_exits_zero() {
    let s = start(Mode::Stateful);
    let home = temp_home("serve-interrupt");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url, "--no-probe"])).code,
        0
    );
    let mut proxy = serve(&home, &["work", "--listen", "127.0.0.1:0"]);

    // A client that initializes and walks away, so a session is still open when
    // the interrupt arrives.
    let initialize = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "walks-away", "version": "0"}}});
    let resp = ureq::post(&proxy.url)
        .header("Content-Type", "application/json")
        .send(&initialize.to_string())
        .expect("the proxy answers initialize");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().get("mcp-session-id").is_some());
    let deletes = |server: &common::FakeServer| {
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "DELETE")
            .count()
    };
    assert_eq!(deletes(&s), 0, "the session is open");

    let signalled = Command::new("kill")
        .args(["-INT", &proxy.child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signalled.success());

    let ended = proxy.child.wait().expect("serve exits");
    assert_eq!(ended.code(), Some(0), "{ended:?}");
    assert_eq!(deletes(&s), 1, "the upstream session was left open");
}

#[test]
fn what_serve_refuses_before_it_binds_anything() {
    let home = temp_home("serve-usage");

    let o = run(mcpdial(&home).args(["serve", "nobody"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("unknown server"), "{}", o.stderr);

    assert_eq!(
        run(mcpdial(&home).args([
            "add",
            "work",
            "--http",
            "https://example.com/mcp",
            "--no-probe"
        ]))
        .code,
        0
    );

    let o = run(mcpdial(&home).args(["serve", "work", "--listen", "0.0.0.0:0"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("--listen-any"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["serve", "work", "--bearer-env", "NOT_SET_ANYWHERE"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("NOT_SET_ANYWHERE"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["serve", "work", "--stdio", "--listen", "127.0.0.1:8321"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
}
