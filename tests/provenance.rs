//! Where a saved server came from: `add --registry` records the entry's name and
//! version under `source`, and `ls --no-probe` shows it.

mod common;

use common::{mcpdial, run, start, temp_home, Mode};
use serde_json::Value;

fn saved(home: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap()
}

#[test]
fn add_from_the_registry_records_its_source() {
    let s = start(Mode::Stateless);
    let home = temp_home("provenance");
    let at_registry = || {
        let mut c = mcpdial(&home);
        c.env("MCPDIAL_REGISTRY", &s.base);
        c
    };

    let o = run(at_registry().args(["add", "web", "--registry", "io.github.acme/remote"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(at_registry().args([
        "add",
        "fs",
        "--registry",
        "io.github.acme/files",
        "--arg",
        "/srv",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["add", "plain", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let file = saved(&home);
    assert_eq!(
        file["servers"]["web"]["source"],
        serde_json::json!({"registry": "io.github.acme/remote", "version": "2.0.0"})
    );
    assert_eq!(
        file["servers"]["fs"]["source"],
        serde_json::json!({"registry": "io.github.acme/files", "version": "1.2.0"})
    );
    assert!(
        file["servers"]["plain"].get("source").is_none(),
        "a server added by hand has no source: {file}"
    );

    // The registry name shows in a SOURCE column, and `--json` carries the object.
    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert!(
        lines[0].split_whitespace().collect::<Vec<_>>()
            == ["NAME", "TYPE", "AUTH", "SOURCE", "LOCATION"],
        "{}",
        o.stdout
    );
    let row = |name: &str| {
        lines
            .iter()
            .find(|l| l.starts_with(name))
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .unwrap_or_else(|| panic!("no row for {name}: {}", o.stdout))
    };
    assert_eq!(row("web")[3], "io.github.acme/remote");
    assert_eq!(row("fs")[3], "io.github.acme/files");
    assert_eq!(row("plain")[3], "-");

    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let by_name = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap().clone();
    assert_eq!(
        by_name("web")["source"]["registry"],
        "io.github.acme/remote"
    );
    assert_eq!(by_name("web")["source"]["version"], "2.0.0");
    assert_eq!(by_name("plain")["source"], Value::Null);
}

#[test]
fn the_source_column_waits_until_a_server_has_one() {
    let home = temp_home("provenance-plain");
    let o = run(mcpdial(&home).args(["add", "a", "--http", "https://a.example/mcp"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["NAME", "TYPE", "AUTH", "LOCATION"],
        "{}",
        o.stdout
    );
}
