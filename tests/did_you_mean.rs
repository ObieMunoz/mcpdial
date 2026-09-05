//! A wrong tool name gets the right one named, whichever way the server said no.

mod common;

use common::{echo_command, mcpdial, run, temp_home};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

fn echo_target() -> String {
    format!("stdio:{}", echo_command())
}

/// The echo server with its unknown-tool answer switched from a `-32602` error
/// to a failed result, the shape DeepWiki and its kind send.
fn deepwiki_style(home: &std::path::Path) -> Command {
    let mut c = mcpdial(home);
    c.env("ECHO_SERVER_UNKNOWN_TOOL_RESULT", "1");
    c
}

/// Every `{"hint": ...}` line on stderr, which is where a hint goes under `--json`
/// when the result itself is on stdout.
fn hint_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["hint"].as_str().map(String::from))
        .collect()
}

#[test]
fn a_transposed_tool_name_is_still_named() {
    let home = temp_home("dym-transposed");
    let target = echo_target();

    let o = run(mcpdial(&home).args(["call", &target, "ecoh", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("MCP error -32602: Tool ecoh not found"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("did you mean echo?"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["--json", "call", &target, "ecoh", "{}"]));
    assert_eq!(o.code, 1);
    let err: Value = serde_json::from_str(o.stderr.trim()).unwrap_or_else(|e| {
        panic!("stderr is one JSON object: {e}: {}", o.stderr);
    });
    assert_eq!(err["error"]["code"], -32602);
    assert!(
        err["error"]["hint"]
            .as_str()
            .unwrap_or("")
            .starts_with("did you mean echo?"),
        "{}",
        o.stderr
    );
}

#[test]
fn an_unknown_tool_reported_as_a_failed_result_still_gets_a_suggestion() {
    let home = temp_home("dym-result");
    let target = echo_target();

    let o = run(deepwiki_style(&home).args(["call", &target, "ecoh", "{}"]));
    assert_eq!(o.code, 1, "an isError result exits 1");
    assert_eq!(o.stdout.trim(), "Unknown tool: ecoh");
    assert!(
        o.stderr.contains("(tool reported an error)"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("did you mean echo?"), "{}", o.stderr);

    // Nothing close by: the hint still says where the real names are.
    let o = run(deepwiki_style(&home).args(["call", &target, "read_wiki_struct", "{}"]));
    assert_eq!(o.code, 1);
    assert_eq!(o.stdout.trim(), "Unknown tool: read_wiki_struct");
    assert!(
        o.stderr.contains("lists all 5 tools on this server"),
        "{}",
        o.stderr
    );

    // Under --json the result stays on stdout untouched and the hint is its own
    // object on stderr, as a failed result's usage hint already is.
    let o = run(deepwiki_style(&home).args(["--json", "call", &target, "ecoh", "{}"]));
    assert_eq!(o.code, 1);
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(result["isError"], true, "{}", o.stdout);
    assert_eq!(
        result["content"][0]["text"], "Unknown tool: ecoh",
        "{}",
        o.stdout
    );
    let hints = hint_lines(&o.stderr);
    assert_eq!(hints.len(), 1, "{}", o.stderr);
    assert!(hints[0].starts_with("did you mean echo?"), "{}", o.stderr);
}

#[test]
fn a_tool_that_exists_and_failed_gets_no_suggestion() {
    let home = temp_home("dym-real-failure");
    let target = echo_target();

    for cmd in [mcpdial(&home), deepwiki_style(&home)] {
        let mut cmd = cmd;
        let o = run(cmd.args(["call", &target, "fail", "{}"]));
        assert_eq!(o.code, 1);
        assert_eq!(o.stdout.trim(), "it failed");
        assert!(
            !o.stderr.contains("did you mean") && !o.stderr.contains("lists all"),
            "a tool's own failure is not a typo: {}",
            o.stderr
        );
        assert!(!o.stderr.contains("usage:"), "{}", o.stderr);
    }
}

#[test]
fn the_shell_names_the_tool_whichever_way_the_server_said_no() {
    let home = temp_home("dym-shell");
    let target = echo_target();

    let (stdout, stderr, _) = shell(
        deepwiki_style(&home),
        &target,
        false,
        "call ecoh {}\ncall fail {}\nquit\n",
    );
    assert!(stdout.contains("Unknown tool: ecoh"), "{stdout}");
    assert_eq!(stderr.matches("did you mean echo?").count(), 1, "{stderr}");
    assert!(!stderr.contains("usage:"), "{stderr}");

    // The same with a -32602 error, where the hint rides on the error object.
    let (stdout, _, _) = shell(mcpdial(&home), &target, true, "call ecoh {}\n");
    let err: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        err["error"]["hint"]
            .as_str()
            .unwrap_or("")
            .starts_with("did you mean echo?"),
        "{stdout}"
    );

    // And as a failed result under --json: the result line on stdout, the hint on stderr.
    let (stdout, stderr, _) = shell(deepwiki_style(&home), &target, true, "call ecoh {}\n");
    let result: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(result["isError"], true, "{stdout}");
    let hints = hint_lines(&stderr);
    assert_eq!(hints.len(), 1, "{stderr}");
    assert!(hints[0].starts_with("did you mean echo?"), "{stderr}");
}

/// Pipe `script` into `mcpdial shell` and collect everything it said.
fn shell(
    mut cmd: Command,
    target: &str,
    json: bool,
    script: &str,
) -> (String, String, Option<i32>) {
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
