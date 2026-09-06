//! `completion/complete`: what a server suggests for a prompt argument or a
//! resource template variable, on the command line and under Tab.
//!
//! The Tab half runs inside a pseudo-terminal, because the line editor exists
//! nowhere else. A terminal with nobody typing at it waits for ever, so both
//! runs here are on a wall clock: each is given [`common::BOUND`] to reach what
//! it is waiting for and to finish, and one still going when the clock runs out
//! is killed and fails. That is what makes a wedged Tab a failing test rather
//! than a hung suite.

mod common;

use common::{
    both_streams, mcpdial, pty_args, pty_line, run, start, temp_home, within_bound, Mode,
};
use serde_json::{json, Value};
use std::io::Write;

/// The template the fake server lists, whose one variable only it can fill in.
const NOTES: &str = "file:///notes/{name}.md";

/// Plain output at a terminal, so what the screen shows is what the assertions
/// read; the line editor is there either way.
const PLAINLY: &[(&str, &str)] = &[("MCPDIAL_PLAIN", "1")];

#[test]
fn suggests_values_for_a_prompt_argument() {
    let s = start(Mode::Completing { answers: true });
    let home = temp_home("complete-prompt");

    let o = run(mcpdial(&home).args(["complete", &s.url, "prompt", "summarize", "style", "t"]));
    assert_eq!(o.code, 0, "{}", said(&o));
    assert_eq!(o.stdout, "terse\nthorough\n");

    // What has been typed narrows it, and a value nothing matches is empty
    // output and still a success: the server was asked, and had nothing.
    let narrowed =
        run(mcpdial(&home).args(["complete", &s.url, "prompt", "summarize", "style", "th"]));
    assert_eq!(narrowed.stdout, "thorough\n");
    let nothing =
        run(mcpdial(&home).args(["complete", &s.url, "prompt", "summarize", "style", "zz"]));
    assert_eq!((nothing.code, nothing.stdout.as_str()), (0, ""));

    // An argument this server suggests nothing for is not an error either.
    let unknown = run(mcpdial(&home).args(["complete", &s.url, "prompt", "summarize", "text"]));
    assert_eq!((unknown.code, unknown.stdout.as_str()), (0, ""));
}

#[test]
fn under_json_it_is_the_values_and_whether_there_are_more() {
    let s = start(Mode::Completing { answers: true });
    let home = temp_home("complete-json");

    let o = run(mcpdial(&home).args([
        "--json",
        "complete",
        &s.url,
        "prompt",
        "summarize",
        "style",
        "t",
    ]));
    assert_eq!(o.code, 0, "{}", said(&o));
    let found: Value = serde_json::from_str(&o.stdout).expect("one JSON object");
    assert_eq!(
        found,
        json!({"values": ["terse", "thorough"], "hasMore": false})
    );

    // The server holds one back and says so - in the field a script reads, and
    // on stderr rather than among the values when there is no field.
    let more = run(mcpdial(&home).args(["--json", "complete", &s.url, "resource", NOTES, "name"]));
    let found: Value = serde_json::from_str(&more.stdout).expect("one JSON object");
    assert_eq!(
        found,
        json!({"values": ["weekly", "daily"], "hasMore": true})
    );
    let plain = run(mcpdial(&home).args(["complete", &s.url, "resource", NOTES, "name"]));
    assert_eq!(plain.stdout, "weekly\ndaily\n");
    assert!(plain.stderr.contains("has more"), "{}", said(&plain));
}

#[test]
fn what_is_already_settled_goes_with_the_request() {
    let s = start(Mode::Completing { answers: true });
    let home = temp_home("complete-context");

    let o = run(mcpdial(&home).args([
        "complete",
        &s.url,
        "resource",
        NOTES,
        "name",
        "--context",
        r#"{"author": "ada"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", said(&o));
    // The fake server narrows by the author it was told about, so its answer is
    // itself proof that the context arrived.
    assert_eq!(o.stdout, "ada-notes\n");

    let sent = completion_requests(&s);
    assert_eq!(sent.len(), 1, "one completion request, not a retry");
    assert_eq!(
        sent[0]["params"],
        json!({
            "ref": {"type": "ref/resource", "uri": NOTES},
            "argument": {"name": "name", "value": ""},
            "context": {"arguments": {"author": "ada"}},
        })
    );
}

#[test]
fn a_server_without_the_capability_says_so_and_fails() {
    let s = start(Mode::Stateless);
    let home = temp_home("complete-no-capability");

    let o = run(mcpdial(&home).args(["complete", &s.url, "prompt", "summarize", "style"]));
    assert_eq!(o.code, 1, "{}", said(&o));
    assert!(o.stderr.contains("no completions"), "{}", said(&o));
    assert!(o.stderr.contains("info"), "{}", said(&o));
}

#[test]
fn tab_writes_the_value_the_server_suggested_into_the_line() {
    if common::no_pty() {
        return;
    }
    let s = start(Mode::Completing { answers: true });
    let home = temp_home("complete-tab");
    // Tab where one value matches: the editor writes it in place of the half
    // that was typed, the `}` closes the object, and the prompt that runs is
    // the completed one.
    let shown = at_a_terminal(
        &home,
        &s.url,
        &[("prompt summarize {\"style\": \"th\t}\r", "Summarize this:")],
    );
    assert!(
        shown.contains("thorough"),
        "Tab did not write the value the server suggested:\n{shown}"
    );
    assert_eq!(
        completion_requests(&s).len(),
        1,
        "one Tab, one request:\n{shown}"
    );
}

/// The whole point of the bound: a server that takes the request and never
/// answers costs one Tab its two seconds, and the session nothing at all.
///
/// The quote Tab was pressed inside is closed by hand here, so the prompt only
/// runs if Tab left the line exactly as it was; the `tools` after it is the
/// session carrying on as though nothing had happened.
#[test]
fn a_completion_that_never_comes_back_leaves_the_shell_working() {
    if common::no_pty() {
        return;
    }
    let s = start(Mode::Completing { answers: false });
    let home = temp_home("complete-hangs");
    let shown = at_a_terminal(
        &home,
        &s.url,
        &[
            (
                "prompt summarize {\"style\": \"th\t\"}\r",
                "Summarize this:",
            ),
            ("tools\r", "tool(s):"),
        ],
    );
    assert!(
        !shown.contains("thorough"),
        "the server answered after all, so this proves nothing:\n{shown}"
    );
    assert_eq!(
        completion_requests(&s).len(),
        1,
        "Tab never asked, so it was not the waiting that was bounded:\n{shown}"
    );
}

/// Both streams of a run, for an assertion that has to say what happened.
fn said(o: &common::Out) -> String {
    format!("stdout:\n{}stderr:\n{}", o.stdout, o.stderr)
}

fn completion_requests(s: &common::FakeServer) -> Vec<Value> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(common::Recorded::json)
        .filter(|m| m["method"] == "completion/complete")
        .collect()
}

/// Type at an `mcpdial shell` running in a pseudo-terminal, a line at a time,
/// waiting for what each line printed before typing the next, then `quit`.
/// Hands back everything that reached the screen.
///
/// A line at a time because the line editor builds a fresh reader for each one:
/// what the last reader had already taken off the terminal goes with it, so a
/// line typed ahead of the one before it being read is read by nobody.
///
/// Every wait is on the clock, and so is the exit. A Tab that wedges the editor
/// prints nothing and fails the wait it is holding up.
fn at_a_terminal(home: &std::path::Path, url: &str, lines: &[(&str, &str)]) -> String {
    let mut child = pty_line(home, &pty_args(&["shell", url]), PLAINLY)
        .spawn()
        .expect("spawn script");
    let mut typing = child.stdin.take().expect("stdin is piped");
    let printed = both_streams(&mut child);
    // Not before the line editor is there to read them: keys typed at a
    // terminal that is still connecting are echoed, not completed.
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
