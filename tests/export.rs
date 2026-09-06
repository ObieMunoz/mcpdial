//! `export` writes saved servers in a host's shape: a quoted stdio argument comes
//! out as one `args` element and survives `import`, `--merge` keeps what else the
//! host's file holds, a saved token is left out and named, and a stranger is exit 2.

mod common;

use common::{echo_command, mcpdial, run, temp_home};
use serde_json::{json, Value};

#[test]
fn a_quoted_argument_becomes_one_args_element_and_survives_import() {
    let home = temp_home("export-args");
    let stdio = format!("{} --name 'a b' plain", echo_command());
    let o = run(mcpdial(&home).args([
        "add",
        "fs",
        "--stdio",
        &stdio,
        "--env",
        "DEBUG=1",
        "--timeout",
        "90",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["export"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    let fs = &doc["mcpServers"]["fs"];
    assert_eq!(fs["args"], json!(["--name", "a b", "plain"]));
    assert_eq!(fs["env"], json!({"DEBUG": "1"}));
    assert_eq!(fs["timeout"], json!(90));
    assert!(fs.get("type").is_none(), "{fs}");

    let file = home.join("exported.json");
    std::fs::write(&file, &o.stdout).unwrap();
    let o = run(mcpdial(&home).args(["rm", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["import", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let again = run(mcpdial(&home).args(["--json", "export", "fs"]));
    assert_eq!(again.code, 0, "{}", again.stderr);
    let redone: Value = serde_json::from_str(&again.stdout).unwrap();
    assert_eq!(redone, doc, "export, import, export again is a fixed point");

    // The re-imported command line still dials: the quoting was kept, not just
    // the words.
    let o = run(mcpdial(&home).args(["--json", "tools", "fs"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

#[test]
fn merge_keeps_an_unrelated_key_and_replaces_the_entry() {
    let home = temp_home("export-merge");
    let o = run(mcpdial(&home).args([
        "add",
        "wiki",
        "--http",
        "https://mcp.example.com/mcp",
        "-H",
        "X-Tenant: acme",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let file = home.join("mcp.json");
    std::fs::write(
        &file,
        json!({
            "inputs": [{"id": "t", "type": "promptString"}],
            "servers": {
                "keep": {"type": "stdio", "command": "old"},
                "wiki": {"type": "sse", "url": "https://stale/sse"}
            }
        })
        .to_string(),
    )
    .unwrap();
    let before = std::fs::read_to_string(&file).unwrap();

    let o = run(mcpdial(&home).args([
        "export",
        "--format",
        "vscode",
        "--merge",
        file.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(doc["inputs"], json!([{"id": "t", "type": "promptString"}]));
    assert_eq!(
        doc["servers"]["keep"],
        json!({"type": "stdio", "command": "old"})
    );
    assert_eq!(
        doc["servers"]["wiki"],
        json!({"type": "http", "url": "https://mcp.example.com/mcp", "headers": {"X-Tenant": "acme"}})
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "the file itself is never written"
    );

    let o = run(mcpdial(&home).args([
        "export",
        "--merge",
        home.join("missing.json").to_str().unwrap(),
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("missing.json"), "{}", o.stderr);
}

#[test]
fn an_oauth_server_is_exported_without_its_token_and_named() {
    let home = temp_home("export-token");
    let o = run(mcpdial(&home).args([
        "add",
        "wiki",
        "--http",
        "https://mcp.example.com/mcp",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home)
        .args(["token", "set", "wiki", "--env", "T"])
        .env("T", "sekrit-token"));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["export", "wiki"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        "wiki: exported without its saved token; the host will need its own login"
    );
    assert!(!o.stdout.contains("sekrit"), "{}", o.stdout);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        doc,
        json!({"mcpServers": {"wiki": {"type": "http", "url": "https://mcp.example.com/mcp"}}})
    );

    let o = run(mcpdial(&home).args(["--json", "export", "wiki"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let note: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(
        note,
        json!({"hint": "wiki: exported without its saved token; the host will need its own login"})
    );

    // A token_env is the one credential a host can hold, as its own variable.
    let o = run(mcpdial(&home).args([
        "add",
        "api",
        "--http",
        "https://api.example.com/mcp",
        "--token-env",
        "API_TOKEN",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["export", "api", "--format", "codex"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "[mcp_servers.api]\nurl = \"https://api.example.com/mcp\"\nbearer_token_env_var = \"API_TOKEN\"\n"
    );
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["export", "api"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        doc["mcpServers"]["api"]["headers"]["Authorization"],
        "Bearer ${API_TOKEN}"
    );
    assert!(
        o.stderr
            .starts_with("api: token_env API_TOKEN written as the header"),
        "{}",
        o.stderr
    );
}

#[test]
fn a_name_that_is_not_saved_is_a_usage_error() {
    let home = temp_home("export-stranger");
    let o = run(mcpdial(&home).args(["export", "nope"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("no server named \"nope\""),
        "{}",
        o.stderr
    );
    assert!(o.stdout.is_empty(), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["export", "--format", "yaml"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("mcpservers"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["export"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap(),
        json!({"mcpServers": {}})
    );
}
