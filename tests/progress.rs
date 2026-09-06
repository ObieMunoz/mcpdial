//! Progress and log notifications during a call: the token that asks for them,
//! the routing that delivers them, and the rule that a pipe never sees one.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode, Out};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// The echo server with `ECHO_SERVER_PROGRESS` set: it reports `steps` times
/// during every `tools/call`, plus one report under a token nobody asked for
/// and one log message at each end of the severity scale.
fn talkative(home: &std::path::Path, steps: u32) -> Command {
    let mut cmd = mcpdial(home);
    cmd.env("ECHO_SERVER_PROGRESS", steps.to_string());
    cmd
}

fn echo_target() -> String {
    format!("stdio:{}", echo_command())
}

fn call(o: &Out) -> &str {
    o.stdout.trim()
}

fn progress_lines(o: &Out) -> Vec<&str> {
    o.stderr
        .lines()
        .filter(|l| l.starts_with("progress:"))
        .collect()
}

#[test]
fn a_progress_token_rides_only_when_somebody_is_listening() {
    let s = start(Mode::Logging);
    let home = temp_home("progress-token");

    let quiet = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(quiet.code, 0, "{}", quiet.stderr);
    let sent = last_call(&s);
    assert!(
        sent["params"]["_meta"]["progressToken"].is_null(),
        "a piped run asks for no progress: {sent}"
    );

    let asked = run(talkative(&home, 2).args([
        "--progress",
        "call",
        &s.url,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(asked.code, 0, "{}", asked.stderr);
    let sent = last_call(&s);
    assert_eq!(
        sent["params"]["_meta"]["progressToken"], sent["id"],
        "the request's own id is the token: {sent}"
    );
}

/// The last `tools/call` the fake server was sent.
fn last_call(s: &common::FakeServer) -> Value {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .rfind(|m| m["method"] == "tools/call")
        .expect("a tools/call was sent")
}

#[test]
fn a_piped_run_sees_no_progress_and_an_asked_one_sees_every_step() {
    let home = temp_home("progress-piped");
    let target = echo_target();
    let args = ["call", &target, "echo", r#"{"message":"hi"}"#];

    let quiet = run(talkative(&home, 3).args(args));
    assert_eq!(quiet.code, 0, "{}", quiet.stderr);
    assert_eq!(call(&quiet), "Echo: hi");
    assert!(
        progress_lines(&quiet).is_empty(),
        "a pipe gets no progress: {:?}",
        quiet.stderr
    );

    let asked = run(talkative(&home, 3).args(["--progress"]).args(args));
    assert_eq!(asked.code, 0, "{}", asked.stderr);
    assert_eq!(
        asked.stdout, quiet.stdout,
        "stdout is the same byte for byte"
    );
    assert_eq!(
        progress_lines(&asked),
        [
            "progress: 1/3 step 1",
            "progress: 2/3 step 2",
            "progress: 3/3 step 3"
        ],
        "the report under a token nobody asked for is dropped: {}",
        asked.stderr
    );
}

/// A server may talk as much as it likes; what a program reads must not move.
///
/// Both runs are compared whole, so the flood is held to the same bytes on both
/// streams and the same exit code as a silent server. The one line the flood
/// does add - the server's own warning - is filtered out here by asking for the
/// top of the scale, and is what
/// `log_messages_print_from_warning_up_and_the_flag_moves_the_line` is about.
#[test]
fn a_flood_of_notifications_leaves_the_result_and_the_exit_code_alone() {
    let home = temp_home("progress-flood");
    let target = echo_target();
    let quietly = ["--log-level", "emergency"];

    for tool in ["echo", "fail"] {
        for mode in [vec![], vec!["--json"]] {
            let args = ["call", &target, tool, r#"{"message":"hi"}"#];
            let calm = run(mcpdial(&home).args(quietly).args(&mode).args(args));
            let flooded = run(talkative(&home, 500).args(quietly).args(&mode).args(args));
            assert_eq!(flooded.code, calm.code, "{tool}: {}", flooded.stderr);
            assert_eq!(flooded.stdout, calm.stdout, "{tool}: stdout moved");
            assert_eq!(flooded.stderr, calm.stderr, "{tool}: stderr moved");
        }
    }

    // And with the threshold left alone, the flood is still only the one line
    // a warning is worth; none of the five hundred reports reaches the pipe.
    let flooded = run(talkative(&home, 500).args(["call", &target, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(flooded.code, 0, "{}", flooded.stderr);
    assert_eq!(call(&flooded), "Echo: hi");
    assert!(progress_lines(&flooded).is_empty(), "{}", flooded.stderr);
    assert_eq!(flooded.stderr.lines().count(), 1, "{}", flooded.stderr);
}

#[test]
fn a_server_that_talks_during_a_call_is_still_answered_the_same_way() {
    let s = start(Mode::Logging);
    let home = temp_home("progress-http");

    let o = run(talkative(&home, 2).args([
        "--progress",
        "call",
        &s.url,
        "echo",
        r#"{"message":"over http"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(call(&o), "Echo: over http");
    assert_eq!(
        progress_lines(&o),
        ["progress: 1/2 step 1", "progress: 2/2 step 2"],
        "the stale token is dropped: {}",
        o.stderr
    );
}

#[test]
fn log_messages_print_from_warning_up_and_the_flag_moves_the_line() {
    let home = temp_home("progress-levels");
    let target = echo_target();
    let args = ["call", &target, "echo", r#"{"message":"hi"}"#];

    let quiet = run(talkative(&home, 1).args(args));
    assert!(
        quiet.stderr.contains("server [warning] echo: nearly there"),
        "a warning is worth showing: {}",
        quiet.stderr
    );
    assert!(
        !quiet.stderr.contains("starting"),
        "a debug line is not: {}",
        quiet.stderr
    );

    let loud = run(talkative(&home, 1)
        .args(["--log-level", "debug"])
        .args(args));
    assert!(
        loud.stderr.contains("server [debug] echo: starting"),
        "{}",
        loud.stderr
    );

    let silent = run(talkative(&home, 1)
        .args(["--log-level", "emergency"])
        .args(args));
    assert!(!silent.stderr.contains("nearly there"), "{}", silent.stderr);
    assert_eq!(call(&silent), "Echo: hi");

    let refused = run(talkative(&home, 1)
        .args(["--log-level", "chatty"])
        .args(args));
    assert_eq!(refused.code, 2, "an unknown level is a usage error");
}

#[test]
fn under_json_every_line_of_both_streams_is_still_an_object() {
    let home = temp_home("progress-json");
    let target = echo_target();

    let o = run(talkative(&home, 2).args([
        "--json",
        "--progress",
        "--log-level",
        "debug",
        "call",
        &target,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let result: Value = serde_json::from_str(&o.stdout).expect("stdout is the result object");
    assert_eq!(result["content"][0]["text"], "Echo: hi");

    let notifications: Vec<Value> = o
        .stderr
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?} is not an object: {e}")))
        .collect();
    let methods: Vec<&str> = notifications
        .iter()
        .map(|n| n["notification"]["method"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        methods,
        [
            "notifications/message",
            "notifications/progress",
            "notifications/progress",
            "notifications/message"
        ],
        "{}",
        o.stderr
    );
    assert_eq!(
        notifications[1]["notification"]["params"]["message"],
        "step 1"
    );
}

#[test]
fn set_level_goes_out_only_to_a_server_that_advertises_logging() {
    let advertised = start(Mode::Logging);
    let home = temp_home("progress-setlevel");
    let o = run(mcpdial(&home).args([
        "--log-level",
        "debug",
        "call",
        &advertised.url,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = methods(&advertised);
    assert!(asked.contains(&"logging/setLevel".to_string()), "{asked:?}");

    let silent = start(Mode::Stateless);
    let o = run(mcpdial(&home).args([
        "--log-level",
        "debug",
        "call",
        &silent.url,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !methods(&silent).contains(&"logging/setLevel".to_string()),
        "a server that never offered logging is not told a level"
    );

    let never = start(Mode::Logging);
    let o = run(mcpdial(&home).args(["call", &never.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !methods(&never).contains(&"logging/setLevel".to_string()),
        "and neither is one when no level was asked for"
    );
}

fn methods(s: &common::FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r.json()["method"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn the_shell_reports_per_command_and_still_prints_one_line_per_command() {
    let home = temp_home("progress-shell");
    let target = echo_target();
    let script = "call echo {\"message\":\"one\"}\ncall echo {\"message\":\"two\"}\nquit\n";

    let mut child = talkative(&home, 2)
        .args(["--json", "--progress", "shell", &target])
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
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let results: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("one object per command"))
        .collect();
    assert_eq!(results.len(), 2, "{stdout}");
    assert_eq!(results[0]["content"][0]["text"], "Echo: one");
    assert_eq!(results[1]["content"][0]["text"], "Echo: two");
    let reported = stderr
        .lines()
        .filter(|l| l.contains("notifications/progress"))
        .count();
    assert_eq!(reported, 4, "two per command: {stderr}");
}

/// The one updating line is the terminal's alone, and it is the same run that
/// proves the token goes out at a terminal without anybody asking for it.
///
/// What the line looks like belongs to the presenter; what is asserted here is
/// that the server's own words got as far as the screen, and that the result
/// still went to stdout untouched. `script` is a unix tool, so Windows skips
/// this, and without the `rich` feature a terminal is a pipe.
#[test]
fn a_terminal_is_shown_what_the_server_reports_and_a_pipe_is_not() {
    if !cfg!(unix) || !cfg!(feature = "rich") {
        eprintln!("skipped: needs `script` and the rich presenter");
        return;
    }
    let home = temp_home("progress-tty");
    let target = echo_target();
    let shown = String::from_utf8_lossy(&common::under_pty(
        &home,
        &["call", &target, "echo", r#"{"message":"hi"}"#],
        &[("ECHO_SERVER_PROGRESS", "2")],
    ))
    .into_owned();
    assert!(shown.contains("step 2"), "{shown}");
    assert!(shown.contains("Echo: hi"), "{shown}");

    let piped = run(talkative(&home, 2).args(["call", &target, "echo", r#"{"message":"hi"}"#]));
    assert!(!piped.stderr.contains("step 2"), "{}", piped.stderr);
}
