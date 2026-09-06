//! Multi Round-Trip Requests: a 2026-07-28 server asking for one more fact
//! before it will answer, and mcpdial answering it with a second request.
//!
//! The revision took server-initiated requests out. What elicitation was is now
//! a `resultType: "input_required"` result carrying the requests to serve and an
//! opaque `requestState`; the client answers by sending the same call again with
//! `inputResponses` under the keys the server named them by. That is a returned
//! value rather than a question down an open connection, so the answer is a
//! fresh POST - and Streamable HTTP, which could never reply mid-stream, can
//! answer at last.
//!
//! The property that outranks all of it: **the non-interactive path cannot
//! hang.** A piped stdin, `--json` and `--plain` each decline at once. Every
//! test here that is not about a person typing runs under
//! [`common::within_bound`], so a client that stopped to ask a question nobody
//! can answer fails the clock rather than sitting there.

mod common;

use common::{
    mcpdial, no_pty, run, start, temp_home, with_a_silent_pipe_on_stdin, within_a_pty, FakeServer,
    Mode, BOUND, MODERN_VERSION,
};
use serde_json::Value;

fn bodies(s: &FakeServer) -> Vec<Value> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .collect()
}

/// Every `tools/call` the server was sent, in order.
fn calls(s: &FakeServer) -> Vec<Value> {
    bodies(s)
        .into_iter()
        .filter(|m| m["method"] == "tools/call")
        .collect()
}

fn declared(call: &Value) -> &Value {
    &call["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]
}

/// The whole pattern, over Streamable HTTP, with the answers given ahead of
/// time: the demand is served, the call goes again carrying the answers and the
/// state, and the server's second result is what the caller sees.
#[test]
fn a_demand_for_input_is_answered_and_the_call_goes_again_over_http() {
    let s = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-answered");

    let o = run(mcpdial(&home).args([
        "call",
        &s.url,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm": true}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout
            .contains(r#"elicited {"action":"accept","content":{"confirm":true}}"#),
        "{}",
        o.stdout
    );
    // The state came back exactly as it went out, and nothing here read it.
    assert!(o.stdout.contains(r#"state "opaque-state""#), "{}", o.stdout);
    assert!(
        o.stderr.contains("server asked: confirm before running"),
        "{}",
        o.stderr
    );

    let calls = calls(&s);
    assert_eq!(calls.len(), 2, "one demand, one retry");
    assert_ne!(calls[0]["id"], calls[1]["id"], "two independent requests");
    assert!(
        calls[0]["params"]["inputResponses"].is_null(),
        "nothing was answered before anything was asked"
    );
    assert_eq!(
        calls[1]["params"]["inputResponses"]["confirm"]["content"]["confirm"],
        true
    );
    assert_eq!(calls[1]["params"]["requestState"], "opaque-state");
    assert_eq!(calls[1]["params"]["arguments"], serde_json::json!({}));
    assert_eq!(
        calls[1]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        MODERN_VERSION
    );
}

/// #118 found Streamable HTTP could promise nothing: the question arrived on the
/// response stream and the reply would have needed a second POST while the first
/// was still open, so `elicitation` was declared on stdio alone. A demand for
/// input is a returned value, and a second POST is exactly what answers it.
#[test]
fn streamable_http_declares_elicitation_on_the_new_revision_and_not_the_old() {
    let modern = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-declared");
    let o = run(mcpdial(&home).args([
        "call",
        &modern.url,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm": true}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let calls = calls(&modern);
    assert_eq!(
        declared(&calls[0]),
        &serde_json::json!({"elicitation": {"form": {}, "url": {}}}),
        "the request that invited the demand declared it could serve one"
    );
    // And the server saw the same declaration on the retry it answered.
    assert!(
        o.stdout
            .contains(r#"declared {"elicitation":{"form":{},"url":{}}}"#),
        "{}",
        o.stdout
    );

    // The older revision over the same transport still promises nothing: there
    // is nowhere for the reply to go, and #118's rule has not moved.
    let legacy = start(Mode::Stateless);
    let home = temp_home("mrtr-declared-legacy");
    let o = run(mcpdial(&home).args(["-v", "call", &legacy.url, "echo", "{}"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains(r#""capabilities":{}"#),
        "http declared something it could not answer: {}",
        o.stderr
    );
}

/// A capability is promised only where something can honour it. Without a
/// terminal and without `--elicit` there is nothing that can fill in a form, so
/// only url mode is declared - and a server that asks for a form anyway is
/// declined rather than left waiting.
#[test]
fn only_what_can_be_answered_is_declared_and_the_rest_is_declined() {
    let s = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-honest");

    let done = with_a_silent_pipe_on_stdin(mcpdial(&home).args(["--json", "call", &s.url, "echo"]));
    assert_eq!(done.code, 0, "{}", done.output);
    assert!(done.took < BOUND, "took {:?}", done.took);

    let calls = calls(&s);
    assert_eq!(
        declared(&calls[0]),
        &serde_json::json!({"elicitation": {"url": {}}}),
        "no form can be filled in here, so none is promised"
    );
    assert_eq!(
        calls[1]["params"]["inputResponses"]["confirm"]["action"], "decline",
        "the demand was answered, not waited on"
    );
    assert!(
        done.output.contains(r#"elicited {\"action\":\"decline\"}"#),
        "{}",
        done.output
    );
}

/// The property this whole feature lives under. A prompt with nobody in front of
/// it never returns, so each of these runs against a server that will not answer
/// until it is told something, with an open pipe on stdin and a wall clock: a
/// client that stopped to ask fails the clock, whatever it printed.
#[test]
fn nothing_without_a_person_in_front_of_it_ever_waits_for_an_answer() {
    let s = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-no-hang");
    for flags in [vec![], vec!["--json"], vec!["--plain"]] {
        let args: Vec<&str> = flags
            .iter()
            .copied()
            .chain(["call", s.url.as_str(), "echo"])
            .collect();
        let done = with_a_silent_pipe_on_stdin(mcpdial(&home).args(&args));
        assert_eq!(done.code, 0, "{flags:?}: {}", done.output);
        assert!(
            done.output.contains("decline"),
            "{flags:?} did not decline: {}",
            done.output
        );
        assert!(
            !done.output.contains("Really run it?"),
            "{flags:?} put the form to a pipe: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{flags:?} took {:?}, which is the bound",
            done.took
        );
    }
}

/// A pseudo-terminal is the harder half of the same rule: everything is a
/// terminal, so the only thing standing between a program and a question it
/// cannot answer is the flag it passed. `--plain`, `MCPDIAL_PLAIN` and a dumb
/// terminal each say the output is a program's, and #82 already has them decline
/// a missing argument; a server's question is the same question put twice, so it
/// is declined here too rather than typed at a terminal nobody is watching.
#[test]
fn a_terminal_that_asked_for_plain_output_is_not_asked_either() {
    if no_pty() {
        return;
    }
    let s = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-plain-pty");
    let call = ["call", s.url.as_str(), "echo"];
    for (flags, env) in [
        (vec!["--plain"], vec![]),
        (vec!["--json"], vec![]),
        (vec![], vec![("MCPDIAL_PLAIN", "1")]),
        (vec![], vec![("TERM", "dumb")]),
    ] {
        let args: Vec<&str> = flags.iter().copied().chain(call).collect();
        let done = within_a_pty(&home, &args, &env);
        assert_eq!(done.code, 0, "{flags:?} {env:?}: {}", done.output);
        assert!(
            done.output.contains("decline"),
            "{flags:?} {env:?} did not decline: {}",
            done.output
        );
        assert!(
            !done.output.contains("Really run it?"),
            "{flags:?} {env:?} put the form to a program: {}",
            done.output
        );
        assert!(
            done.took < BOUND,
            "{flags:?} {env:?} took {:?}, which is the bound",
            done.took
        );
    }
}

/// The other way round trips could hang: a server that is never satisfied. It
/// demands again however it is answered, so the client is the one that has to
/// stop, and say why.
#[test]
fn a_server_that_never_stops_asking_is_given_up_on_rather_than_answered_for_ever() {
    let s = start(Mode::ModernInput { insatiable: true });
    let home = temp_home("mrtr-insatiable");

    let done = with_a_silent_pipe_on_stdin(mcpdial(&home).args([
        "call",
        &s.url,
        "echo",
        "{}",
        "--elicit",
        r#"{"confirm": true}"#,
    ]));
    assert_eq!(done.code, 1, "{}", done.output);
    assert!(
        done.output.contains("still asking"),
        "it did not say why it stopped: {}",
        done.output
    );
    assert!(done.took < BOUND, "took {:?}", done.took);
    assert!(
        calls(&s).len() <= 5,
        "it went round {} times",
        calls(&s).len()
    );
}

/// `raw` was asked to send one request. A result demanding input is a fact about
/// the server worth seeing, not something to answer behind the caller's back.
#[test]
fn raw_shows_the_demand_rather_than_answering_it() {
    let s = start(Mode::ModernInput { insatiable: false });
    let home = temp_home("mrtr-raw");

    let o = run(mcpdial(&home).args([
        "--json",
        "raw",
        &s.url,
        "tools/call",
        r#"{"name":"echo","arguments":{}}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(result["resultType"], "input_required");
    assert_eq!(result["requestState"], "opaque-state");
    assert_eq!(
        result["inputRequests"]["confirm"]["method"],
        "elicitation/create"
    );
    assert_eq!(calls(&s).len(), 1, "one request is one request");
}
