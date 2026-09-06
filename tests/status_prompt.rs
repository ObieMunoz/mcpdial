//! #87: what a shell session says about itself - the one line on connect, the
//! status dot in the prompt, and the reconnect after the transport drops.
//!
//! All of it is the terminal's, so the runs that assert it need a pseudo-
//! terminal and the runs that assert its absence need a pipe. A terminal with
//! nobody typing at it waits for ever, so every wait here is on [`BOUND`] and a
//! shell still going when the clock runs out is killed and fails.

mod common;

use common::{
    both_streams, echo_command, mcpdial, no_pty, pty_args, pty_line, run, start, temp_home,
    within_bound, Mode,
};
use std::io::Write;

/// The healthy dot, the one a token running out gets, and the one left after
/// the transport drops: full, half, hollow.
const FINE: &str = "\u{25cf}";
const EXPIRING: &str = "\u{25d0}";
const LOST: &str = "\u{25cb}";

/// Types `lines` at an `mcpdial shell` in a pseudo-terminal, waiting for what
/// each one printed before typing the next, and hands back the whole screen.
///
/// A line at a time because the line editor builds a fresh reader for each one:
/// what the last reader took off the terminal goes with it, so a line typed
/// ahead of the one before it being read is read by nobody.
fn at_a_terminal(
    home: &std::path::Path,
    target: &str,
    env: &[(&str, &str)],
    lines: &[(&str, &str)],
) -> String {
    let mut child = pty_line(home, &pty_args(&["shell", target]), env)
        .spawn()
        .expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    // Not before the line editor is there to read them: keys typed at a
    // terminal that is still connecting are echoed, not acted on.
    printed.wait_for("connected  ");
    for (keys, expected) in lines {
        typing
            .write_all(keys.as_bytes())
            .expect("type at the shell");
        typing.flush().ok();
        printed.wait_for(expected);
    }
    typing.write_all(b"quit\r").expect("type at the shell");
    typing.flush().ok();
    within_bound(child, Some(typing), &printed).output
}

fn rich_at_a_terminal() -> bool {
    if no_pty() {
        return false;
    }
    if !cfg!(feature = "rich") {
        eprintln!("skipped: without the rich presenter a terminal is a pipe");
        return false;
    }
    true
}

/// One line on connect, saying what answered and how much of itself it offered,
/// and a green dot in the prompt from then on.
#[test]
fn a_terminal_is_told_what_connected_and_shown_that_it_is_healthy() {
    if !rich_at_a_terminal() {
        return;
    }
    let s = start(Mode::Stateless);
    let home = temp_home("status-connected");
    let shown = at_a_terminal(&home, &s.url, &[], &[("tools\r", "Add two numbers")]);

    assert!(
        shown.contains("connected  fake-mcp 1.0  2 tools  2 resources  2 prompts"),
        "one line, with the counts: {shown}"
    );
    assert!(
        !shown.contains("connected to fake-mcp"),
        "and not the two lines it replaced: {shown}"
    );
    assert!(
        shown.contains(FINE),
        "the prompt wears the healthy dot: {shown:?}"
    );
}

/// A token with less than ten minutes left turns the dot yellow, and is said
/// once, in one dim line, with the command that renews it.
#[test]
fn a_token_running_out_is_shown_and_said_once() {
    if !rich_at_a_terminal() {
        return;
    }
    let s = start(Mode::Stateless);
    let home = temp_home("status-expiring");
    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &s.url, "--no-probe"])).code,
        0
    );
    let soon = mcpdial::config::now() + 5 * 60;
    std::fs::write(
        home.join("credentials.json"),
        serde_json::json!({"credentials": {"web": {"access_token": "tok", "expires_at": soon}}})
            .to_string(),
    )
    .expect("write a credential that is nearly out");

    let shown = at_a_terminal(
        &home,
        "web",
        &[],
        &[
            ("tools\r", "Add two numbers"),
            ("prompts\r", "Greet someone"),
        ],
    );
    assert!(
        shown.contains(EXPIRING),
        "the prompt wears the half dot: {shown:?}"
    );
    assert_eq!(
        shown.matches("the token expires in").count(),
        1,
        "said once, however many prompts follow: {shown}"
    );
    assert!(
        shown.contains("`mcpdial login web` renews it"),
        "and says what to do about it: {shown}"
    );
}

/// A stdio server that exits under a call leaves the session with nothing on
/// the other end: the dot goes hollow, and the next command dials again and
/// says so once.
#[test]
fn a_dropped_session_shows_it_and_the_next_command_dials_again() {
    if !rich_at_a_terminal() {
        return;
    }
    let home = temp_home("status-dropped");
    let target = format!("stdio:{}", echo_command());
    let shown = at_a_terminal(
        &home,
        &target,
        &[("ECHO_SERVER_EXIT_ON_CALL", "2")],
        &[
            ("call count\r", "count=1"),
            ("call count\r", "exited with status 9"),
            ("call count\r", "reconnected to"),
        ],
    );

    assert!(
        shown.contains(LOST),
        "the prompt says the session is gone: {shown:?}"
    );
    // One line for the reconnect, and a process that counts from one again,
    // which is the whole point of having dialed it.
    assert_eq!(
        shown.matches("reconnected to").count(),
        1,
        "one line, not one per command: {shown}"
    );
    assert_eq!(
        shown.matches("count=1").count(),
        2,
        "the fresh process started its own count: {shown}"
    );
}

/// None of it reaches a pipe: no connect line, no dot, and a stdio server that
/// died stays dead rather than being restarted under a script that would then
/// be talking to a process holding none of the state it had built up.
#[test]
fn a_piped_shell_is_told_nothing_and_nothing_is_dialed_again_under_it() {
    let home = temp_home("status-piped");
    let target = format!("stdio:{}", echo_command());
    let mut cmd = mcpdial(&home);
    cmd.args(["shell", &target])
        .env("ECHO_SERVER_EXIT_ON_CALL", "2")
        .stdin(std::process::Stdio::piped());
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mcpdial");
    let mut typing = child.stdin.take().expect("stdin is piped");
    typing
        .write_all(b"call count\ncall count\ncall count\n")
        .expect("write the script");
    drop(typing);
    let printed = both_streams(&mut child);
    let done = within_bound(child, None, &printed);

    assert_eq!(done.code, 1, "{}", done.output);
    for absent in ["connected  ", "reconnected to", FINE, LOST] {
        assert!(
            !done.output.contains(absent),
            "{absent:?} reached a pipe: {}",
            done.output
        );
    }
    assert_eq!(
        done.output.matches("count=1").count(),
        1,
        "a restarted server would have counted from one again: {}",
        done.output
    );
}

/// The prompt a redirected stdout is given is the one it has always been given,
/// whatever the session is doing: a person typing into a pipe is still writing
/// bytes something else will read.
#[test]
fn a_redirected_stdout_keeps_the_prompt_it_has_always_had() {
    if no_pty() {
        return;
    }
    let s = start(Mode::Stateless);
    let home = temp_home("status-redirected");
    let line = format!("{} | cat", pty_args(&["shell", &s.url]));
    let mut child = pty_line(&home, &line, &[]).spawn().expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    typing.write_all(b"quit\n").expect("type at the shell");
    typing.flush().ok();
    let shown = within_bound(child, Some(typing), &printed).output;

    // The fake server is `fake-mcp`, and an ad-hoc URL is prompted by that.
    assert!(shown.contains("fake-mcp> "), "{shown:?}");
    for absent in [FINE, LOST] {
        assert!(
            !shown.contains(absent),
            "{absent:?} in a pipe's prompt: {shown:?}"
        );
    }
}

/// A tool call is not the only thing a run prints: what the server logs on its
/// way is one line beside the answer, and the answer itself still goes to
/// stdout untouched.
#[test]
fn a_servers_log_line_is_one_line_beside_the_answer() {
    let s = start(Mode::Logging);
    let home = temp_home("status-logging");
    let o = run(mcpdial(&home).args([
        "--log-level",
        "debug",
        "call",
        &s.url,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let logs: Vec<&str> = o
        .stderr
        .lines()
        .filter(|l| l.contains("server ["))
        .collect();
    assert!(!logs.is_empty(), "{}", o.stderr);
    for log in logs {
        assert!(
            log.starts_with("server ["),
            "one line, with its level, and nothing painted onto a pipe: {log:?}"
        );
    }
}
