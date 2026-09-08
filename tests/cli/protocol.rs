//! Which revision is spoken, and how a server that speaks an older one is met.

use crate::common::{mcpdial, run, start, temp_home, timed, Mode};
use serde_json::Value;

#[test]
fn the_protocol_version_header_rides_every_request_after_initialize() {
    let s = start(Mode::Stateless);
    let home = temp_home("protocol-version");

    let o = run(mcpdial(&home).args(["tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let reqs = s.requests.lock().unwrap();
    assert!(
        reqs.iter().all(|r| r.header("mcp-session-id").is_none()),
        "a stateless server hands out no session id"
    );
    for r in reqs.iter() {
        let msg = r.json();
        let method = msg["method"].as_str().unwrap_or_default();
        let sent = r.header("mcp-protocol-version");
        match method {
            "initialize" => assert_eq!(sent, None, "nothing is negotiated yet on initialize"),
            // The request that works out which era this server speaks says which
            // one it is asking as, since that revision has no handshake to say it at.
            "server/discover" => assert_eq!(sent, Some("2026-07-28")),
            _ => assert_eq!(sent, Some("2025-06-18"), "missing on {method}"),
        }
    }
    for method in ["tools/list", "tools/call"] {
        assert!(
            reqs.iter().any(|r| r.json()["method"] == method),
            "{method} never reached the server"
        );
    }
}

#[test]
fn the_version_on_the_wire_is_the_one_the_server_agreed_to() {
    let s = start(Mode::OlderProtocol);
    let home = temp_home("older-protocol");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let reqs = s.requests.lock().unwrap();
    let handshake = reqs
        .iter()
        .position(|r| r.json()["method"] == "initialize")
        .expect("a server without server/discover is handed the handshake");
    assert_eq!(
        reqs[handshake].json()["params"]["protocolVersion"],
        "2025-11-25"
    );
    let after_the_handshake = &reqs[handshake + 1..];
    assert!(!after_the_handshake.is_empty());
    for r in after_the_handshake {
        assert_eq!(
            r.header("mcp-protocol-version"),
            Some("2025-06-18"),
            "{}",
            r.body
        );
    }
}

#[test]
fn a_2024_11_05_server_is_named_rather_than_reported_as_a_bare_status() {
    let s = start(Mode::LegacySse);
    let home = temp_home("legacy-sse");
    assert_eq!(
        run(mcpdial(&home).args(["add", "old", "--http", &s.url])).code,
        0
    );

    // No --timeout: a probe that read the stream to its end would sit here for the
    // default 60s instead, and the server never ends it.
    let (took, o) = timed(mcpdial(&home).args(["info", "old"]));
    assert!(
        took < std::time::Duration::from_secs(10),
        "the probe waited out a stream that never ends, {took:?}"
    );
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("HTTP+SSE"), "{}", o.stderr);
    assert!(o.stderr.contains("2024-11-05"), "{}", o.stderr);
    assert!(!o.stderr.contains("HTTP 405"), "{}", o.stderr);

    let probed = s.requests.lock().unwrap().iter().any(|r| r.method == "GET");
    assert!(probed, "the failed POST should have been followed by a GET");

    let o = run(mcpdial(&home).args(["ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("legacy sse"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "ls"]));
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "legacy_sse");
}

#[test]
fn a_server_that_answers_is_never_probed_for_the_older_transport() {
    let s = start(Mode::Stateful);
    let home = temp_home("no-legacy-probe");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let requests = s.requests.lock().unwrap();
    assert!(
        requests.iter().all(|r| r.method != "GET"),
        "a healthy server costs no extra round trip: {:?}",
        requests.iter().map(|r| &r.method).collect::<Vec<_>>()
    );
}
