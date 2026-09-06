//! Protocol 2026-07-28 end to end: `server/discover` in place of `initialize`,
//! the `_meta` fields and mirrored headers on every request, the fall-back to
//! `initialize` on an earlier server, and the handshake a saved server answered
//! remembered in `probes.json`.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, FakeServer, Mode, Recorded};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

fn requests(s: &FakeServer) -> Vec<Recorded> {
    s.requests.lock().unwrap().clone()
}

/// What each request was: the JSON-RPC method of a POST, the verb of anything else.
fn methods(s: &FakeServer) -> Vec<String> {
    requests(s)
        .iter()
        .map(|r| {
            if r.method == "POST" {
                r.json()["method"].as_str().unwrap_or("?").to_string()
            } else {
                r.method.clone()
            }
        })
        .collect()
}

fn handshakes(home: &Path) -> Value {
    let text = std::fs::read_to_string(home.join("probes.json")).unwrap_or_default();
    serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)["handshakes"].clone()
}

#[test]
fn a_2026_07_28_server_is_opened_with_discover_and_nothing_else() {
    let s = start(Mode::Modern);
    let home = temp_home("discover-info");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(o.stdout.contains("protocol 2026-07-28"), "{}", o.stdout);
    let capabilities = o
        .stdout
        .lines()
        .find(|l| l.starts_with("capabilities:"))
        .unwrap_or_default();
    for each in ["tools", "resources", "prompts"] {
        assert!(capabilities.contains(each), "{}", o.stdout);
    }
    assert!(
        o.stdout.contains("Discovered, not initialized."),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        found["protocolVersion"], "2026-07-28",
        "the version settled on"
    );
    assert_eq!(
        found["serverInfo"]["name"], "fake-mcp",
        "lifted out of _meta"
    );
    assert_eq!(found["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(found["capabilities"]["tools"], json!({}));

    assert_eq!(
        methods(&s),
        ["server/discover", "server/discover"],
        "no initialize, no notification, no DELETE"
    );
    for r in requests(&s) {
        let meta = &r.json()["params"]["_meta"];
        assert_eq!(meta[META_VERSION], "2026-07-28");
        assert_eq!(
            meta["io.modelcontextprotocol/clientCapabilities"],
            json!({})
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["name"],
            "mcpdial"
        );
        assert_eq!(r.header("mcp-protocol-version"), Some("2026-07-28"));
        assert_eq!(r.header("mcp-method"), Some("server/discover"));
        assert!(r.header("mcp-name").is_none());
        assert!(r.header("mcp-session-id").is_none());
    }
}

#[test]
fn tools_call_read_and_prompt_work_end_to_end_with_the_mirrored_headers() {
    let s = start(Mode::Modern);
    let home = temp_home("discover-methods");

    let o = run(mcpdial(&home).args(["--json", "tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["tools"].as_array().unwrap().len(), 2, "both pages");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");

    let o = run(mcpdial(&home).args(["read", &s.url, "file:///readme.md"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "# fake-mcp\nA readme.\n");

    let o = run(mcpdial(&home).args(["--json", "prompt", &s.url, "summarize", r#"{"text":"x"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["messages"][0]["content"]["text"], "Summarize this: x");

    let o = run(mcpdial(&home).args(["--json", "resources", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["resources"].as_array().unwrap().len(), 2);
    assert_eq!(v["resourceTemplates"].as_array().unwrap().len(), 1);

    let o = run(mcpdial(&home).args(["--json", "prompts", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["prompts"].as_array().unwrap().len(), 2);

    let named: Vec<(String, String)> = requests(&s)
        .iter()
        .filter_map(|r| {
            let name = r.header("mcp-name")?;
            Some((
                r.json()["method"].as_str().unwrap().to_string(),
                name.to_string(),
            ))
        })
        .collect();
    assert_eq!(
        named,
        [
            ("tools/call".to_string(), "echo".to_string()),
            (
                "resources/read".to_string(),
                "file:///readme.md".to_string()
            ),
            ("prompts/get".to_string(), "summarize".to_string()),
        ],
        "Mcp-Name on exactly the requests that name something"
    );
    for r in requests(&s) {
        let msg = r.json();
        let method = msg["method"].as_str().unwrap();
        assert_eq!(
            msg["params"]["_meta"][META_VERSION], "2026-07-28",
            "{}",
            r.body
        );
        assert_eq!(r.header("mcp-protocol-version"), Some("2026-07-28"));
        assert_eq!(r.header("mcp-method"), Some(method), "{}", r.body);
        assert!(
            method != "initialize" && method != "notifications/initialized",
            "{}",
            r.body
        );
    }
    assert!(
        methods(&s).iter().all(|m| m != "DELETE"),
        "{:?}",
        methods(&s)
    );
}

#[test]
fn a_name_the_header_cannot_carry_is_base64_encoded() {
    let s = start(Mode::Modern);
    let home = temp_home("discover-base64");

    let o = run(mcpdial(&home).args(["--json", "read", &s.url, "file:///nötes.md"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(
        err["error"]["code"], -32002,
        "the server decoded the header and found it matched the body: {}",
        o.stderr
    );

    let read = requests(&s)
        .into_iter()
        .find(|r| r.json()["method"] == "resources/read")
        .unwrap();
    let header = read.header("mcp-name").unwrap();
    assert!(
        header.starts_with("=?base64?") && header.ends_with("?="),
        "{header}"
    );
    assert!(header.is_ascii());
}

#[test]
fn raw_carries_the_meta_fields_and_keeps_the_callers_own() {
    let s = start(Mode::Modern);
    let home = temp_home("discover-raw");

    let o = run(mcpdial(&home).args(["--json", "raw", &s.url, "tools/list"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["tools"][0]["name"], "echo");
    assert_eq!(
        v["resultType"], "complete",
        "the result is passed on untouched"
    );

    let o = run(mcpdial(&home).args([
        "--json",
        "raw",
        &s.url,
        "tools/list",
        r#"{"cursor":"page-2","_meta":{"io.modelcontextprotocol/logLevel":"debug"}}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["tools"][0]["name"], "add");
    let sent = requests(&s).last().unwrap().json();
    assert_eq!(sent["params"]["cursor"], "page-2");
    assert_eq!(
        sent["params"]["_meta"]["io.modelcontextprotocol/logLevel"],
        "debug"
    );
    assert_eq!(sent["params"]["_meta"][META_VERSION], "2026-07-28");
}

#[test]
fn an_earlier_server_is_probed_once_and_then_initialized() {
    let s = start(Mode::Stateless);
    let home = temp_home("discover-fallback");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2025-06-18"), "{}", o.stdout);
    assert_eq!(
        methods(&s),
        ["server/discover", "initialize", "notifications/initialized"]
    );
    let reqs = requests(&s);
    assert_eq!(
        reqs[0].header("mcp-protocol-version"),
        Some("2026-07-28"),
        "the probe says what era it is"
    );
    assert_eq!(
        reqs[1].header("mcp-protocol-version"),
        None,
        "nothing is negotiated yet on initialize"
    );
    assert!(reqs[1].json()["params"].get("_meta").is_none());
    assert_eq!(reqs[1].json()["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(reqs[2].header("mcp-protocol-version"), Some("2025-06-18"));

    // A stateful server complains about the missing session instead, under a
    // 400; that is a refusal too, and the session it then hands out is ended.
    let s = start(Mode::Stateful);
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", r#"{"message":"hi"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");
    let seen = methods(&s);
    assert_eq!(
        seen[..3],
        ["server/discover", "initialize", "notifications/initialized"]
    );
    assert!(seen.contains(&"tools/call".to_string()), "{seen:?}");
    assert_eq!(seen.last().unwrap(), "DELETE", "{seen:?}");
    assert!(requests(&s)[0].header("mcp-session-id").is_none());
}

#[test]
fn a_server_of_both_eras_is_opened_with_discover_unless_an_earlier_version_is_pinned() {
    let s = start(Mode::DualEra);
    let home = temp_home("discover-dual");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2026-07-28"), "{}", o.stdout);
    assert_eq!(methods(&s), ["server/discover"]);

    let o = run(mcpdial(&home).args(["--protocol-version", "2025-11-25", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("protocol 2025-06-18"),
        "the server's answer to initialize: {}",
        o.stdout
    );
    assert_eq!(
        methods(&s)[1..],
        ["initialize", "notifications/initialized"]
    );
    assert_eq!(
        requests(&s)[1].json()["params"]["protocolVersion"],
        "2025-11-25"
    );

    // Saved with the pin, the server is dialed the old way every time, and
    // nothing is remembered about it: a pin says nothing about the server.
    let o = run(mcpdial(&home).args([
        "add",
        "dual",
        "--http",
        &s.url,
        "--protocol-version",
        "2025-11-25",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "tools", "dual"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let since = &methods(&s)[3..];
    assert!(!since.contains(&"server/discover".to_string()), "{since:?}");
    assert_eq!(handshakes(&home), Value::Null);
}

#[test]
fn pinning_2026_07_28_on_an_earlier_server_does_not_fall_back() {
    let s = start(Mode::Stateless);
    let home = temp_home("discover-pinned-modern");

    let o =
        run(mcpdial(&home).args(["--protocol-version", "2026-07-28", "--json", "info", &s.url]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rpc");
    assert_eq!(err["error"]["code"], -32601);
    assert_eq!(methods(&s), ["server/discover"]);
}

#[test]
fn a_2026_07_28_server_wanting_another_version_is_not_mistaken_for_an_earlier_one() {
    let s = start(Mode::ModernFromTheFuture);
    let home = temp_home("discover-future");

    let o = run(mcpdial(&home).args(["--json", "info", &s.url]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "transport", "{}", o.stderr);
    let message = err["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("2099-01-01") && message.contains("2026-07-28"),
        "{message}"
    );
    assert_eq!(methods(&s), ["server/discover"], "no initialize");
}

#[test]
fn the_handshake_a_saved_server_answered_is_remembered() {
    let old = start(Mode::Stateless);
    let new = start(Mode::Modern);
    let home = temp_home("discover-remembered");
    let o = run(mcpdial(&home).args(["add", "old", "--http", &old.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["add", "new", "--http", &new.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["--json", "tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(methods(&old)[..2], ["server/discover", "initialize"]);
    assert_eq!(handshakes(&home)["old"]["handshake"], "initialize");

    let dialed = methods(&old).len();
    let o = run(mcpdial(&home).args(["--json", "tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        methods(&old)[dialed..][..2],
        ["initialize", "notifications/initialized"],
        "no probe the second time"
    );

    let o = run(mcpdial(&home).args(["info", "new"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(handshakes(&home)["new"]["handshake"], "server/discover");

    let dialed = methods(&old).len();
    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row["status"]["state"], "connected", "{}", o.stdout);
    }
    let since = &methods(&old)[dialed..];
    assert!(
        !since.contains(&"server/discover".to_string()),
        "ls knew better too: {since:?}"
    );
    assert_eq!(handshakes(&home)["old"]["handshake"], "initialize");

    // An entry edited is a different server, and is asked again.
    let path = home.join("servers.json");
    let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    saved["servers"]["old"]["headers"] = json!({"X-Test": "1"});
    std::fs::write(&path, saved.to_string()).unwrap();
    let dialed = methods(&old).len();
    let o = run(mcpdial(&home).args(["--json", "tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(methods(&old)[dialed], "server/discover");

    // Nothing is remembered for a server that was never saved.
    let o = run(mcpdial(&home).args(["--json", "tools", &old.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(handshakes(&home).as_object().unwrap().len(), 2);
}

#[test]
fn a_remembered_handshake_the_server_no_longer_answers_is_found_out_again() {
    let s = start(Mode::Stateless);
    let home = temp_home("discover-upgraded");
    let o = run(mcpdial(&home).args(["add", "up", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "tools", "up"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(handshakes(&home)["up"]["handshake"], "initialize");

    s.switch_to(Mode::Modern);
    let dialed = methods(&s).len();
    let o = run(mcpdial(&home).args(["info", "up"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("protocol 2026-07-28"), "{}", o.stdout);
    assert_eq!(
        methods(&s)[dialed..],
        ["initialize", "server/discover"],
        "initialize refused under a 404 with a JSON-RPC body, so no look for the older transport"
    );
    assert_eq!(handshakes(&home)["up"]["handshake"], "server/discover");
}

#[test]
fn the_shell_runs_on_a_2026_07_28_server() {
    let s = start(Mode::Modern);
    let home = temp_home("discover-shell");

    let mut child = mcpdial(&home)
        .args(["--json", "shell", &s.url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"info\ntools\ncall echo {\"message\":\"hi\"}\nquit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `info` is printed pretty, over several lines, so read values rather than lines.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<Value> = serde_json::Deserializer::from_str(&stdout)
        .into_iter::<Value>()
        .map(|v| v.unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert_eq!(lines[0]["protocolVersion"], "2026-07-28");
    assert_eq!(lines[0]["serverInfo"]["name"], "fake-mcp");
    assert_eq!(lines[1]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(lines[2]["content"][0]["text"], "Echo: hi");
    assert!(
        !methods(&s).contains(&"DELETE".to_string()),
        "{:?}",
        methods(&s)
    );
}

#[test]
fn a_stdio_server_is_probed_with_discover_before_initialize() {
    let home = temp_home("discover-stdio");
    let target = format!("stdio:{}", echo_command());

    let o = run(mcpdial(&home).args(["-v", "info", &target]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("echo-server"), "{}", o.stdout);
    let discover = o
        .stderr
        .find("\"method\":\"server/discover\"")
        .expect("the probe was sent");
    let initialize = o
        .stderr
        .find("\"method\":\"initialize\"")
        .expect("then the handshake");
    assert!(discover < initialize, "{}", o.stderr);
}

#[test]
fn a_2026_07_28_stdio_server_is_opened_with_discover_and_nothing_else() {
    let home = temp_home("discover-stdio-modern");
    let target = format!("stdio:{}", echo_command());

    let o = run(mcpdial(&home)
        .env("ECHO_SERVER_MODERN", "1")
        .args(["-v", "--json", "info", &target]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(found["protocolVersion"], "2026-07-28");
    assert_eq!(found["serverInfo"]["name"], "echo-server");
    assert!(
        !o.stderr.contains("\"method\":\"initialize\""),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).env("ECHO_SERVER_MODERN", "1").args([
        "call",
        &target,
        "echo",
        r#"{"message":"hi"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: hi");
}

/// A config directory short enough that `run/NAME.sock` fits a Unix socket
/// path, which macOS caps at 104 bytes.
#[cfg(unix)]
fn short_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("md-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn a_daemon_answers_the_handshake_it_opened_with() {
    let home = short_home("dsc");
    let echo = echo_command();
    let o = run(mcpdial(&home).args([
        "add",
        "modern",
        "--stdio",
        &echo,
        "--env",
        "ECHO_SERVER_MODERN=1",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["start", "modern", "--idle", "120"]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    // Nothing is remembered yet, so the caller probes: the daemon answers
    // `server/discover` itself, and the server never sees an `initialize`.
    let o = run(mcpdial(&home).args(["-v", "--json", "info", "modern"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(found["protocolVersion"], "2026-07-28");
    assert_eq!(found["serverInfo"]["name"], "echo-server");
    assert_eq!(handshakes(&home)["modern"]["handshake"], "server/discover");

    let o = run(mcpdial(&home).args(["call", "modern", "count"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=1");
    let o = run(mcpdial(&home).args(["call", "modern", "count"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "count=2", "the same process served both");

    let o = run(mcpdial(&home).args(["stop", "modern"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}
