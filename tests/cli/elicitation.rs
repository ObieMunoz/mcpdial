//! A server asking for one more fact, and every way of answering it.

use crate::common::{echo_command, mcpdial, run, start, temp_home, timed, Mode};
use serde_json::{json, Value};
use std::process::Command;

/// The echo server in the mode where every `tools/call` asks the client for one
/// more fact and blocks until the answer arrives.
pub(crate) fn elicits(home: &std::path::Path, mode: &str) -> Command {
    let mut c = mcpdial(home);
    c.env("ECHO_SERVER_ELICIT", mode);
    c
}

#[test]
fn an_elicitation_with_nobody_to_ask_is_declined_rather_than_refused() {
    let home = temp_home("elicit-decline");
    let target = format!("stdio:{}", echo_command());

    // Nothing here is a terminal, so there is no one to put the question to.
    // The old answer was -32601, which fails the call; a decline is an answer
    // the server can degrade around, and it has to arrive at once.
    let (took, o) =
        timed(elicits(&home, "form").args(["--timeout", "30", "call", &target, "echo", "{}"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), r#"elicited {"action":"decline"}"#);
    assert!(
        o.stderr
            .contains("server asked: confirm before running; declined (no terminal; use --elicit)"),
        "{}",
        o.stderr
    );
    assert!(
        took < std::time::Duration::from_secs(20),
        "the call must not sit on the timeout waiting for a person, {took:?}"
    );

    // And with nothing that can fill in a form, none is offered at initialize.
    let o = run(elicits(&home, "form").args(["-v", "call", &target, "echo", "{}"]));
    assert!(
        o.stderr
            .contains(r#""capabilities":{"elicitation":{"url":{}}}"#),
        "{}",
        o.stderr
    );
}

#[test]
fn elicit_answers_a_form_from_the_command_line() {
    let home = temp_home("elicit-answers");
    let target = format!("stdio:{}", echo_command());

    let o = run(elicits(&home, "form").args([
        "-v",
        "call",
        &target,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm":true,"region":"eu","spare":"unused"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        r#"elicited {"action":"accept","content":{"confirm":true,"region":"eu"}}"#
    );
    assert!(
        o.stderr
            .contains("server asked: confirm before running; answered from --elicit"),
        "{}",
        o.stderr
    );
    // Having something to answer with is what makes the form capability true.
    assert!(
        o.stderr
            .contains(r#""capabilities":{"elicitation":{"form":{},"url":{}}}"#),
        "{}",
        o.stderr
    );

    // A file of answers is the same thing, for a form too long for one line.
    let file = home.join("answers.json");
    std::fs::write(&file, r#"{"confirm":false,"region":"us","count":2}"#).unwrap();
    let o = run(elicits(&home, "form").args([
        "call",
        &target,
        "echo",
        "{}",
        "--elicit",
        &format!("@{}", file.display()),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        r#"elicited {"action":"accept","content":{"confirm":false,"count":2,"region":"us"}}"#
    );
}

#[test]
fn an_answer_the_schema_forbids_is_declined_before_it_is_sent() {
    let home = temp_home("elicit-invalid");
    let target = format!("stdio:{}", echo_command());

    // `count` is capped at 3. Sending 9 anyway would make the server reject a
    // form it had already declared the bounds of.
    let o = run(elicits(&home, "form").args([
        "call",
        &target,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm":true,"region":"eu","count":9}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), r#"elicited {"action":"decline"}"#);
    assert!(
        o.stderr
            .contains("declined (--elicit count: 9 is more than the maximum 3)"),
        "{}",
        o.stderr
    );

    // A required property the answers do not cover is declined by name.
    let o = run(elicits(&home, "form").args([
        "call",
        &target,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm":true}"#,
    ]));
    assert_eq!(o.stdout.trim(), r#"elicited {"action":"decline"}"#);
    assert!(
        o.stderr
            .contains(r#"declined (--elicit has no value for "region")"#),
        "{}",
        o.stderr
    );
}

#[test]
fn a_url_elicitation_names_its_address_and_is_accepted() {
    let home = temp_home("elicit-url");
    let target = format!("stdio:{}", echo_command());

    let o = run(elicits(&home, "url").args(["-v", "call", &target, "echo", "{}", "--no-browser"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    // The spec has a url-mode accept carry no content: the interaction happens
    // out of band, and the server may say so later.
    assert_eq!(o.stdout.trim(), r#"elicited {"action":"accept"}"#);
    assert!(
        o.stderr.contains("https://example.test/elicit/1"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains("notifications/elicitation/complete"),
        "the completion notification shows under -v: {}",
        o.stderr
    );
    // url mode needs nobody at a terminal, so it is always on offer.
    assert!(
        o.stderr.contains(r#""elicitation":{"url":{}}"#),
        "{}",
        o.stderr
    );
}

#[test]
fn under_json_an_elicitation_is_an_object_on_stderr_and_never_blocks() {
    let home = temp_home("elicit-json");
    let target = format!("stdio:{}", echo_command());

    let o = run(elicits(&home, "form").args(["--json", "call", &target, "echo", "{}"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let note: Value = o
        .stderr
        .lines()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .expect("every --json stderr line is an object");
    assert_eq!(note["elicitation"]["action"], "decline");
    assert_eq!(note["elicitation"]["message"], "confirm before running");
    assert!(note["elicitation"]["detail"]
        .as_str()
        .unwrap()
        .contains("--elicit"));
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        result["content"][0]["text"],
        r#"elicited {"action":"decline"}"#
    );

    // With answers in hand, --json accepts without ever asking a human.
    let o = run(elicits(&home, "form").args([
        "--json",
        "call",
        &target,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm":true,"region":"us"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        result["content"][0]["text"],
        r#"elicited {"action":"accept","content":{"confirm":true,"region":"us"}}"#
    );
}

#[test]
fn streamable_http_promises_no_elicitation_it_could_not_answer() {
    let s = start(Mode::Stateless);
    let home = temp_home("elicit-http");

    // The question would arrive on the response stream, but replying to it
    // needs a second POST while the first is still open. Declaring the
    // capability anyway would invite a server to ask and then wait out the
    // whole timeout, which is the one thing an elicitation must never cost.
    let o = run(mcpdial(&home).args([
        "call",
        &s.url,
        "add",
        r#"{"a":1,"b":1}"#,
        "--elicit",
        r#"{"confirm":true}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let requests = s.requests.lock().unwrap();
    let handshake = requests
        .iter()
        .map(crate::common::Recorded::json)
        .find(|body| body["method"] == "initialize")
        .expect("an initialize request");
    assert_eq!(
        handshake["params"]["capabilities"],
        json!({}),
        "nothing may be declared where nothing can answer"
    );
}
