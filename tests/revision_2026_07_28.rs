//! Revision 2026-07-28 end to end: which era a server speaks is worked out from
//! how it answers `server/discover`, and on a server from that revision every
//! request carries its own metadata and mirrors it into the headers.

mod common;

use common::{mcpdial, run, start, temp_home, FakeServer, Mode, MODERN_VERSION};
use serde_json::Value;

fn requests(s: &FakeServer) -> Vec<Value> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .collect()
}

fn methods(s: &FakeServer) -> Vec<String> {
    requests(s)
        .iter()
        .filter_map(|m| Some(m["method"].as_str()?.to_string()))
        .collect()
}

/// The `Mcp-Name` header on the one request for `method`, decoded if it travelled
/// base64'd behind the sentinel.
fn named_in_the_header(s: &FakeServer, method: &str) -> Option<String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let sent = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.json()["method"] == method)?
        .header("mcp-name")?
        .to_string();
    match sent
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        Some(encoded) => Some(String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap()),
        None => Some(sent),
    }
}

#[test]
fn discovery_stands_in_for_the_handshake_and_info_reads_the_same() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-info");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("modern-mcp 2.0"), "{}", o.stdout);
    assert!(o.stdout.contains("protocol 2026-07-28"), "{}", o.stdout);
    assert!(
        o.stdout.contains("capabilities: prompts, resources, tools"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("Echoes what you give it."),
        "{}",
        o.stdout
    );
    assert_eq!(
        methods(&s),
        ["server/discover"],
        "one request, no handshake"
    );

    let o = run(mcpdial(&home).args(["--json", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let info: Value = serde_json::from_str(&o.stdout).unwrap();
    // The keys `info --json` has always had, so nothing reading it has to change.
    assert_eq!(info["protocolVersion"], MODERN_VERSION);
    assert_eq!(info["serverInfo"]["name"], "modern-mcp");
    assert!(info["capabilities"]["tools"].is_object());
    // And what the revision added, where the server put it.
    assert_eq!(info["supportedVersions"][0], MODERN_VERSION);
    assert_eq!(info["resultType"], "complete");
}

#[test]
fn tools_call_read_and_prompt_all_work_against_the_new_revision() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-commands");
    let at = |args: &[&str]| {
        let o = run(mcpdial(&home).args(args));
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        o
    };

    let o = at(&["tools", &s.url]);
    assert!(
        o.stdout.contains("echo") && o.stdout.contains("add"),
        "{}",
        o.stdout
    );

    let o = at(&["call", &s.url, "echo", r#"{"message":"hi"}"#]);
    assert_eq!(o.stdout.trim(), "Echo: hi");

    let o = at(&["resources", &s.url]);
    assert!(o.stdout.contains("file:///readme.md"), "{}", o.stdout);

    let o = at(&["read", &s.url, "file:///readme.md"]);
    assert!(o.stdout.contains("A readme."), "{}", o.stdout);

    let o = at(&["prompt", &s.url, "summarize", r#"{"text":"a book"}"#]);
    assert!(o.stdout.contains("Summarize this: a book"), "{}", o.stdout);

    // The fake server refuses any request whose headers and body disagree, so
    // reaching this line at all is the proof that they never did.
    assert_eq!(
        named_in_the_header(&s, "tools/call").as_deref(),
        Some("echo")
    );
    assert_eq!(
        named_in_the_header(&s, "resources/read").as_deref(),
        Some("file:///readme.md")
    );
    assert_eq!(
        named_in_the_header(&s, "prompts/get").as_deref(),
        Some("summarize")
    );
}

#[test]
fn every_request_carries_the_metadata_that_replaced_the_handshake() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-meta");
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let sent = s.requests.lock().unwrap();
    assert!(sent.len() >= 2, "discovery, then the call");
    for r in sent.iter() {
        let meta = &r.json()["params"]["_meta"];
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            MODERN_VERSION
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["name"],
            "mcpdial"
        );
        assert!(
            meta["io.modelcontextprotocol/clientCapabilities"].is_object(),
            "{}",
            r.body
        );
        assert_eq!(r.header("mcp-method"), r.json()["method"].as_str());
        assert_eq!(r.header("mcp-protocol-version"), Some(MODERN_VERSION));
    }
    assert!(
        sent.iter().all(|r| r.method != "DELETE"),
        "a revision with no sessions has none to terminate"
    );
    assert!(
        sent.iter().all(|r| r.header("mcp-session-id").is_none()),
        "and none to echo"
    );
}

#[test]
fn a_name_that_will_not_fit_a_header_travels_encoded() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-encoded-name");
    let unicode = "file:///notes/世界.md";

    // The resource does not exist; what is under test is that the request reached
    // the server's own lookup rather than being refused over its headers.
    let o = run(mcpdial(&home).args(["read", &s.url, unicode]));
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("Resource not found"), "{}", o.stderr);
    assert_eq!(
        named_in_the_header(&s, "resources/read").as_deref(),
        Some(unicode)
    );
    let raw = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.json()["method"] == "resources/read")
        .unwrap()
        .header("mcp-name")
        .unwrap()
        .to_string();
    assert!(raw.starts_with("=?base64?"), "{raw}");
}

#[test]
fn raw_reaches_the_new_revision_without_spelling_out_the_metadata() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-raw");

    let o = run(mcpdial(&home).args(["raw", &s.url, "tools/list"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let result: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(result["tools"][0]["name"], "echo");

    // A `_meta` key of the caller's own survives beside the three required ones.
    let o = run(mcpdial(&home).args([
        "raw",
        &s.url,
        "tools/list",
        r#"{"_meta":{"progressToken":7}}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = requests(&s)
        .into_iter()
        .rfind(|m| m["method"] == "tools/list")
        .unwrap();
    assert_eq!(asked["params"]["_meta"]["progressToken"], 7);
    assert_eq!(
        asked["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        MODERN_VERSION
    );
}

#[test]
fn ls_reports_a_server_of_the_new_revision_like_any_other() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-ls");
    let o = run(mcpdial(&home).args(["add", "new", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "connected");
    assert_eq!(rows[0]["server"], "modern-mcp 2.0");
    assert_eq!(rows[0]["tools"], 2);
}

#[test]
fn a_server_that_never_heard_of_discovery_is_handed_the_handshake() {
    let s = start(Mode::Stateless);
    let home = temp_home("fallback-stateless");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");
    assert_eq!(
        &methods(&s)[..3],
        ["server/discover", "initialize", "notifications/initialized"]
    );
    assert!(
        requests(&s)
            .iter()
            .filter(|m| m["method"] != "server/discover")
            .all(|m| m["params"]["_meta"].is_null()),
        "an older server is sent nothing it was never told about"
    );
}

#[test]
fn a_stateful_server_complaining_about_a_session_is_still_only_an_older_one() {
    let s = start(Mode::Stateful);
    let home = temp_home("fallback-stateful");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");
    assert_eq!(methods(&s)[0], "server/discover");
    assert!(
        s.requests.lock().unwrap().iter().all(|r| r.method != "GET"),
        "the era probe is not worth a hunt for the transport MCP retired"
    );
}

#[test]
fn a_server_that_names_what_it_speaks_is_met_there() {
    let s = start(Mode::DualEra);
    let home = temp_home("dual-era");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("protocol 2025-11-25"),
        "the newest it named that has a handshake: {}",
        o.stdout
    );
    assert_eq!(
        methods(&s),
        ["server/discover", "initialize", "notifications/initialized"]
    );
}

#[test]
fn pinning_an_older_revision_asks_for_the_handshake_and_nothing_else() {
    let s = start(Mode::Stateless);
    let home = temp_home("pinned-older");

    let o = run(mcpdial(&home).args(["--protocol-version", "2025-11-25", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        methods(&s),
        ["initialize", "notifications/initialized"],
        "a named revision is not worth working out"
    );
}

#[test]
fn pinning_the_new_revision_holds_an_older_server_to_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("pinned-newer");

    let o = run(mcpdial(&home).args(["--protocol-version", MODERN_VERSION, "info", &s.url]));
    assert_ne!(o.code, 0, "{}", o.stdout);
    assert!(o.stderr.contains("-32601"), "{}", o.stderr);
    assert_eq!(
        methods(&s),
        ["server/discover"],
        "no handshake behind the pin"
    );
}

#[test]
fn the_new_revision_can_be_saved_with_a_server() {
    let s = start(Mode::Modern);
    let home = temp_home("modern-saved");

    let o = run(mcpdial(&home).args([
        "add",
        "new",
        "--http",
        &s.url,
        "--protocol-version",
        MODERN_VERSION,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    assert_eq!(saved["servers"]["new"]["protocol_version"], MODERN_VERSION);

    let o = run(mcpdial(&home).args(["info", "new"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2026-07-28"), "{}", o.stdout);
}
