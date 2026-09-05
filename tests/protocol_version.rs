//! Protocol version negotiation end to end: what `initialize` offers, what `info`
//! reports the server agreed to, and how `--protocol-version` pins the offer.

mod common;

use common::{mcpdial, run, start, temp_home, Mode};
use serde_json::Value;

fn offered(s: &common::FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "initialize")
        .map(|m| m["params"]["protocolVersion"].as_str().unwrap().to_string())
        .collect()
}

fn headers_after_the_handshake(s: &common::FakeServer) -> Vec<Option<String>> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.json()["method"] != "initialize")
        .map(|r| r.header("mcp-protocol-version").map(str::to_string))
        .collect()
}

#[test]
fn the_newest_version_is_offered_and_a_server_that_takes_it_runs_on_it() {
    let s = start(Mode::EchoProtocol);
    let home = temp_home("echo-protocol");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-11-25"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let init: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(init["protocolVersion"], "2025-11-25");

    assert_eq!(offered(&s), ["2025-11-25", "2025-11-25"]);
    let headers = headers_after_the_handshake(&s);
    assert!(!headers.is_empty());
    assert!(
        headers.iter().all(|h| h.as_deref() == Some("2025-11-25")),
        "{headers:?}"
    );
}

#[test]
fn a_server_that_insists_on_an_older_version_is_run_on_that_one() {
    let s = start(Mode::OlderProtocol);
    let home = temp_home("older-protocol-info");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-06-18"), "{}", o.stdout);

    assert_eq!(
        offered(&s),
        ["2025-11-25"],
        "the offer was still the newest"
    );
    let headers = headers_after_the_handshake(&s);
    assert!(!headers.is_empty());
    assert!(
        headers.iter().all(|h| h.as_deref() == Some("2025-06-18")),
        "{headers:?}"
    );
}

#[test]
fn the_protocol_version_flag_changes_what_is_offered() {
    let s = start(Mode::EchoProtocol);
    let home = temp_home("pinned-protocol");

    let o = run(mcpdial(&home).args(["--protocol-version", "2025-06-18", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-06-18"), "{}", o.stdout);

    // Also placed after the subcommand, as a global flag may be.
    let o = run(mcpdial(&home).args(["info", &s.url, "--protocol-version", "2025-03-26"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-03-26"), "{}", o.stdout);

    assert_eq!(offered(&s), ["2025-06-18", "2025-03-26"]);
    assert_eq!(
        headers_after_the_handshake(&s),
        [
            Some("2025-06-18".to_string()),
            Some("2025-03-26".to_string())
        ]
    );
}

#[test]
fn a_version_mcpdial_does_not_speak_cannot_be_pinned() {
    let home = temp_home("unknown-pin");
    let o = run(mcpdial(&home).args(["--protocol-version", "2024-11-05", "info", "x"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("2024-11-05"), "{}", o.stderr);
    assert!(o.stderr.contains("2025-11-25"), "{}", o.stderr);
}

#[test]
fn add_saves_the_pinned_version_and_the_flag_still_beats_it() {
    let s = start(Mode::EchoProtocol);
    let home = temp_home("saved-protocol");

    let o = run(mcpdial(&home).args([
        "add",
        "fake",
        "--http",
        &s.url,
        "--protocol-version",
        "2025-06-18",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    assert_eq!(saved["servers"]["fake"]["protocol_version"], "2025-06-18");

    let o = run(mcpdial(&home).args(["info", "fake"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-06-18"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--protocol-version", "2025-03-26", "info", "fake"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-03-26"), "{}", o.stdout);

    assert_eq!(offered(&s), ["2025-06-18", "2025-03-26"]);

    // A server saved without the flag carries no pin at all.
    let o = run(mcpdial(&home).args(["add", "plain", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    assert!(saved["servers"]["plain"].get("protocol_version").is_none());
}

#[test]
fn a_saved_version_this_build_does_not_speak_is_a_config_error() {
    let s = start(Mode::EchoProtocol);
    let home = temp_home("bad-saved-protocol");
    std::fs::write(
        home.join("servers.json"),
        format!(
            r#"{{"servers":{{"fake":{{"http":"{}","protocol_version":"1999-01-01"}}}}}}"#,
            s.url
        ),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["--json", "info", "fake"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "config");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1999-01-01"),
        "{}",
        o.stderr
    );
    assert!(s.requests.lock().unwrap().is_empty(), "nothing was sent");
}
