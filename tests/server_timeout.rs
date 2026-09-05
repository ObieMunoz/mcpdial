//! A timeout saved with a server: `add --timeout` writes it, dialing honours it
//! unless the flag is given, `ls --no-probe` shows it, and `import` keeps one it
//! finds in a host's config.

mod common;

use common::{echo_command, mcpdial, run, temp_home};
use serde_json::Value;

fn servers_file(home: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap()
}

#[test]
fn a_saved_timeout_bounds_the_wait_and_the_flag_still_beats_it() {
    let home = temp_home("saved-timeout");
    let o = run(mcpdial(&home).args([
        "add",
        "hang",
        "--stdio",
        &echo_command(),
        "--env",
        "ECHO_SERVER_HANG=1",
        "--timeout",
        "0.2",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(servers_file(&home)["servers"]["hang"]["timeout"], 0.2);

    // No flag on the call: the saved 0.2 s applies, not the 60 s default.
    let o = run(mcpdial(&home).args(["tools", "hang"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("no reply after 0.2s"), "{}", o.stderr);

    // The flag on the call beats what was saved.
    let o = run(mcpdial(&home).args(["--timeout", "1", "tools", "hang"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("no reply after 1s"), "{}", o.stderr);
}

#[test]
fn add_without_the_flag_saves_no_timeout_and_refuses_a_bad_one() {
    let home = temp_home("no-timeout");
    let o = run(mcpdial(&home).args(["add", "plain", "--stdio", &echo_command(), "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(servers_file(&home)["servers"]["plain"]
        .get("timeout")
        .is_none());

    let o = run(mcpdial(&home).args([
        "--json",
        "add",
        "bad",
        "--stdio",
        &echo_command(),
        "--timeout=-1",
        "--no-probe",
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "usage");
    assert!(
        servers_file(&home)["servers"].get("bad").is_none(),
        "nothing was written"
    );
}

#[test]
fn ls_shows_the_column_only_once_a_server_has_a_timeout() {
    let home = temp_home("ls-timeout");
    let o = run(mcpdial(&home).args(["add", "plain", "--stdio", &echo_command(), "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("TIMEOUT"), "{}", o.stdout);

    let o = run(mcpdial(&home).args([
        "add",
        "slow",
        "--stdio",
        &echo_command(),
        "--timeout",
        "120",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("TIMEOUT"), "{}", o.stdout);
    let slow = o.stdout.lines().find(|l| l.starts_with("slow")).unwrap();
    assert!(slow.contains("120s"), "{slow}");
    let plain = o.stdout.lines().find(|l| l.starts_with("plain")).unwrap();
    assert!(plain.contains(" - "), "{plain}");

    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let by_name = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap().clone();
    assert_eq!(by_name("slow")["timeout"], 120.0);
    assert_eq!(by_name("plain")["timeout"], Value::Null);
}

#[test]
fn import_keeps_a_timeout_it_finds() {
    let home = temp_home("import-timeout");
    let file = home.join("host.json");
    std::fs::write(
        &file,
        r#"{"mcpServers":{
            "slow":  {"command":"npx","args":["-y","slow-server"],"timeout":120},
            "plain": {"command":"npx","args":["-y","other"]}
        }}"#,
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["--json", "import", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(receipt["imported"], serde_json::json!(["plain", "slow"]));

    let saved = servers_file(&home);
    assert_eq!(saved["servers"]["slow"]["timeout"], 120.0);
    assert!(saved["servers"]["plain"].get("timeout").is_none());
}
