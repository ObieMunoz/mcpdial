//! Prompting for a missing argument, and the far more important half: not
//! prompting.
//!
//! A prompt is the one thing in the terminal experience that can hang its
//! caller for ever. So every test here that is not about a person typing runs
//! the command with an open pipe on its stdin - one nothing is ever written to
//! and nobody ever closes - and gives it a wall clock to finish inside. A
//! command that stops to ask a question there never finishes, so it is the
//! clock that fails, whether or not the output happens to look right.

mod common;

use common::{
    both_streams, echo_command, mcpdial, no_pty, pty_args, pty_line, temp_home,
    with_a_silent_pipe_on_stdin, within_a_pty, within_bound, BOUND,
};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than [`BOUND`], so that a stdin which is supposed to stay silent has
/// not gone quiet on its own before the clock runs out. Short enough that the
/// process it belongs to is gone soon after the test is.
const SILENCE: Duration = Duration::from_secs(30);

/// The question `echo` is asked when nothing filled in `message`. Kept short so
/// that no terminal width can wrap it away from a search.
const QUESTION: &str = "message (string, required)";

/// What a missing `message` gets where there is nobody to ask: the server's own
/// complaint, and the usage line under it. These are the bytes the contract
/// snapshots hold.
const TODAYS_ERROR: &str = "Required at message";
const TODAYS_HINT: &str = "mcpdial call echo echo message=<string>";

fn home_with_echo(tag: &str) -> PathBuf {
    let home = temp_home(tag);
    let saved = mcpdial(&home)
        .args(["add", "echo", "--stdio", &echo_command(), "--no-probe"])
        .output()
        .expect("spawn mcpdial");
    assert!(saved.status.success(), "{saved:?}");
    home
}

/// The rule the whole feature lives under: a missing required argument with
/// anything but a person on stdin is today's error, and it arrives.
#[test]
fn a_pipe_gets_todays_error_and_never_waits_for_an_answer() {
    let home = home_with_echo("prompts-pipe");
    for args in [
        vec!["call", "echo", "echo"],
        vec!["call", "echo", "echo", "{}"],
        vec!["--json", "call", "echo", "echo"],
        vec!["--plain", "call", "echo", "echo"],
    ] {
        let done = with_a_silent_pipe_on_stdin(mcpdial(&home).args(&args));
        assert_eq!(done.code, 1, "{args:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_ERROR),
            "{args:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(QUESTION),
            "{args:?} asked a question into a pipe: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{args:?} took {:?}, which is the bound",
            done.took
        );
    }
}

/// The dangerous half of the same rule. At a terminal `Rich` is chosen and
/// everything else about the output changes; only the question of what is on
/// stdin stands between a piped-in caller and a wait with no end. The pipe here
/// is fed by a process that says nothing for longer than the bound, so a
/// command that reads it does not finish, and this test does not either.
#[test]
fn a_terminal_reading_a_silent_pipe_still_gets_todays_error() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-tty-pipe");
    let piped_in = format!(
        "sleep {} | {}",
        SILENCE.as_secs(),
        pty_args(&["call", "echo", "echo"])
    );
    let mut child = pty_line(&home, &piped_in, &[])
        .spawn()
        .expect("spawn script");
    let held = child.stdin.take();
    let printed = both_streams(&mut child);
    let shown = printed.wait_for(TODAYS_ERROR);
    assert!(shown.contains(TODAYS_HINT), "{shown}");
    assert!(!shown.contains(QUESTION), "{shown}");
    // The sleep still holds the pipeline open, so the shell has not exited and
    // never will inside the bound; the command inside it is what was watched.
    child.kill().ok();
    child.wait().ok();
    drop(held);
}

/// `--json` and `--plain` never prompt, whatever the terminal. Each is run at a
/// terminal on both ends, where nothing but the flag stands in the way.
#[test]
fn nothing_that_asked_for_plain_output_is_ever_asked_a_question() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-flags");
    for (args, env) in [
        (vec!["--json", "call", "echo", "echo"], vec![]),
        (vec!["--plain", "call", "echo", "echo"], vec![]),
        (vec!["call", "echo", "echo"], vec![("MCPDIAL_PLAIN", "1")]),
        (vec!["call", "echo", "echo"], vec![("TERM", "dumb")]),
    ] {
        let done = within_a_pty(&home, &args, &env);
        assert_eq!(done.code, 1, "{args:?} {env:?}: {}", done.output);
        assert!(
            done.output.contains(TODAYS_ERROR),
            "{args:?} {env:?}: {}",
            done.output
        );
        assert!(
            !done.output.contains(QUESTION),
            "{args:?} {env:?} asked a question it must not ask: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{args:?} {env:?} took {:?}, the bound",
            done.took
        );
    }
}

/// And what it is all for: at a terminal the missing argument is asked for, the
/// answer goes into the call, and the line that would have made the same call
/// outright is printed so it can go into a script.
#[test]
fn a_terminal_is_asked_for_what_the_call_left_out() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-tty");
    if !cfg!(feature = "rich") {
        // The agent-only build has no `Rich` to ask with, so a terminal is a
        // pipe here too, and gets exactly what a pipe gets.
        let done = within_a_pty(&home, &["call", "echo", "echo"], &[]);
        assert!(done.output.contains(TODAYS_ERROR), "{}", done.output);
        assert!(!done.output.contains(QUESTION), "{}", done.output);
        assert!(done.took < BOUND, "took {:?}, the bound", done.took);
        return;
    }
    let mut child = pty_line(&home, &pty_args(&["call", "echo", "echo"]), &[])
        .spawn()
        .expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    // Answered only once the question is on the screen, so what is asserted
    // below is an answer to it and not a line that raced past it.
    printed.wait_for(QUESTION);
    writeln!(typing, "hello there").unwrap();
    typing.flush().unwrap();

    let done = within_bound(child, Some(typing), &printed);
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(
        done.output.contains("Echo: hello there"),
        "the answer was sent as the argument: {}",
        done.output
    );
    assert!(
        done.output
            .contains("mcpdial call echo echo 'message=hello there'"),
        "the finished call is echoed as a command line: {}",
        done.output
    );
    assert!(
        !done.output.contains(TODAYS_ERROR),
        "nothing was left for the server to complain about: {}",
        done.output
    );
}

/// A call that is already complete is not interrupted to be asked about, and
/// neither is a tool that requires nothing.
#[test]
fn a_complete_call_at_a_terminal_is_left_alone() {
    if no_pty() {
        return;
    }
    let home = home_with_echo("prompts-complete");
    for (args, printed) in [
        (vec!["call", "echo", "echo", "message=hi"], "Echo: hi"),
        (
            vec!["call", "echo", "echo", r#"{"message":"hi"}"#],
            "Echo: hi",
        ),
        (vec!["call", "echo", "count"], "count=1"),
    ] {
        let done = within_a_pty(&home, &args, &[]);
        assert_eq!(done.code, 0, "{args:?}: {}", done.output);
        assert!(done.output.contains(printed), "{args:?}: {}", done.output);
        assert!(
            !done.output.contains("required)") && !done.output.contains("enter to skip"),
            "{args:?} was asked something: {}",
            done.output
        );
        assert!(done.took < BOUND, "{args:?} took {:?}", done.took);
    }
}
