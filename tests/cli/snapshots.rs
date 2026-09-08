//! `tools --snapshot` and `--check`: the drift gate, and what it refuses.

use crate::common::{mcpdial, run, start, temp_home, Mode};
use serde_json::{json, Value};

/// The tools the fake server offers, as a snapshot file holds them, plus the
/// path it was written to.
fn snapshot_of(home: &std::path::Path, url: &str, name: &str) -> (std::path::PathBuf, Value) {
    let path = home.join(name);
    let o = run(mcpdial(home).args(["tools", url, "--snapshot", path.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("wrote 2 tool(s) to"), "{}", o.stdout);
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    (path, document)
}

#[test]
fn a_snapshot_holds_the_whole_tool_and_a_check_of_it_passes() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-write");
    let (path, document) = snapshot_of(&home, &s.url, "tools.json");

    assert_eq!(
        document["server"],
        json!({"name": "fake-mcp", "version": "1.0"})
    );
    assert_eq!(document["protocolVersion"], "2025-06-18");
    let names: Vec<&str> = document["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["add", "echo"], "tools are sorted by name");
    assert_eq!(
        document["tools"][1]["inputSchema"]["properties"]["message"]["type"], "string",
        "a snapshot holds the schema a listing drops"
    );
    assert!(
        std::fs::read_to_string(&path).unwrap().ends_with("}\n"),
        "a file a repository commits ends in a newline"
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "ok");

    let o =
        run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap(), "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap(),
        json!({"ok": true, "differences": []})
    );
}

#[test]
fn a_renamed_parameter_is_exit_three_and_stops_the_call_it_would_have_broken() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-drift");
    let (_, mut document) = snapshot_of(&home, &s.url, "tools.json");

    // The snapshot a script committed back when the parameter was `text`.
    let echo = &mut document["tools"][1];
    echo["inputSchema"]["properties"] = json!({"text": {"type": "string"}});
    echo["inputSchema"]["required"] = json!(["text"]);
    let older = home.join("older.json");
    std::fs::write(&older, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let older = older.to_str().unwrap().to_string();

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older]));
    assert_eq!(o.code, 3, "drift has an exit code of its own: {}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "tool echo: required property text was removed",
            "tool echo: property message is new and required",
            "2 differences",
        ]
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older, "--json"]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    let report: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["differences"][0]["tool"], "echo");
    assert_eq!(report["differences"][0]["kind"], "property-removed");
    assert_eq!(report["differences"][0]["level"], "fail");
    assert_eq!(report["differences"][1]["kind"], "now-required");

    // The call the snapshot was written to protect never goes out.
    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "message=hi", "--check", &older]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    assert!(
        o.stdout.is_empty(),
        "stdout holds a result or nothing: {:?}",
        o.stdout
    );
    assert!(
        o.stderr
            .contains("tool echo: required property text was removed"),
        "{}",
        o.stderr
    );
    let sent = s.requests.lock().unwrap();
    assert!(
        !sent[before..]
            .iter()
            .any(|r| r.json()["method"] == "tools/call"),
        "nothing was called"
    );
    drop(sent);

    // A tool the snapshot still describes correctly is called as ever.
    let o = run(mcpdial(&home).args([
        "call",
        &s.url,
        "add",
        r#"{"a":40,"b":2}"#,
        "--check",
        &older,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 40 and 2 is 42.");
    assert!(
        o.stderr.is_empty(),
        "a check that passes says nothing: {}",
        o.stderr
    );
}

#[test]
fn a_change_that_breaks_nobody_passes_until_strict_asks_for_the_object_back() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-strict");
    let (_, mut document) = snapshot_of(&home, &s.url, "tools.json");

    // A snapshot from back when echo also took an optional `encoding`, and
    // said so in other words. A caller holding it breaks on neither.
    document["tools"][1]["description"] = json!("Echo something back.");
    document["tools"][1]["inputSchema"]["properties"]["encoding"] = json!({"type": "string"});
    let older = home.join("older.json");
    std::fs::write(&older, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let older = older.to_str().unwrap().to_string();

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "info: tool echo: optional property encoding was removed",
            "ok",
        ],
        "a compatible change is still shown"
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older, "--strict"]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "tool echo: optional property encoding was removed",
            "tool echo: description differs from the snapshot",
            "2 differences",
        ]
    );
}

#[test]
fn a_snapshot_path_that_cannot_hold_a_file_is_refused_before_the_server_is_dialed() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-refuse");
    let taken = home.join("taken.json");
    std::fs::write(&taken, "keep me").unwrap();
    let directory = home.join("adir");
    std::fs::create_dir(&directory).unwrap();

    let refused = |path: &std::path::Path, expected: &str| {
        let o = run(mcpdial(&home).args(["tools", &s.url, "--snapshot", path.to_str().unwrap()]));
        assert_eq!(o.code, 2, "{}", o.stderr);
        assert!(o.stderr.contains(expected), "{}", o.stderr);
        assert!(o.stdout.is_empty(), "{}", o.stdout);
    };
    refused(&taken, "already exists; name a path that does not");
    refused(&directory, "is a directory; name the file to write");
    refused(&home.join("nowhere/deep.json"), "is not a directory");

    assert_eq!(
        std::fs::read_to_string(&taken).unwrap(),
        "keep me",
        "a file mcpdial did not make is never written over"
    );
    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a path that could never be written costs no connection"
    );
}

/// Only a Unix test: the mode bits are what make a directory unwritable, and
/// Windows has nothing that answers to `chmod`.
#[cfg(unix)]
#[test]
fn a_directory_the_process_cannot_write_to_leaves_no_snapshot_behind() {
    use std::os::unix::fs::PermissionsExt;

    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-readonly");
    let sealed = home.join("sealed");
    std::fs::create_dir(&sealed).unwrap();
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500)).unwrap();

    // Mode bits do not restrain root, which is what a container runs the suite
    // as, so the premise is checked rather than assumed: a directory this
    // process can still write to is not the one the test is about.
    let probe = sealed.join("writable");
    if std::fs::write(&probe, "").is_ok() {
        std::fs::remove_file(&probe).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: this process writes to a directory it has no write bit on");
        return;
    }

    let path = sealed.join("tools.json");
    let o = run(mcpdial(&home).args(["tools", &s.url, "--snapshot", path.to_str().unwrap()]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("--snapshot"), "{}", o.stderr);
    assert!(!path.exists(), "nothing half-written is left behind");

    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_check_file_that_is_not_a_snapshot_is_a_usage_error_and_never_a_pass() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-unreadable");
    let not_json = home.join("not.json");
    std::fs::write(&not_json, "tools: []").unwrap();
    let wrong_shape = home.join("wrong.json");
    std::fs::write(&wrong_shape, r#"{"servers": []}"#).unwrap();

    let refused = |path: &std::path::Path, expected: &str| {
        let o = run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap()]));
        assert_eq!(
            o.code, 2,
            "not 0, and not the 3 that means drift: {}",
            o.stderr
        );
        assert!(o.stderr.contains(expected), "{}", o.stderr);
        assert!(o.stdout.is_empty(), "{}", o.stdout);
    };
    refused(&home.join("missing.json"), "--check");
    refused(&not_json, "not JSON");
    refused(&wrong_shape, "no tools array");

    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a snapshot that is not one costs no connection"
    );
}
