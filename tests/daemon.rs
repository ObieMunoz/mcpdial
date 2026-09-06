//! `mcpdial start` keeps a stdio server alive, and everything that dials it
//! rides that one process until `stop`. The echo server's `count` tool tells a
//! shared session from a fresh one: it counts calls per process.

#![cfg(unix)]

mod common;

use common::{echo_command, mcpdial, run, Out};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A config directory short enough that `run/NAME.sock` fits a Unix socket
/// path, which macOS caps at 104 bytes.
fn short_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("md-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn add_echo(home: &Path, name: &str, extra: &[&str]) {
    let echo = echo_command();
    let mut args = vec!["add", name, "--stdio", &echo];
    args.extend_from_slice(extra);
    let o = run(mcpdial(home).args(args));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

/// Every daemon here is started with an idle limit, so one a failing test
/// leaves behind still goes away on its own.
fn start(home: &Path, name: &str) -> Out {
    run(mcpdial(home).args(["start", name, "--idle", "120"]))
}

fn count(home: &Path, name: &str, extra: &[&str]) -> Out {
    let mut args = extra.to_vec();
    args.extend_from_slice(&["call", name, "count"]);
    run(mcpdial(home).args(args))
}

fn socket(home: &Path, name: &str) -> PathBuf {
    home.join("run").join(format!("{name}.sock"))
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_started_server_is_shared_until_it_is_stopped() {
    let home = short_home("share");
    add_echo(&home, "echo", &[]);

    let o = start(&home, "echo");
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.starts_with("started echo (pid "), "{}", o.stderr);
    assert!(socket(&home, "echo").exists());

    // Two calls, one process: the count carries over.
    let o = count(&home, "echo", &[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    let o = count(&home, "echo", &[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=2");

    // Opting out dials a fresh process, and leaves the daemon's count alone.
    let o = count(&home, "echo", &["--no-daemon"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    let o = run(mcpdial(&home)
        .env("MCPDIAL_NO_DAEMON", "1")
        .args(["call", "echo", "count"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    let o = count(&home, "echo", &[]);
    assert_eq!(o.stdout.trim(), "count=3", "{}", o.stderr);

    // The listing knows, in both shapes, and so does --no-probe.
    let o = run(mcpdial(&home).args(["ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("DAEMON"), "{}", o.stdout);
    assert!(o.stdout.contains("running"), "{}", o.stdout);
    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["running"], true, "{}", o.stdout);
    assert_eq!(rows[0]["status"]["state"], "connected");
    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["running"], true, "{}", o.stdout);

    // A second start is refused rather than doubled up.
    let o = start(&home, "echo");
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("already running"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["stop", "echo"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stderr.trim(), "stopped echo");
    assert!(!socket(&home, "echo").exists(), "stop removes the socket");

    // Back to one process per call.
    let o = count(&home, "echo", &[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["running"], false, "{}", o.stdout);

    let o = run(mcpdial(&home).args(["stop", "echo"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("is not running"), "{}", o.stderr);
}

#[test]
fn a_socket_left_by_a_killed_daemon_is_cleaned_up() {
    let home = short_home("stale");
    add_echo(&home, "echo", &[]);

    let o = run(mcpdial(&home).args(["--json", "start", "echo", "--idle", "120"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let started: Value = serde_json::from_str(&o.stdout).unwrap();
    let pid = started["pid"].as_u64().unwrap();
    assert_eq!(
        Path::new(started["socket"].as_str().unwrap()),
        socket(&home, "echo")
    );
    let o = count(&home, "echo", &[]);
    assert_eq!(o.stdout.trim(), "count=1", "{}", o.stderr);

    // SIGKILL gives the daemon no chance to tidy up.
    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    wait_until("the daemon to die", || {
        !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    });
    assert!(
        socket(&home, "echo").exists(),
        "the socket file is left behind"
    );

    // The next call notices, removes it, and dials as if it had never been there.
    let o = count(&home, "echo", &[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    assert!(o.stderr.contains("stale socket"), "{}", o.stderr);
    assert!(!socket(&home, "echo").exists());

    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["running"], false, "{}", o.stdout);
}

#[test]
fn what_the_server_asks_mid_call_reaches_the_caller() {
    let home = short_home("ping");
    add_echo(&home, "pinger", &["--env", "ECHO_SERVER_PING=1"]);
    let o = start(&home, "pinger");
    assert_eq!(o.code, 0, "{}", o.stderr);

    // The server interrupts the call with a notification, a ping, and a request
    // the client refuses, and finishes only once both requests are answered. The
    // daemon has to hand them to the caller: they arrive in its trace, answered.
    let o = run(mcpdial(&home).args([
        "-v",
        "call",
        "pinger",
        "echo",
        r#"{"message":"through the daemon"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: through the daemon");
    assert!(o.stderr.contains("-> socket"), "{}", o.stderr);
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

    // A tool error and an unknown tool keep their exit codes across the relay.
    let o = run(mcpdial(&home).args(["call", "pinger", "fail"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "call", "pinger", "nope"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["code"], -32602, "{}", o.stderr);

    assert_eq!(run(mcpdial(&home).args(["stop", "pinger"])).code, 0);
}

#[test]
fn start_is_for_saved_stdio_servers_and_idle_ends_it() {
    let home = short_home("idle");
    let o = run(mcpdial(&home).args(["add", "web", "--http", "http://127.0.0.1:9/mcp"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["start", "web"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("stdio servers"), "{}", o.stderr);
    let adhoc = format!("stdio:{}", echo_command());
    let o = run(mcpdial(&home).args(["start", &adhoc]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("saved server name"), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["start", "nope"]));
    assert_eq!(o.code, 2, "{}", o.stderr);

    // A server that dies on startup fails `start` with its own post-mortem.
    let o = run(mcpdial(&home).args([
        "add",
        "dying",
        "--stdio",
        "/bin/sh -c 'echo boom >&2; exit 3'",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = start(&home, "dying");
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("exited with status 3"), "{}", o.stderr);
    assert!(o.stderr.contains("| boom"), "{}", o.stderr);
    assert!(!socket(&home, "dying").exists());

    add_echo(&home, "echo", &[]);
    let o = run(mcpdial(&home).args(["start", "echo", "--idle", "0"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["start", "echo", "--idle", "1"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(socket(&home, "echo").exists());
    wait_until("the idle daemon to exit", || {
        !socket(&home, "echo").exists()
    });
    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let echo = rows.iter().find(|r| r["name"] == "echo").unwrap();
    assert_eq!(echo["running"], false, "{}", o.stdout);
}
