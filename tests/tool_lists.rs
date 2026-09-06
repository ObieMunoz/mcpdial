//! Per-server tool allow and deny lists: what `tools`, `ls`, `call`, `schema` and
//! the shell see once a saved server carries them, and how `set` changes them.

mod common;

use common::{mcpdial, run, start, temp_home, Mode};
use serde_json::{json, Value};
use std::process::Stdio;

fn saved(home: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap()
}

fn sent(s: &common::FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r.json()["method"].as_str().map(str::to_string))
        .collect()
}

fn names(tools: &Value) -> Vec<String> {
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn shell(home: &std::path::Path, args: &[&str], script: &str) -> (String, String, i32) {
    let mut child = mcpdial(home)
        .args(args)
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
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn tools_hides_a_denied_tool_and_all_shows_it_marked() {
    let s = start(Mode::Stateless);
    let home = temp_home("deny-tools");

    let o = run(mcpdial(&home).args([
        "--json",
        "add",
        "fs",
        "--http",
        &s.url,
        "--deny",
        "a?d",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(receipt["saved"]["deny"], json!(["a?d"]));
    assert!(
        receipt["saved"].get("allow").is_none(),
        "an empty list is left out of the receipt: {}",
        o.stdout
    );
    assert_eq!(saved(&home)["servers"]["fs"]["deny"], json!(["a?d"]));

    let o = run(mcpdial(&home).args(["tools", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("1 tool(s):"), "{}", o.stdout);
    assert!(o.stdout.contains("echo"), "{}", o.stdout);
    assert!(!o.stdout.contains("add"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "tools", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(names(&serde_json::from_str(&o.stdout).unwrap()), ["echo"]);

    let o = run(mcpdial(&home).args(["tools", "fs", "--all"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);
    assert!(o.stdout.contains("add (denied)"), "{}", o.stdout);
    assert!(!o.stdout.contains("echo (denied)"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "tools", "fs", "--all", "--long"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let all: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(names(&all), ["echo", "add"]);
    assert_eq!(all["tools"][1]["denied"], true);
    assert!(
        all["tools"][0].get("denied").is_none(),
        "a permitted tool is the server's object, untouched: {}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "tools", "fs", "--all"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let short: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        short["tools"],
        json!([
            {"name": "echo", "description": "Echo a message back."},
            {"name": "add", "denied": true, "description": "Add two numbers."},
        ]),
        "a short listing still marks what the lists hide: {}",
        o.stdout
    );

    // `--all` needs a server whose lists there are to look past.
    let o = run(mcpdial(&home).args(["tools", "--all"]));
    assert_eq!(o.code, 2, "{}", o.stderr);

    // `ls` and the all-server listing count what is permitted.
    let o = run(mcpdial(&home).args(["--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["tools"], 1, "{}", o.stdout);
    let o = run(mcpdial(&home).args(["tools"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("(1 tools)"), "{}", o.stdout);
    assert!(!o.stdout.contains("add"), "{}", o.stdout);

    // An ad-hoc target has no config to carry a list.
    let o = run(mcpdial(&home).args(["--json", "tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        names(&serde_json::from_str(&o.stdout).unwrap()),
        ["echo", "add"]
    );

    // The configuration listing carries both lists as they are saved.
    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["deny"], json!(["a?d"]));
    assert_eq!(rows[0]["allow"], json!([]));
}

#[test]
fn a_denied_call_is_refused_before_anything_is_sent() {
    let s = start(Mode::Stateless);
    let home = temp_home("deny-call");
    let o = run(mcpdial(&home).args(["add", "fs", "--http", &s.url, "--deny", "a*", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("deny: a*"), "{}", o.stderr);
    assert!(sent(&s).is_empty(), "--no-probe dials nothing");

    let o = run(mcpdial(&home).args(["call", "fs", "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        "error: add is denied for fs by its deny list; edit with mcpdial set fs"
    );
    assert!(o.stdout.is_empty(), "{}", o.stdout);
    assert!(sent(&s).is_empty(), "nothing was sent: {:?}", sent(&s));

    let o = run(mcpdial(&home).args(["--json", "call", "fs", "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "config");
    assert_eq!(err["error"]["tool"], "add");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("denied for fs by its deny list"),
        "{}",
        o.stderr
    );
    assert!(sent(&s).is_empty());

    // `schema` of a hidden tool is the same refusal, not a not-found from the server.
    let o = run(mcpdial(&home).args(["--json", "schema", "fs", "add"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "config");
    assert_eq!(err["error"]["tool"], "add");
    assert!(sent(&s).is_empty());

    // What the lists permit goes through as before.
    let o = run(mcpdial(&home).args(["call", "fs", "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");

    // `raw` is the escape hatch: it sends whatever it is given.
    let o = run(mcpdial(&home).args([
        "--json",
        "raw",
        "fs",
        "tools/call",
        r#"{"name":"add","arguments":{"a":1,"b":2}}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(result["structuredContent"]["sum"], 3.0);

    // A tool the allow list leaves out is refused by name of that list.
    let o = run(mcpdial(&home).args(["set", "fs", "--allow", "echo", "--clear-deny"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", "fs", "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        "error: add is denied for fs by its allow list; edit with mcpdial set fs"
    );
    // Deny wins over allow.
    let o = run(mcpdial(&home).args(["set", "fs", "--allow", "*", "--deny", "echo"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", "fs", "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("by its deny list"), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", "fs", "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

#[test]
fn set_shows_and_changes_the_lists_and_clear_deny_restores_a_tool() {
    let s = start(Mode::Stateless);
    let home = temp_home("set-lists");
    let o = run(mcpdial(&home).args([
        "add",
        "fs",
        "--http",
        &s.url,
        "--allow",
        "e*",
        "--allow",
        "add",
        "--deny",
        "add",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("allow: e*, add"), "{}", o.stderr);
    assert!(o.stderr.contains("deny: add"), "{}", o.stderr);

    // No flags: show.
    let o = run(mcpdial(&home).args(["set", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        ["fs", "  allow: e*, add", "  deny:  add"],
        "{}",
        o.stdout
    );
    let o = run(mcpdial(&home).args(["--json", "set", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let shown: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        shown,
        json!({"name": "fs", "allow": ["e*", "add"], "deny": ["add"]})
    );
    let o = run(mcpdial(&home).args(["tools", "fs"]));
    assert!(o.stdout.starts_with("1 tool(s):"), "{}", o.stdout);

    // Dropping the deny list brings the tool back; the allow list still admits it.
    let o = run(mcpdial(&home).args(["--json", "set", "fs", "--clear-deny"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        receipt,
        json!({"saved": {"name": "fs", "kind": "http", "location": s.url,
                         "allow": ["e*", "add"], "deny": []}})
    );
    let file = saved(&home);
    assert!(
        file["servers"]["fs"].get("deny").is_none(),
        "an emptied list is left out of the file: {file}"
    );
    let o = run(mcpdial(&home).args(["tools", "fs"]));
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);
    let o = run(mcpdial(&home).args(["call", "fs", "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("The sum of 1 and 2 is 3."),
        "{}",
        o.stdout
    );

    // `--allow` replaces the whole list rather than adding to it.
    let o = run(mcpdial(&home).args(["set", "fs", "--allow", "echo"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        format!("saved fs (http {})\n  allow: echo", s.url)
    );
    assert_eq!(saved(&home)["servers"]["fs"]["allow"], json!(["echo"]));
    let o = run(mcpdial(&home).args(["set", "fs", "--clear-allow"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["set", "fs"]));
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        ["fs", "  allow: (every tool not denied)", "  deny:  (none)"],
        "{}",
        o.stdout
    );
    assert_eq!(
        saved(&home)["servers"]["fs"],
        json!({"http": s.url}),
        "back to a plain entry"
    );

    // Mistakes are usage errors and nothing is written.
    let o = run(mcpdial(&home).args(["set", "nope", "--deny", "x"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("no server named \"nope\""),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["set", "fs", "--deny", " "]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("--deny needs a tool name"),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["set", "fs", "--allow", "x", "--clear-allow"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let o = run(mcpdial(&home).args([
        "add",
        "other",
        "--http",
        &s.url,
        "--allow",
        "",
        "--no-probe",
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("--allow needs a tool name"),
        "{}",
        o.stderr
    );
    assert_eq!(saved(&home)["servers"]["fs"], json!({"http": s.url}));
    assert!(saved(&home)["servers"].get("other").is_none());
}

#[test]
fn import_leaves_both_lists_empty() {
    let s = start(Mode::Stateless);
    let home = temp_home("import-lists");
    let cfg = home.join("hostconfig.json");
    std::fs::write(
        &cfg,
        json!({"mcpServers": {
            "web": {"type": "http", "url": s.url},
            "local": {"command": "npx", "args": ["-y", "pkg"]}
        }})
        .to_string(),
    )
    .unwrap();
    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let file = saved(&home);
    for name in ["web", "local"] {
        assert!(file["servers"][name].get("allow").is_none(), "{file}");
        assert!(file["servers"][name].get("deny").is_none(), "{file}");
    }
    let o = run(mcpdial(&home).args(["--json", "set", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap(),
        json!({"name": "web", "allow": [], "deny": []})
    );
    let o = run(mcpdial(&home).args(["--json", "tools", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        names(&serde_json::from_str(&o.stdout).unwrap()),
        ["echo", "add"]
    );
}

#[test]
fn the_shell_sees_only_what_is_permitted_and_raw_sees_everything() {
    let s = start(Mode::Stateless);
    let home = temp_home("shell-lists");
    let o =
        run(mcpdial(&home).args(["add", "fs", "--http", &s.url, "--deny", "add", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let script = concat!(
        "tools\n",
        "call add {\"a\":1,\"b\":2}\n",
        "help add\n",
        "schema add\n",
        "call ad {\"a\":1,\"b\":2}\n",
        "raw tools/call {\"name\":\"add\",\"arguments\":{\"a\":1,\"b\":2}}\n",
        "call echo {\"message\":\"hi\"}\n",
        "quit\n",
    );
    let (stdout, stderr, _) = shell(&home, &["shell", "fs"], script);
    assert!(stdout.starts_with("1 tool(s):"), "{stdout}");
    assert!(
        stderr.contains("error: add is denied for fs by its deny list; edit with mcpdial set fs"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("add is denied for fs").count(),
        3,
        "call, help and schema all refuse: {stderr}"
    );
    assert!(
        stderr.contains("did you mean echo?") || stderr.contains("lists all 1"),
        "a near miss is not steered toward the hidden tool: {stderr}"
    );
    assert!(!stderr.contains("did you mean add"), "{stderr}");
    assert!(stdout.contains("\"sum\": 3"), "raw is unfiltered: {stdout}");
    assert!(stdout.contains("Echo: hi"), "{stdout}");
    let calls: Vec<String> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "tools/call")
        .map(|m| m["params"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        calls,
        ["ad", "add", "echo"],
        "the typo is the server's to refuse; add reached it through raw alone"
    );

    let (stdout, _, _) = shell(
        &home,
        &["--json", "shell", "fs"],
        "call add {\"a\":1,\"b\":2}\nquit\n",
    );
    let line: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(line["error"]["kind"], "config");
    assert_eq!(line["error"]["tool"], "add");
}
