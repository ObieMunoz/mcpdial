mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode};
use serde_json::{json, Value};
use std::io::Write;
use std::process::Stdio;

fn file(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[test]
fn add_checks_the_location_and_keeps_what_is_already_saved() {
    let s = start(Mode::Stateless);
    let home = temp_home("add");
    let servers = home.join("servers.json");

    // A location that could never be dialed is a usage error, and nothing is written.
    for (flag, bad) in [
        ("--http", "notaurl"),
        ("--http", "ftp://u/mcp"),
        ("--http", "http://"),
        ("--stdio", "   "),
    ] {
        let before = file(&servers);
        let o = run(mcpdial(&home).args(["add", "z", flag, bad]));
        assert_eq!(o.code, 2, "{flag} {bad}: {}", o.stderr);
        assert!(o.stderr.starts_with("error: "), "{}", o.stderr);
        assert_eq!(file(&servers), before, "{flag} {bad} was saved");
    }
    let o = run(mcpdial(&home).args(["--json", "add", "z", "--http", "notaurl"]));
    assert_eq!(o.code, 2);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
    assert!(
        e["error"]["message"].as_str().unwrap().contains("notaurl"),
        "{}",
        o.stderr
    );

    // A name already in use keeps its server unless --force says otherwise.
    let o = run(mcpdial(&home).args(["add", "x", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let probes = home.join("probes.json");
    assert!(
        file(&probes).is_some_and(|p| p.contains("\"x\"")),
        "add remembers its probe"
    );
    let before = file(&servers).unwrap();
    let o = run(mcpdial(&home).args(["add", "x", "--http", "https://u2/mcp"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        format!(
            "error: x is already saved (http {}); pass --force to replace it",
            s.url
        )
    );
    assert_eq!(file(&servers).unwrap(), before);

    let o = run(mcpdial(&home).args([
        "add",
        "x",
        "--http",
        "https://u2/mcp",
        "--force",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("saved x (http https://u2/mcp)"),
        "{}",
        o.stderr
    );
    let saved: Value = serde_json::from_str(&file(&servers).unwrap()).unwrap();
    assert_eq!(saved["servers"]["x"]["http"], "https://u2/mcp");
    assert!(
        !file(&probes).unwrap().contains("\"x\""),
        "--force drops the remembered probe"
    );
}

#[test]
fn add_dials_the_server_and_prints_the_row_ls_would() {
    let home = temp_home("add-probe");
    let echo = echo_command();

    let o = run(mcpdial(&home).args(["add", "echo", "--stdio", &echo]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("saved echo (stdio"), "{}", o.stderr);
    assert!(
        o.stdout.lines().next().unwrap().starts_with("NAME"),
        "{}",
        o.stdout
    );
    let row = o
        .stdout
        .lines()
        .find(|l| l.starts_with("echo"))
        .unwrap_or_else(|| panic!("no row for echo in {}", o.stdout));
    assert!(row.contains("connected"), "{row}");
    assert!(row.contains("echo-server 0.0.1"), "{row}");
    assert!(row.contains("now"), "{row}");

    // The same row, remembered: ls need not dial again.
    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "connected");

    // --json puts the listing row inside the receipt.
    let o = run(mcpdial(&home).args(["--json", "add", "echo2", "--stdio", &echo]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["saved"]["name"], "echo2");
    assert_eq!(v["saved"]["kind"], "stdio");
    assert_eq!(v["saved"]["location"], echo);
    assert_eq!(v["saved"]["status"]["state"], "connected");
    assert_eq!(v["saved"]["server"], "echo-server 0.0.1");
    assert_eq!(v["saved"]["tools"], 4);

    // --no-probe saves without dialing: the receipt is the config alone.
    let o = run(mcpdial(&home).args(["--json", "add", "echo3", "--stdio", &echo, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(
        v,
        json!({"saved": {"name": "echo3", "kind": "stdio", "location": echo}})
    );
    let o = run(mcpdial(&home).args(["add", "echo4", "--stdio", &echo, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.is_empty(), "{}", o.stdout);

    // A server that does not answer is still saved; the row says so.
    let o = run(mcpdial(&home).args(["add", "dead", "--http", "http://127.0.0.1:1/mcp"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let row = o.stdout.lines().find(|l| l.starts_with("dead")).unwrap();
    assert!(row.contains("unreachable"), "{row}");
}

#[test]
fn a_failed_result_is_marked_in_human_mode_and_left_alone_in_json() {
    let home = temp_home("call-marker");
    let echo = echo_command();
    assert_eq!(
        run(mcpdial(&home).args(["add", "echo", "--stdio", &echo, "--no-probe"])).code,
        0
    );

    let o = run(mcpdial(&home).args(["call", "echo", "fail"]));
    assert_eq!(o.code, 1);
    assert_eq!(o.stdout.trim(), "it failed");
    assert_eq!(o.stderr.trim(), "(tool reported an error)");

    // A schema complaint in a failed result gets the marker and then the usage.
    let o = run(mcpdial(&home).args(["call", "echo", "strict", "{}"]));
    assert_eq!(o.code, 1);
    let mut lines = o.stderr.lines();
    assert_eq!(lines.next(), Some("(tool reported an error)"));
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with("usage: mcpdial call echo strict"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "call", "echo", "fail"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["isError"], true);
    assert_eq!(v["content"][0]["text"], "it failed");

    // The shell prints the same marker.
    let mut child = mcpdial(&home)
        .args(["shell", "echo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call fail\ncall echo {\"message\":\"fine\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        ["it failed", "Echo: fine"]
    );
    assert_eq!(stderr.trim(), "(tool reported an error)");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a failed result is not a failed command"
    );
}
