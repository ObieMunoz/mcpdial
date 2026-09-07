mod common;

use common::{
    echo_command, echo_server, mcpdial, run, start, temp_home, timed, Iss, Mode, Out,
    CONFIDENTIAL_ID, CONFIDENTIAL_SECRET as SECRET,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn stateful_http_handshake_session_and_call() {
    let s = start(Mode::Stateful);
    let home = temp_home("stateful");

    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp 1.0"), "{}", o.stdout);
    assert!(
        o.stdout.contains("capabilities: prompts, resources, tools"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"));
    assert!(o.stdout.contains("echo") && o.stdout.contains("Echo a message back."));
    assert!(
        !o.stdout.contains("Second line"),
        "short listing shows the first line only"
    );

    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":40,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 40 and 2 is 42.");

    // What actually went over the wire for that last call.
    let reqs = s.requests.lock().unwrap();
    let last4 = &reqs[reqs.len() - 4..];
    assert_eq!(last4[0].json()["method"], "initialize");
    assert!(
        last4[0]
            .header("user-agent")
            .unwrap()
            .starts_with("Mozilla/5.0"),
        "browser UA is mandatory"
    );
    assert_eq!(
        last4[0].header("accept").unwrap(),
        "application/json, text/event-stream"
    );
    assert!(last4[0].header("mcp-session-id").is_none());
    assert_eq!(last4[1].json()["method"], "notifications/initialized");
    assert!(last4[1].json().get("id").is_none());
    assert_eq!(
        last4[1].header("mcp-session-id"),
        Some("sess-1"),
        "session id is echoed back"
    );
    assert_eq!(last4[2].json()["method"], "tools/call");
    assert_eq!(last4[2].header("mcp-protocol-version"), Some("2025-06-18"));
    assert_eq!(
        last4[3].method, "DELETE",
        "one-shot commands end the session"
    );
    assert_eq!(last4[3].header("mcp-session-id"), Some("sess-1"));
}

#[test]
fn stateless_http_and_json_output() {
    let s = start(Mode::Stateless);
    let home = temp_home("stateless");
    let o = run(mcpdial(&home).args([
        "--json",
        "call",
        &s.url,
        "echo",
        r#"{"message":"plain json"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], "Echo: plain json");

    let o = run(mcpdial(&home).args(["--json", "tools", &s.url]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);

    let o = run(mcpdial(&home).args(["raw", &s.url, "tools/list"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("\"name\": \"echo\""));
    assert!(o.stdout.contains("\"nextCursor\""), "{}", o.stdout);
}

#[test]
fn a_json_listing_is_names_and_one_line_until_long_asks_for_the_rest() {
    let s = start(Mode::Stateless);
    let home = temp_home("json-listing");

    let o = run(mcpdial(&home).args(["--json", "tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        v["tools"],
        json!([
            {"name": "echo", "description": "Echo a message back."},
            {"name": "add", "description": "Add two numbers."},
        ]),
        "no inputSchema, no outputSchema, no annotations: {}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "tools", &s.url, "--long"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        v["tools"][0]["description"],
        "Echo a message back.\nSecond line."
    );
    assert_eq!(v["tools"][0]["inputSchema"]["required"][0], "message");
    assert_eq!(v["tools"][1]["outputSchema"]["required"][0], "sum");

    // The one tool an agent settles on comes back whole with no flag at all.
    let o = run(mcpdial(&home).args(["--json", "schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["inputSchema"]["required"], json!(["a", "b"]));

    let o = run(mcpdial(&home).args(["--json", "prompts", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["prompts"][0]["name"], "summarize");
    assert!(v["prompts"][0].get("arguments").is_none(), "{}", o.stdout);
    let o = run(mcpdial(&home).args(["--json", "prompts", &s.url, "--long"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["prompts"][0]["arguments"][0]["name"], "text");

    let o = run(mcpdial(&home).args(["--json", "resources", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["resources"][0]["uri"], "file:///readme.md");
    assert_eq!(v["resources"][0]["description"], "The project readme.");
    assert!(v["resources"][0].get("mimeType").is_none(), "{}", o.stdout);
    assert_eq!(
        v["resourceTemplates"][0]["uriTemplate"],
        "file:///notes/{name}.md"
    );
    let o = run(mcpdial(&home).args(["--json", "resources", &s.url, "--long"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["resources"][0]["mimeType"], "text/markdown");

    let o = run(mcpdial(&home).args(["guide"]));
    assert!(
        o.stdout
            .contains("Two steps: list, then fetch the one you will call."),
        "the guide spells out the two-step: {}",
        o.stdout
    );
}

#[test]
fn tool_lists_are_merged_across_pages() {
    let s = start(Mode::Stateless);
    let home = temp_home("paged");
    assert_eq!(
        run(mcpdial(&home).args(["add", "paged", "--http", &s.url, "--no-probe"])).code,
        0
    );

    let o = run(mcpdial(&home).args(["tools", "paged"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("echo") && o.stdout.contains("add"),
        "both pages: {}",
        o.stdout
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "tools/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "page-2");

    let o = run(mcpdial(&home).args(["--json", "tools", "paged"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo", "add"]);

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["tools"], 2, "{}", o.stdout);

    let tool_from_the_second_page = "add";
    let o = run(mcpdial(&home).args(["schema", "paged", tool_from_the_second_page]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args([
        "call",
        "paged",
        tool_from_the_second_page,
        r#"{"a":1,"b":2}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
}

#[test]
fn a_repeating_cursor_stops_instead_of_looping() {
    let s = start(Mode::StuckCursor);
    let home = temp_home("stuck");

    let o = run(mcpdial(&home).args(["--timeout", "5", "tools", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 tool(s):"), "{}", o.stdout);

    let pages = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.json()["method"] == "tools/list")
        .count();
    assert_eq!(pages, 2, "stopped the moment the cursor repeated");
}

#[test]
fn errors_map_to_exit_codes() {
    let s = start(Mode::Stateless);
    let home = temp_home("errors");

    let o = run(mcpdial(&home).args(["call", &s.url, "fail"]));
    assert_eq!(o.code, 1, "isError result exits 1");
    assert_eq!(o.stdout.trim(), "it failed");

    let o = run(mcpdial(&home).args(["call", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("MCP error -32602: Tool nope not found"),
        "{}",
        o.stderr
    );

    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "{not json"]));
    assert_eq!(o.code, 2, "bad JSON is a usage error, nothing sent");
    assert_eq!(s.requests.lock().unwrap().len(), before, "nothing was sent");

    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "[1,2]"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("must be a JSON object"));

    // An unquoted {"message":"hi"} reaches us as {message:hi}: name the cause
    // and answer with the command line that would have worked.
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "{message:hi}"]));
    assert_eq!(o.code, 2);
    assert!(
        o.stderr.contains(r#"did you mean {"message": "hi"}?"#),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains(&format!(
            "mcpdial call {} echo '{{\"message\": \"hi\"}}'",
            s.url
        )),
        "{}",
        o.stderr
    );

    // With two keys the shell splits the object at the comma into two words,
    // which are not key=value pairs either.
    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "message:hi", "n:2"]));
    assert_eq!(o.code, 2);
    assert_eq!(s.requests.lock().unwrap().len(), before, "nothing was sent");
    assert!(
        o.stderr.contains(r#""message:hi" is neither"#)
            && o.stderr.contains("split a JSON object at its commas"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["info", "no-such-server"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("unknown server"));

    let o = run(mcpdial(&home).args(["--timeout", "2", "info", "http://127.0.0.1:1/mcp"]));
    assert_eq!(o.code, 1);
    // Refused on unix; a Windows host drops it instead, which arrives as a timeout.
    let nothing_answered =
        o.stderr.contains("could not reach") || o.stderr.contains("no reply from");
    assert!(nothing_answered, "{}", o.stderr);
}

#[test]
fn blocked_403_is_not_blamed_on_the_token() {
    let s = start(Mode::Blocked);
    let home = temp_home("blocked");
    let o = run(mcpdial(&home).args(["info", &s.url]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("HTTP 403"));
    assert!(o.stderr.contains("policy/edge block"), "{}", o.stderr);
    assert!(!o.stderr.contains("login"), "must not suggest a credential");
}

#[test]
fn stdio_adhoc_call_and_timeout() {
    let home = temp_home("stdio");
    let target = format!("stdio:{}", echo_command());

    let o = run(mcpdial(&home).args(["call", &target, "echo", r#"{"message":"over a pipe"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: over a pipe");

    let o = run(mcpdial(&home).args(["info", &target]));
    assert!(o.stdout.contains("echo-server 0.0.1"));

    let o = run(mcpdial(&home).args(["call", &target, "fail"]));
    assert_eq!(o.code, 1);

    let o =
        run(mcpdial(&home)
            .env("ECHO_SERVER_HANG", "1")
            .args(["--timeout", "1", "info", &target]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("no reply after"), "{}", o.stderr);

    // A server that dies on startup explains itself: exit status plus its stderr.
    let dies_on_startup = if cfg!(windows) {
        "stdio:cmd /C 'echo npm error 404 Not Found 1>&2 & exit 3'"
    } else {
        "stdio:/bin/sh -c 'echo npm error 404 Not Found >&2; exit 3'"
    };
    let o = run(mcpdial(&home).args(["info", dies_on_startup]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("exited with status 3"), "{}", o.stderr);
    assert!(
        o.stderr.contains("| npm error 404 Not Found"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "--timeout", "3", "ls"]));
    let _ = o; // ls is covered elsewhere; this just proves nothing hangs after a dead server

    let o = run(mcpdial(&home).args(["info", "stdio:/definitely/not/a/program"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("could not start"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["login", &target]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("stdio servers need no token"));
}

#[test]
fn environment_defaults_stand_in_for_the_global_flags() {
    let s = start(Mode::Stateless);
    let home = temp_home("env-defaults");
    let o = run(mcpdial(&home).args(["add", "web", "--http", &s.url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let listing = ["ls", "--no-probe"];

    // Neither flag nor variable is the readable summary it has always been.
    let o = run(mcpdial(&home).args(listing));
    assert!(!o.stdout.starts_with('['), "{}", o.stdout);
    // The variable is the flag.
    let o = run(mcpdial(&home).env("MCPDIAL_JSON", "1").args(listing));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value = serde_json::from_str(&o.stdout).expect(&o.stdout);
    assert_eq!(saved[0]["name"], "web");
    // `0` turns it off, as it does for MCPDIAL_PLAIN, and the flag still wins.
    let o = run(mcpdial(&home).env("MCPDIAL_JSON", "0").args(listing));
    assert!(!o.stdout.starts_with('['), "{}", o.stdout);
    let o = run(mcpdial(&home)
        .env("MCPDIAL_JSON", "0")
        .args(["--json", "ls", "--no-probe"]));
    assert!(o.stdout.starts_with('['), "{}", o.stdout);

    // MCPDIAL_TIMEOUT bounds a server that never answers, and the flag beats it:
    // the seconds in the message say which of the two was used.
    let stuck = format!("stdio:{}", echo_command());
    let o = run(mcpdial(&home)
        .env("ECHO_SERVER_HANG", "1")
        .env("MCPDIAL_TIMEOUT", "0.5")
        .args(["info", &stuck]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("no reply after 0.5s"), "{}", o.stderr);
    let o = run(mcpdial(&home)
        .env("ECHO_SERVER_HANG", "1")
        .env("MCPDIAL_TIMEOUT", "1800")
        .args(["--timeout", "1", "info", &stuck]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("no reply after 1s"), "{}", o.stderr);

    // A timeout that is not seconds is refused by name rather than ignored,
    // before anything is dialed, and in whichever shape was asked for.
    let o = run(mcpdial(&home).env("MCPDIAL_TIMEOUT", "30s").args(listing));
    assert_eq!(o.code, 2, "{}", o.stdout);
    assert!(
        o.stderr
            .contains(r#"MCPDIAL_TIMEOUT must be a non-negative number of seconds, got "30s""#),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home)
        .env("MCPDIAL_TIMEOUT", "30s")
        .env("MCPDIAL_JSON", "1")
        .args(listing));
    assert_eq!(o.code, 2, "{}", o.stdout);
    let failed: Value = serde_json::from_str(&o.stderr).expect(&o.stderr);
    assert_eq!(failed["error"]["kind"], "usage");

    // MCPDIAL_USER_AGENT rides the request the way --user-agent does.
    let o = run(mcpdial(&home)
        .env("MCPDIAL_USER_AGENT", "probe/1")
        .args(["info", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let reqs = s.requests.lock().unwrap();
    assert_eq!(reqs.last().unwrap().header("user-agent"), Some("probe/1"));
}

#[test]
fn saved_servers_and_status_listing() {
    let http = start(Mode::Stateful);
    let auth = start(Mode::Auth {
        tokens: vec!["secret".into()],
    });
    let blocked = start(Mode::Blocked);
    let home = temp_home("ls");
    let echo = echo_command();

    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &http.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "local", "--stdio", &echo])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "locked", "--http", &auth.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "wall", "--http", &blocked.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "dead", "--http", "http://127.0.0.1:1/mcp"])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args([
            "add",
            "envtok",
            "--http",
            &auth.url,
            "--token-env",
            "FAKE_TOKEN",
            "--no-probe"
        ]))
        .code,
        0
    );
    let o = run(mcpdial(&home).args(["add", "bad name", "--http", "x"]));
    assert_eq!(o.code, 2);
    let o = run(mcpdial(&home).args(["add", "both", "--http", "x", "--stdio", "y"]));
    assert_ne!(o.code, 0);

    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("web") && o.stdout.contains("local") && o.stdout.contains("stdio"));
    assert!(o.stdout.contains("$FAKE_TOKEN"));

    let o =
        run(mcpdial(&home)
            .env("FAKE_TOKEN", "secret")
            .args(["--timeout", "3", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let by_name = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap().clone();
    assert_eq!(by_name("web")["status"]["state"], "connected");
    assert_eq!(by_name("web")["server"], "fake-mcp 1.0");
    assert_eq!(by_name("web")["tools"], 2);
    assert_eq!(by_name("local")["status"]["state"], "connected");
    assert_eq!(by_name("local")["server"], "echo-server 0.0.1");
    assert_eq!(by_name("locked")["status"]["state"], "auth_required");
    assert_eq!(by_name("wall")["status"]["state"], "blocked");
    assert_eq!(by_name("dead")["status"]["state"], "unreachable");
    assert_eq!(by_name("envtok")["status"]["state"], "connected");
    assert_eq!(by_name("envtok")["auth"], "env");

    // The AUTH column names the variable a token is read from, in a row that
    // was dialed as much as in one that was only remembered, so that a dialed
    // `ls` and `ls --no-probe` say the same thing about the same server.
    for flags in [
        &["--timeout", "3", "ls"][..],
        &["--timeout", "3", "ls", "--refresh"][..],
    ] {
        let o = run(mcpdial(&home).env("FAKE_TOKEN", "secret").args(flags));
        assert!(o.stdout.contains("$FAKE_TOKEN"), "{flags:?}: {}", o.stdout);
    }

    // A different $FAKE_TOKEN is not something the saved status can know about.
    let o =
        run(mcpdial(&home)
            .env("FAKE_TOKEN", "wrong")
            .args(["--timeout", "3", "ls", "--refresh"]));
    assert!(o.stdout.contains("token rejected"), "{}", o.stdout);
    assert!(o.stdout.contains("auth required"));
    assert!(o.stdout.contains("blocked (403)"));
    assert!(o.stdout.contains("unreachable"));

    // Every server's tools at once.
    let o = run(mcpdial(&home)
        .env("FAKE_TOKEN", "secret")
        .args(["--timeout", "3", "tools"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("## web  fake-mcp 1.0  (2 tools)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("## local  echo-server 0.0.1  (5 tools)"));
    assert!(o.stdout.contains("## locked  auth required"));
    assert!(o.stdout.contains("## dead  unreachable:"));

    let o = run(mcpdial(&home).args(["tools", "web", "--long"]));
    assert!(
        o.stdout.contains("Second line"),
        "long listing shows the full description"
    );
    assert!(
        o.stdout
            .contains("message: string (required) - What to echo"),
        "{}",
        o.stdout
    );

    assert_eq!(run(mcpdial(&home).args(["rm", "dead"])).code, 0);
    assert_eq!(run(mcpdial(&home).args(["rm", "dead"])).code, 2);
    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert!(!o.stdout.contains("dead"));
}

/// A tool's annotations are how a server says a call cannot be taken back, and
/// they are no use buried in the raw JSON. Every listing that names the tool has
/// to carry them: the long one spelled out, the short one as a mark on the name,
/// and the block a refused call prints.
#[test]
fn what_a_tool_says_about_itself_reaches_every_listing_that_names_it() {
    let home = temp_home("annotations");
    let o = run(mcpdial(&home).args(["add", "erasers", "--stdio", &echo_command(), "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let annotated = || {
        let mut c = mcpdial(&home);
        c.env("ECHO_SERVER_ANNOTATED", "1");
        c
    };

    let o = run(annotated().args(["tools", "erasers", "--long"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout
            .contains(r#"erase  "Erase a file"  [destructive] [open-world] [task:optional]"#),
        "{}",
        o.stdout
    );
    // A tool the server annotated with nothing is tagged with nothing.
    assert!(o.stdout.contains("\necho\n"), "{}", o.stdout);

    // The short listing has room for the one hint that matters most.
    let o = run(annotated().args(["tools", "erasers"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("erase*"), "{}", o.stdout);
    assert!(!o.stdout.contains("echo*"), "{}", o.stdout);

    // The same tags where a caller looks the tool up, with stdout still the
    // tool object and nothing else.
    let o = run(annotated().args(["schema", "erasers", "erase"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains(r#"erase  "Erase a file"  [destructive]"#),
        "{}",
        o.stderr
    );
    let object: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(object["annotations"]["destructiveHint"], true);
}

/// A program reading `ls --json` must not have to know which flag produced it,
/// and every `--json` document is an object.
#[test]
fn both_ls_shapes_carry_the_configuration_and_tools_prints_an_object() {
    let s = start(Mode::Stateless);
    let home = temp_home("json-shapes");
    assert_eq!(
        run(mcpdial(&home).args([
            "add",
            "web",
            "--http",
            &s.url,
            "--deny",
            "add",
            "--no-probe"
        ]))
        .code,
        0
    );
    let configuration = [
        "name",
        "kind",
        "location",
        "headers",
        "token_env",
        "credential",
        "source",
        "timeout",
        "running",
        "allow",
        "deny",
    ];

    let o = run(mcpdial(&home).args(["--json", "ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    for key in configuration {
        assert!(
            saved[0].get(key).is_some(),
            "{key} is missing: {}",
            o.stdout
        );
    }
    for key in ["status", "auth", "server", "checked_at", "age_seconds"] {
        assert!(
            saved[0].get(key).is_none(),
            "--no-probe omits the status fields rather than swapping the shape: {}",
            o.stdout
        );
    }

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let probed: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    for key in configuration {
        assert_eq!(probed[0][key], saved[0][key], "{key}: {}", o.stdout);
    }
    assert_eq!(probed[0]["status"]["state"], "connected", "{}", o.stdout);
    assert_eq!(
        probed[0]["tools"], 1,
        "the deny list hides add: {}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "tools"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let every: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(every["servers"][0]["name"], "web", "{}", o.stdout);
    assert_eq!(every["servers"][0]["tools"][0]["name"], "echo");
    assert_eq!(every["servers"].as_array().unwrap().len(), 1);
}

/// Run `login` with no browser, scrape the auth URL from stderr, and play the browser
/// ourselves: the fake authorization server 302s straight back to the loopback callback.
fn drive_login(home: &std::path::Path, target: &str, extra: &[&str]) -> String {
    drive_login_out(home, target, extra).0
}

/// [`drive_login`], handing back stderr and then whatever went to stdout.
fn drive_login_out(home: &std::path::Path, target: &str, extra: &[&str]) -> (String, String) {
    let mut child = mcpdial(home)
        .args(["login", target, "--no-browser"])
        .args(extra)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep draining stderr for the life of the process, or its next progress line
    // hits a closed pipe. Hand the auth URL over as soon as it appears.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.starts_with("http://127.0.0.1:") && t.contains("/authorize?") {
                let _ = tx.send(t.to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let auth_url = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(u) => u,
        Err(_) => {
            let _ = child.kill();
            panic!(
                "login printed no authorization URL:\n{}",
                drain.join().unwrap()
            );
        }
    };
    assert!(auth_url.contains("code_challenge_method=S256"));
    assert!(
        auth_url.contains("scope=mcp"),
        "scope from discovery: {auth_url}"
    );
    // Follows the 302 to the loopback callback, which hands mcpdial the code.
    let mut resp = ureq::get(&auth_url).call().expect("authorize -> callback");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp
        .body_mut()
        .read_to_string()
        .unwrap()
        .contains("close this tab"));
    let out = child.wait_with_output().unwrap();
    let log = drain.join().unwrap();
    assert!(out.status.success(), "login exited {}:\n{log}", out.status);
    (log, String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`drive_login`] for a login that is expected to be refused. The browser step is
/// still played, because what refuses it arrives on the callback.
fn drive_refused_login(home: &std::path::Path, target: &str) -> String {
    let mut child = mcpdial(home)
        .args(["login", target, "--no-browser"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.starts_with("http://127.0.0.1:") && t.contains("/authorize?") {
                let _ = tx.send(t.to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let auth_url = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(u) => u,
        Err(_) => {
            let _ = child.kill();
            panic!(
                "login printed no authorization URL:\n{}",
                drain.join().unwrap()
            );
        }
    };
    ureq::get(&auth_url).call().expect("authorize -> callback");
    let status = child.wait().unwrap();
    let log = drain.join().unwrap();
    assert!(!status.success(), "login should have been refused:\n{log}");
    log
}

fn saved_credentials(home: &std::path::Path) -> Value {
    match std::fs::read_to_string(home.join("credentials.json")) {
        Ok(text) => serde_json::from_str(&text).unwrap(),
        Err(_) => json!({}),
    }
}

fn add_auth_server(home: &std::path::Path, s: &common::FakeServer) {
    assert_eq!(
        run(mcpdial(home).args(["add", "work", "--http", &s.url])).code,
        0
    );
}

fn registrations(s: &common::FakeServer) -> usize {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/register")
        .count()
}

#[test]
fn an_authorization_response_naming_the_issuer_it_was_asked_of_is_redeemed() {
    let s = start(Mode::AuthIssuer {
        advertised: true,
        iss: Iss::Own,
    });
    let home = temp_home("iss-ok");
    add_auth_server(&home, &s);

    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("saved token for work (expires in"), "{log}");
    assert_eq!(
        saved_credentials(&home)["credentials"]["work"]["issuer"],
        json!(s.base)
    );

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

#[test]
fn an_authorization_response_from_another_issuer_is_never_redeemed() {
    let s = start(Mode::AuthIssuer {
        advertised: true,
        iss: Iss::Other("https://evil.example"),
    });
    let home = temp_home("iss-mixup");
    add_auth_server(&home, &s);

    let log = drive_refused_login(&home, "work");
    assert!(
        log.contains("authorization response came from https://evil.example"),
        "{log}"
    );
    assert!(log.contains("refusing to redeem the code"), "{log}");
    assert_eq!(
        saved_credentials(&home)["credentials"]["work"],
        Value::Null,
        "nothing may be saved for a response that was not this server's"
    );
    assert!(
        !s.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/token"),
        "the code must not reach a token endpoint"
    );
}

#[test]
fn a_response_that_names_nobody_is_refused_only_where_the_server_said_it_always_would() {
    let s = start(Mode::AuthIssuer {
        advertised: true,
        iss: Iss::Absent,
    });
    let home = temp_home("iss-absent");
    add_auth_server(&home, &s);
    let log = drive_refused_login(&home, "work");
    assert!(log.contains("named no issuer"), "{log}");

    // The same response from a server that never advertised RFC 9207 is fine: the
    // servers that have not caught up are still the majority.
    let quiet = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("iss-unadvertised");
    add_auth_server(&home, &quiet);
    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("saved token for work"), "{log}");
}

#[test]
fn a_wrong_issuer_is_refused_even_where_the_server_advertised_nothing() {
    let s = start(Mode::AuthIssuer {
        advertised: false,
        iss: Iss::Other("https://evil.example"),
    });
    let home = temp_home("iss-unadvertised-wrong");
    add_auth_server(&home, &s);
    let log = drive_refused_login(&home, "work");
    assert!(
        log.contains("authorization response came from https://evil.example"),
        "{log}"
    );
}

#[test]
fn dynamic_registration_says_it_is_a_native_client() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("native");
    add_auth_server(&home, &s);
    drive_login(&home, "work", &[]);

    let registration = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.path == "/register")
        .expect("a registration request")
        .json();
    assert_eq!(
        registration["application_type"], "native",
        "an omitted application_type is \"web\" under OIDC, which forbids loopback"
    );
    assert!(registration["redirect_uris"][0]
        .as_str()
        .unwrap()
        .starts_with("http://127.0.0.1:"));
}

#[test]
fn a_client_id_is_not_presented_to_an_authorization_server_that_did_not_grant_it() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("issuer-moved");
    add_auth_server(&home, &s);
    drive_login(&home, "work", &[]);
    assert_eq!(
        saved_credentials(&home)["credentials"]["work"]["issuer"],
        json!(s.base)
    );

    // The endpoint now answers to a different authorization server. What was
    // registered with the old one is no use, so a fresh registration is made.
    let path = home.join("credentials.json");
    let mut saved = saved_credentials(&home);
    saved["credentials"]["work"]["issuer"] = json!("https://elsewhere.example");
    std::fs::write(&path, saved.to_string()).unwrap();

    let log = drive_login(&home, "work", &[]);
    assert!(
        log.contains("no longer https://elsewhere.example"),
        "the change is said out loud:\n{log}"
    );
    assert_eq!(registrations(&s), 2, "the old client id is not presented");
    assert_eq!(
        saved_credentials(&home)["credentials"]["work"]["issuer"],
        json!(s.base)
    );

    // A client an administrator registered by hand cannot be re-registered, so the
    // mismatch is surfaced rather than worked around.
    let mut saved = saved_credentials(&home);
    saved["credentials"]["work"]["issuer"] = json!("https://elsewhere.example");
    saved["credentials"]["work"]["registration"] = json!("pre-registered");
    std::fs::write(&path, saved.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["login", "work", "--no-browser"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("was registered with https://elsewhere.example"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("--client-id"), "{}", o.stderr);
}

/// A `credentials.json` written before mcpdial recorded issuers has no `issuer`
/// key. Its token still works, and the first login stamps the issuer on it rather
/// than throwing the registration away.
#[test]
fn a_credential_saved_before_issuers_were_recorded_keeps_working() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("old-credentials");
    add_auth_server(&home, &s);
    drive_login(&home, "work", &[]);
    assert_eq!(registrations(&s), 1);

    // What an older mcpdial left behind: everything but the issuer.
    let path = home.join("credentials.json");
    let mut saved = saved_credentials(&home);
    saved["credentials"]["work"]
        .as_object_mut()
        .unwrap()
        .remove("issuer");
    std::fs::write(&path, saved.to_string()).unwrap();

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"still here"}"#]));
    assert_eq!(o.code, 0, "the saved token must still work: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: still here");

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("issuer:"), "none is recorded yet");

    drive_login(&home, "work", &[]);
    assert_eq!(
        registrations(&s),
        1,
        "the saved client id is adopted, not registered over"
    );
    assert_eq!(
        saved_credentials(&home)["credentials"]["work"]["issuer"],
        json!(s.base),
        "and the issuer is recorded from now on"
    );
}

#[test]
fn redirects_are_refused_not_followed() {
    let s = start(Mode::Stateless);
    let home = temp_home("redirect");
    let moved = format!("{}/moved", s.base);
    let o = run(mcpdial(&home).args(["info", &moved]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("redirected (301) to"), "{}", o.stderr);
    assert!(
        o.stderr.contains(&s.url),
        "names the final URL: {}",
        o.stderr
    );
    assert_eq!(s.requests.lock().unwrap().len(), 1, "did not follow");

    let o = run(mcpdial(&home).args(["login", &moved, "--no-browser"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("redirected (301) to"), "{}", o.stderr);
}

#[test]
fn login_falls_back_to_localhost_when_127_is_refused() {
    let s = start(Mode::AuthLocalhostOnly);
    let home = temp_home("localhost");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("refused http://127.0.0.1"), "{log}");
    assert!(log.contains("retrying with http://localhost"), "{log}");
    assert!(log.contains("registered client client-abc"), "{log}");
    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(
        creds.contains("\"redirect_host\": \"localhost\""),
        "{creds}"
    );
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    // The saved client id is reused on a second login, with the host it was registered for.
    let log = drive_login(&home, "work", &[]);
    assert!(
        !log.contains("registered client"),
        "should reuse the client id:\n{log}"
    );
    assert!(!log.contains("refused"), "{log}");

    // Forcing 127.0.0.1 gets the server-side hint instead of a silent retry.
    let o = run(mcpdial(&home).args(["logout", "work"]));
    assert_eq!(o.code, 0);
    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--no-browser",
        "--redirect-host",
        "127.0.0.1",
    ]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("refused the loopback redirect URI"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains("force_ssl_in_redirect_uri"),
        "{}",
        o.stderr
    );
}

#[test]
fn oauth_login_saves_a_token_and_refreshes_it() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("oauth");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"x"}"#]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("HTTP 401") && o.stderr.contains("mcpdial login"),
        "{}",
        o.stderr
    );

    let log = drive_login(&home, "work", &[]);
    assert!(log.contains("registered client client-abc"), "{log}");
    assert!(log.contains("saved token for work (expires in"), "{log}");

    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(creds.contains("\"access_token\": \"tok-1\""), "{creds}");
    assert!(creds.contains("\"refresh_token\": \"ref-1\""));
    assert!(creds.contains("\"client_id\": \"client-abc\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("credentials.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // The saved token is used, and the browser is never needed again.
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"with saved token"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: with saved token");
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-1"));

    let o = run(mcpdial(&home).args(["ls"]));
    assert!(
        o.stdout.contains("connected") && o.stdout.contains("saved"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("access token:  present"));
    assert!(o.stdout.contains("refresh token: present"));
    assert!(!o.stdout.contains("tok-1"), "never print the secret");

    // Clock says the token is stale: refresh happens before the call.
    let mut v: Value = serde_json::from_str(&creds).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(home.join("credentials.json"), v.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"after refresh"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-2"));
    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(creds.contains("\"access_token\": \"tok-2\""));
    assert!(
        creds.contains("\"refresh_token\": \"ref-2\""),
        "rotated refresh token is kept: {creds}"
    );

    // Clock says fine but the server disagrees: one refresh and retry, transparently.
    let mut v: Value = serde_json::from_str(&creds).unwrap();
    v["credentials"]["work"]["access_token"] = Value::from("revoked");
    v["credentials"]["work"]["expires_at"] = Value::from(4_000_000_000u64);
    std::fs::write(home.join("credentials.json"), v.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"after 401"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: after 401");
    let last = s.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.header("authorization"), Some("Bearer tok-3"));

    // Logout forgets it.
    assert_eq!(run(mcpdial(&home).args(["logout", "work"])).code, 0);
    let o = run(mcpdial(&home).args(["ls"]));
    assert!(o.stdout.contains("auth required"), "{}", o.stdout);
    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 2);
}

#[test]
fn manual_token_from_stdin_or_env() {
    let s = start(Mode::Auth {
        tokens: vec!["pasted".into(), "from-env".into()],
    });
    let home = temp_home("manual");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let mut child = mcpdial(&home)
        .args(["token", "set", "work"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"pasted\n").unwrap();
    assert!(child.wait().unwrap().success());

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer pasted")
    );

    let o = run(mcpdial(&home)
        .env("T", "from-env")
        .args(["token", "set", "work", "--env", "T"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer from-env")
    );

    // --token-env on the command line beats the saved credential.
    let o = run(mcpdial(&home).env("OVERRIDE", "pasted").args([
        "--token-env",
        "OVERRIDE",
        "call",
        "work",
        "echo",
        "{}",
    ]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer pasted")
    );

    let o = run(mcpdial(&home).args(["--token-env", "UNSET_VAR_X", "call", "work", "echo", "{}"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("$UNSET_VAR_X is unset"));

    assert_eq!(run(mcpdial(&home).args(["token", "rm", "work"])).code, 0);
    assert_eq!(
        run(mcpdial(&home).args(["call", "work", "echo", "{}"])).code,
        1
    );

    // Removing a credential a saved server no longer has stays the idempotent
    // success it looks like, for a URL as much as for a name.
    assert_eq!(run(mcpdial(&home).args(["logout", "work"])).code, 0);
    assert_eq!(run(mcpdial(&home).args(["logout", &s.url])).code, 0);

    // A name that stands for no server is the typo `rm` refuses, not a logout.
    let o = run(mcpdial(&home).args(["logout", "nothere"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("no server named \"nothere\""),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["--json", "token", "rm", "nothere"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stdout.is_empty(), "{}", o.stdout);
    let err: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(err["error"]["kind"], "usage");
}

/// `exit` has always ended the shell; the help the shell prints has to say so,
/// since a reader who only ever sees `quit` cannot know the other word works.
#[test]
fn exit_ends_the_shell_and_the_help_says_both_words() {
    let home = temp_home("shell-exit");
    let target = format!("stdio:{}", echo_command());

    let (stdout, stderr, code) = shell(&home, &target, false, "call count\nexit\ncall count\n");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("count=1\n"), "{stdout}");
    assert!(!stdout.contains("count=2"), "exit stops reading: {stdout}");

    let (_, help, _) = shell(&home, &target, false, "help\n");
    assert!(help.contains("quit (or exit)"), "{help}");
}

/// `--force` writes over a saved name, and the receipt says what it wrote over
/// so an accident is visible rather than silent.
#[test]
fn force_says_what_it_replaced() {
    let home = temp_home("replace");
    let first = "http://127.0.0.1:1/mcp";
    let second = "http://127.0.0.1:2/mcp";

    let o = run(mcpdial(&home).args(["add", "web", "--http", first, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stderr.trim(), format!("saved web (http {first})"));

    let o = run(mcpdial(&home).args(["add", "web", "--http", second, "--no-probe"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("pass --force"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["add", "web", "--http", second, "--no-probe", "--force"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr.trim(),
        format!("saved web (http {second}), replacing http {first}")
    );
}

#[test]
fn extra_headers_are_sent_and_saved() {
    let s = start(Mode::Stateless);
    let home = temp_home("headers");
    let o = run(mcpdial(&home).args(["-H", "X-Team: blue", "info", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(s.requests.lock().unwrap()[0].header("x-team"), Some("blue"));

    assert_eq!(
        run(mcpdial(&home).args(["add", "t", "--http", &s.url, "-H", "X-Saved: yes"])).code,
        0
    );
    let o = run(mcpdial(&home).args(["info", "t"]));
    assert_eq!(o.code, 0);
    assert_eq!(
        s.requests.lock().unwrap().last().unwrap().header("x-saved"),
        Some("yes")
    );

    let o = run(mcpdial(&home).args(["-H", "nocolon", "info", &s.url]));
    assert_eq!(o.code, 2);
}

#[test]
fn stdio_env_and_cwd_are_passed_to_the_process() {
    let home = temp_home("env");
    let echo = echo_command();
    let o = run(mcpdial(&home).args([
        "add",
        "tagged",
        "--stdio",
        &echo,
        "--env",
        "ECHO_SERVER_TAG=hello",
        "--cwd",
        home.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(mcpdial(&home).args(["info", "tagged"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("tag=hello"), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["add", "bad", "--stdio", &echo, "--env", "NOEQUALS"]));
    assert_eq!(o.code, 2);
    let o = run(mcpdial(&home).args(["add", "bad", "--http", "http://x/mcp", "--env", "A=1"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("only apply to --stdio"));
}

#[test]
fn shell_keeps_one_session_alive() {
    let home = temp_home("shell");
    let target = format!("stdio:{}", echo_command());

    // Separate invocations are separate processes: the counter never gets past 1.
    for _ in 0..2 {
        let o = run(mcpdial(&home).args(["call", &target, "count"]));
        assert_eq!(o.stdout.trim(), "count=1");
    }

    // The results are numbered as they print, so the lines after them can name
    // one instead of running it again.
    let saved = home.join("out.txt");
    let script = format!(
        "# a comment\ncall count\ncall count {{}}\ntools\ncall nope\nraw tools/list\n\
         call count\nshow 1\nsave 2 {}\nretry echo message=again\nquit\ncall count\n",
        saved.display()
    );
    let mut child = mcpdial(&home)
        .args(["shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("count=1\n"), "{stdout}");
    assert!(stdout.contains("count=2\n"), "{stdout}");
    assert!(
        stdout.contains("count=3\n"),
        "same process throughout: {stdout}"
    );
    assert!(!stdout.contains("count=4"), "quit stops reading: {stdout}");
    assert!(stdout.contains("5 tool(s):"), "{stdout}");
    assert!(
        stdout.contains("\"name\": \"count\""),
        "raw output: {stdout}"
    );
    assert!(
        stderr.contains("MCP error -32602"),
        "errors go to stderr and do not end the session: {stderr}"
    );
    assert_eq!(
        stdout.matches("count=1\n").count(),
        2,
        "`show 1` prints the first result again: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "count=2",
        "`save 2` writes the second result"
    );
    assert!(
        stdout.contains("Echo: again"),
        "`retry echo message=again` sends the pairs it was given: {stdout}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed command in a script is reported in the exit code"
    );

    let mut child = mcpdial(&home)
        .args(["--json", "shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call count\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["content"][0]["text"], "count=1");
}

/// One `shell` session fed a script, with a wall clock around it: a shell left
/// waiting for input nobody is going to type fails the test instead of stalling
/// the suite.
fn shell_script(cmd: &mut Command, script: &str) -> Out {
    use std::time::{Duration, Instant};
    let mut child = cmd
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
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().ok();
                child.wait().ok();
                panic!("the shell was still running after 60s");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// The three ways of naming a result, what `save` does with each kind of one,
/// and what it says when it will not write.
#[test]
fn shell_names_results_and_saves_them() {
    let home = temp_home("shell-history");
    let target = format!("stdio:{}", echo_command());
    let text = home.join("text.txt");
    let shot = home.join("shot.png");
    let script = format!(
        "call echo {{\"message\":\"one\"}}\ncall count\ncall shot\n\
         show 1\nshow $2\nshow _\nsave 2 {text}\nsave 3 {shot}\n\
         show 9\nshow nope\nsave 1 {text}\nsave 1 {dir}\nquit\n",
        text = text.display(),
        shot = shot.display(),
        dir = home.display(),
    );
    let o = shell_script(mcpdial(&home).args(["shell", &target]), &script);

    assert_eq!(
        o.stdout.matches("Echo: one").count(),
        2,
        "`show 1` prints the first result again: {}",
        o.stdout
    );
    assert_eq!(
        o.stdout.matches("count=1").count(),
        2,
        "`show $2` names the second: {}",
        o.stdout
    );
    assert_eq!(
        std::fs::read_to_string(&text).unwrap(),
        "count=1",
        "text is saved as text"
    );
    let bytes = std::fs::read(&shot).unwrap();
    assert_eq!(
        &bytes[..4],
        b"\x89PNG",
        "a binary block is saved as its bytes"
    );
    assert_eq!(bytes.len(), 4096, "all of them, not the placeholder line");

    assert!(
        o.stderr.contains("there is no result 9"),
        "a number nobody printed: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("not a result number"),
        "a word that is no number: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("already exists"),
        "a file already there is not written over: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("is a directory"),
        "and neither is a directory: {}",
        o.stderr
    );
    assert_eq!(o.code, 1, "the refusals are counted: {}", o.stderr);

    // With no file named, one is named after the tool and the media type. It
    // lands in the working directory, so give the session one of its own.
    let named = temp_home("shell-save-named");
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]).current_dir(&named),
        "call shot\nsave _\nquit\n",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("wrote 4,096 bytes to shot.png"),
        "{}",
        o.stdout
    );
    assert_eq!(std::fs::read(named.join("shot.png")).unwrap().len(), 4096);
}

/// `retry` sends the last call again with one argument different, and takes the
/// types from the tool's schema exactly as a typed `call` does.
#[test]
fn shell_retries_the_last_call_with_one_argument_changed() {
    let home = temp_home("shell-retry");
    let target = format!("stdio:{}", echo_command());
    let session = || {
        let mut c = mcpdial(&home);
        c.env("ECHO_SERVER_TYPES", "1").args(["shell", &target]);
        c
    };
    let o = shell_script(
        &mut session(),
        "call typed text=a count=1\nretry count=2\nretry typed flag=true\n\
         retry echo message=hi\nretry\nquit\n",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains(r#"{"count":1,"text":"a"}"#),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(r#"{"count":2,"text":"a"}"#),
        "the arguments nobody changed are kept, and 2 is still a number: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains(r#"{"count":2,"flag":true,"text":"a"}"#),
        "a named tool retries its own last call: {}",
        o.stdout
    );
    assert_eq!(
        o.stdout.matches("Echo: hi").count(),
        2,
        "a bare `retry` sends the last call over: {}",
        o.stdout
    );
    assert!(
        o.stderr.contains(r#"call typed {"count":2,"text":"a"}"#),
        "what it re-ran is said the way it would be typed: {}",
        o.stderr
    );

    // Nothing to retry yet says so rather than sending anything.
    let o = shell_script(&mut session(), "retry\nedit\nquit\n");
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr.matches("no call in this session yet").count(),
        2,
        "{}",
        o.stderr
    );
}

/// `edit` opens a call's arguments in `$EDITOR` and sends what it left; an editor
/// that fails, or leaves nothing behind, sends nothing.
#[cfg(unix)]
#[test]
fn shell_edit_sends_what_the_editor_left() {
    let home = temp_home("shell-edit");
    let target = format!("stdio:{}", echo_command());
    let write = |name: &str, body: &str| {
        let path = home.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    };
    let good = write(
        "good.sh",
        "#!/bin/sh\nprintf '{\"message\": \"edited\"}' > \"$1\"\n",
    );
    let refuses = write("bad.sh", "#!/bin/sh\nexit 3\n");
    let empties = write("empty.sh", "#!/bin/sh\n: > \"$1\"\n");

    let script = "call echo {\"message\":\"first\"}\nedit 1\nquit\n";
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]).env("EDITOR", &good),
        script,
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Echo: edited"), "{}", o.stdout);
    assert!(
        o.stderr.contains(r#"call echo {"message":"edited"}"#),
        "{}",
        o.stderr
    );

    for (editor, said) in [
        (&refuses, "exited without saving"),
        (&empties, "left empty"),
    ] {
        let o = shell_script(
            mcpdial(&home)
                .args(["shell", &target])
                .env("EDITOR", editor),
            script,
        );
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(o.stderr.contains(said), "{}", o.stderr);
        assert_eq!(
            o.stdout.matches("Echo:").count(),
            1,
            "nothing was sent a second time: {}",
            o.stdout
        );
    }

    // Nothing to open the arguments with is said before anything is written.
    let o = shell_script(
        mcpdial(&home)
            .args(["shell", &target])
            .env_remove("EDITOR")
            .env_remove("VISUAL"),
        script,
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("set $EDITOR"), "{}", o.stderr);
}

/// A `| ...` at the end of a shell line prints one part of the result instead of
/// all of it, after a fresh command and after the number of one already printed.
#[test]
fn shell_filters_a_result_with_a_path_expression() {
    let home = temp_home("shell-filter");
    let target = format!("stdio:{}", echo_command());
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]),
        concat!(
            "raw tools/list | .tools[].name\n",
            "call echo {\"message\":\"hi\"} | .content[0].text\n",
            // A bar inside a JSON string is part of the string, not a filter.
            "call echo {\"message\":\"a|b\"} | .content[0].text\n",
            "_ | .content[0].type\n",
            "show 2 | .content[].text\n",
            "$2 | .\n",
            // A retry carries the filter on the end of the line it hands back.
            "retry message=again | .content[0].text\n",
            "call echo {\"message\":\"z\"} | .nope.deep\n",
            "quit\n",
        ),
    );
    assert_eq!(
        o.code, 0,
        "a path naming nothing is not a failure: {}",
        o.stderr
    );
    assert_eq!(
        o.stdout,
        concat!(
            "echo\nfail\ncount\nstrict\nshot\n",
            "Echo: hi\n",
            "Echo: a|b\n",
            "text\n",
            "Echo: hi\n",
            "{\"content\":[{\"text\":\"Echo: hi\",\"type\":\"text\"}]}\n",
            "Echo: again\n",
        ),
        "stderr was: {}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(r#"call echo {"message":"again"} | .content[0].text"#),
        "the line a retry hands back carries the filter, ready to paste: {}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(".nope.deep matched nothing in this result"),
        "a path that matches nothing prints nothing and says so: {}",
        o.stderr
    );
}

/// What the small grammar will not read goes to `jq`, and what neither of them
/// can read is refused before anything is sent.
#[test]
fn shell_hands_the_rest_to_jq_and_refuses_what_neither_can_read() {
    let home = temp_home("shell-filter-jq");
    let target = format!("stdio:{}", echo_command());

    // `count` answers with how many calls it has had, so the number it comes
    // back with is the proof that an unreadable filter sent nothing.
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]),
        "call count | .content[\ntools | .name\n| .name\ncall count\nquit\n",
    );
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("is not a path"), "{}", o.stderr);
    assert!(
        o.stderr.contains("tools prints no result to filter"),
        "a filter needs a result to filter: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("a filter needs a command in front of it"),
        "and a command in front of it: {}",
        o.stderr
    );
    assert_eq!(
        o.stdout, "count=1\n",
        "the filter nobody could read sent nothing: {}",
        o.stdout
    );

    let script = concat!(
        "call echo {\"message\":\"hi\"} | jq -r '.content[0].text'\n",
        "call echo {\"message\":\"hi\"} | jq '.content['\n",
        "quit\n",
    );
    let o = shell_script(mcpdial(&home).args(["shell", &target]), script);
    if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join("jq").is_file() || dir.join("jq.exe").is_file())
    {
        assert_eq!(o.stdout, "Echo: hi\n", "{}", o.stderr);
        assert!(
            o.stderr.contains("jq failed"),
            "a filter jq itself refuses is reported as jq's refusal: {}",
            o.stderr
        );
    } else {
        assert_eq!(o.stdout, "", "{}", o.stdout);
        assert_eq!(
            o.stderr.matches("jq is not installed").count(),
            2,
            "{}",
            o.stderr
        );
    }
    assert_eq!(o.code, 1, "{}", o.stderr);
}

/// Every way a line can be wrong should answer with the shape that was wanted.
#[test]
fn shell_explains_the_shape_it_expected() {
    let home = temp_home("shell-hints");
    let target = format!("stdio:{}", echo_command());
    let script = concat!(
        "echo\n",                            // a tool name typed as if it were a command
        "call echo\n",                       // a required argument left out
        "call echo [www.x.com](http://x)\n", // arguments that are not JSON at all
        "call echo {message:x}\n",           // an object missing its quotes
        "call ech {\"message\":\"x\"}\n",    // a tool name with a typo
        "tolls\n",                           // a command with a typo
        "schema echo\n",
        "help echo\n",
        "quit\n",
    );
    let (stdout, stderr, code) = shell(&home, &target, false, script);

    // A bare tool name is the commonest mistake, and it names the fix.
    assert!(stderr.contains("echo is a tool, not a command"), "{stderr}");
    // Every failed call answers with a line that would have worked.
    assert_eq!(
        stderr
            .matches(r#"usage: call echo {"message": "<string>"}"#)
            .count(),
        5,
        "bare name, missing argument, two unparseable arguments and `help echo`: {stderr}"
    );
    assert!(
        stderr.contains("message: string (required)"),
        "and the parameter list: {stderr}"
    );
    // Unparseable arguments quote what actually arrived.
    assert!(
        stderr.contains(r#""[www.x.com](http://x)" is not JSON"#),
        "{stderr}"
    );
    // An object that only lacks its quotes gets them back.
    assert!(
        stderr.contains(r#"did you mean {"message": "x"}?"#),
        "{stderr}"
    );
    // Near misses are named, for tools and for commands.
    assert!(stderr.contains("did you mean echo?"), "{stderr}");
    assert!(stderr.contains("did you mean tools?"), "{stderr}");
    // schema prints the tool's own schema; help prints the readable form.
    assert!(
        stdout.contains(r#""required": ["#) && stdout.contains(r#""message""#),
        "{stdout}"
    );
    assert_eq!(code, Some(1), "a script with failures still exits 1");

    // A schema complaint that arrives as a failed *result* rather than a JSON-RPC
    // error is the same mistake, and gets the same answer.
    let (stdout, stderr, _) = shell(&home, &target, false, "call strict {}\n");
    assert!(stdout.contains("Required at pageId"), "{stdout}");
    assert!(
        stderr.contains(r#"usage: call strict {"pageId": <number>}"#),
        "a failed result still explains itself: {stderr}"
    );
    // A tool that just failed does not get a schema dumped under it.
    let (_, stderr, _) = shell(&home, &target, false, "call fail {}\n");
    assert!(!stderr.contains("usage:"), "{stderr}");

    // In --json the hint rides along on the error object, one line per command.
    let (stdout, _, _) = shell(&home, &target, true, "call echo\ncall nope {}\n");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(
        lines[0]["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with(r#"usage: call echo {"message": "<string>"}"#),
        "{stdout}"
    );
    assert!(
        lines[1]["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("lists all 5"),
        "{stdout}"
    );
}

/// Pipe `script` into `mcpdial shell` and collect everything it said.
fn shell(
    home: &std::path::Path,
    target: &str,
    json: bool,
    script: &str,
) -> (String, String, Option<i32>) {
    let mut cmd = mcpdial(home);
    if json {
        cmd.arg("--json");
    }
    let mut child = cmd
        .args(["shell", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

#[test]
fn http_sessions_are_terminated_on_close() {
    let s = start(Mode::Stateful);
    let home = temp_home("session-delete");

    let (_, stderr, code) = shell(&home, &s.url, false, "call add {\"a\":1,\"b\":2}\nquit\n");
    assert_eq!(code, Some(0), "{stderr}");
    let deletes: Vec<_> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "DELETE")
        .cloned()
        .collect();
    assert_eq!(deletes.len(), 1, "the shell ends its session, exactly once");
    assert_eq!(deletes[0].path, "/mcp");
    assert_eq!(deletes[0].header("mcp-session-id"), Some("sess-1"));
    assert_eq!(
        deletes[0].header("mcp-protocol-version"),
        Some("2025-06-18")
    );
    assert!(
        deletes[0]
            .header("user-agent")
            .unwrap()
            .starts_with("Mozilla/5.0"),
        "the same client that sent the POSTs"
    );

    let o = run(mcpdial(&home).args(["-v", "call", &s.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("-> DELETE"), "{}", o.stderr);

    let refuses = start(Mode::StatefulNoDelete);
    let o = run(mcpdial(&home).args(["call", &refuses.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 1 is 2.");
    assert_eq!(o.stderr, "", "a 405 never reaches the user");
    assert!(refuses
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "DELETE"));

    let stateless = start(Mode::Stateless);
    let o = run(mcpdial(&home).args(["call", &stateless.url, "add", r#"{"a":1,"b":1}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !stateless
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "DELETE"),
        "a stateless server never issued a session to end"
    );
}

#[test]
fn import_reads_host_configs() {
    let home = temp_home("import");
    let echo = echo_server().display().to_string();
    let s = start(Mode::Stateless);
    let cfg = home.join("hostconfig.json");
    std::fs::write(
        &cfg,
        serde_json::json!({
            "mcpServers": {
                "local": {"type": "stdio", "command": echo, "args": [], "env": {"ECHO_SERVER_TAG": "imported"}},
                "web": {"type": "http", "url": s.url, "headers": {"X-From": "import"}}
            },
            "projects": {"/some/project": {"mcpServers": {"spaced": {"command": "npx", "args": ["-y", "pkg", "/tmp/a b"]}}}},
            "oauthAccount": {"accessToken": "never-read"}
        })
        .to_string(),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 3 server(s)"), "{}", o.stderr);
    assert!(o.stderr.contains("projects./some/project"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["info", "local"]));
    assert!(
        o.stdout.contains("tag=imported"),
        "env came through: {}",
        o.stdout
    );
    let o = run(mcpdial(&home).args(["info", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        s.requests.lock().unwrap()[0].header("x-from"),
        Some("import")
    );
    let saved = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert!(saved.contains("npx -y pkg '/tmp/a b'"), "{saved}");
    assert!(!saved.contains("never-read"));

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert!(
        o.stderr.contains("skip") && o.stderr.contains("imported 0 server(s)"),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap(), "--force"]));
    assert!(o.stderr.contains("imported 3 server(s)"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["import", "/nonexistent/file.json"]));
    assert_eq!(o.code, 2);
}

#[test]
fn agent_surface_json_errors_file_args_schema_and_guide() {
    let s = start(Mode::Stateless);
    let auth = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("agent");

    // Errors are one JSON object on stderr under --json, with a kind to branch on.
    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "rpc");
    assert_eq!(e["error"]["code"], -32602);
    assert!(o.stdout.is_empty());

    let o = run(mcpdial(&home).args(["--json", "info", &auth.url]));
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "http");
    assert_eq!(e["error"]["status"], 401);
    assert!(e["error"]["www_authenticate"]
        .as_str()
        .unwrap()
        .contains("resource_metadata"));

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "echo", "{bad"]));
    assert_eq!(o.code, 2);
    assert_eq!(
        serde_json::from_str::<Value>(o.stderr.trim()).unwrap()["error"]["kind"],
        "usage"
    );

    // Arguments from a file and from stdin.
    let f = home.join("args.json");
    std::fs::write(&f, r#"{"message":"from a file"}"#).unwrap();
    let at = format!("@{}", f.display());
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", &at]));
    assert_eq!(o.stdout.trim(), "Echo: from a file", "{}", o.stderr);
    let mut child = mcpdial(&home)
        .args(["--json", "call", &s.url, "echo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"message":"from stdin"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], "Echo: from stdin");
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "@/nonexistent.json"]));
    assert_eq!(o.code, 2);

    // One tool's schema, and a helpful miss.
    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["name"], "add");
    assert_eq!(v["inputSchema"]["required"][0], "a");
    // A name this server does not have is the caller's mistake, not the server's:
    // `tools/list` succeeded and nothing was sent for the tool itself.
    let o = run(mcpdial(&home).args(["schema", &s.url, "nah"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("available: echo, add"), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "schema", &s.url, "nah"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "usage", "{}", o.stderr);
    assert!(err["error"].get("code").is_none(), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "schema", &s.url, "ad"]));
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["hint"], "did you mean add?", "{}", o.stderr);

    // The guide is embedded in the binary.
    let o = run(mcpdial(&home).args(["guide"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("# mcpdial for agents"));
    assert!(o.stdout.contains("Exit codes"));

    // Shell in JSON mode keeps errors on stdout, in order.
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
        .write_all(b"call echo {\"message\":\"one\"}\ncall nope\ncall echo {\"message\":\"two\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines[0]["content"][0]["text"], "Echo: one");
    assert_eq!(lines[1]["error"]["kind"], "rpc");
    assert_eq!(lines[2]["content"][0]["text"], "Echo: two");
    assert_eq!(out.status.code(), Some(1));

    // Under --json every line on either stream is an object: the schema hint
    // under a failed result, a missing credential, and each mutation's receipt.
    let objects = |text: &str| -> Vec<Value> {
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l:?}")))
            .collect()
    };
    let local = format!("stdio:{}", echo_command());
    let o = run(mcpdial(&home).args(["--json", "call", &local, "strict", "{}"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap()["isError"],
        true
    );
    let err = objects(&o.stderr);
    assert_eq!(err.len(), 1, "{}", o.stderr);
    assert!(
        err[0]["hint"]
            .as_str()
            .unwrap()
            .starts_with("usage: mcpdial call"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "token", "show", "nobody"]));
    assert_eq!(o.code, 2);
    assert!(o.stdout.is_empty());
    let err = objects(&o.stderr);
    assert_eq!(err[0]["error"]["kind"], "config");
    assert_eq!(err[0]["error"]["message"], "no credential saved for nobody");

    let o = run(mcpdial(&home).args(["--json", "add", "fake", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let out = objects(&o.stdout);
    assert_eq!(out.len(), 1, "{}", o.stdout);
    assert_eq!(out[0]["saved"]["name"], "fake");
    assert_eq!(out[0]["saved"]["kind"], "http");
    assert_eq!(out[0]["saved"]["location"], s.url);
    assert_eq!(out[0]["saved"]["status"]["state"], "connected");

    let o = run(mcpdial(&home)
        .env("TOK", "t")
        .args(["--json", "token", "set", "fake", "--env", "TOK"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    assert_eq!(objects(&o.stdout), [json!({"saved_credential": "fake"})]);
    let o = run(mcpdial(&home).args(["--json", "token", "rm", "fake"]));
    assert_eq!(objects(&o.stdout), [json!({"removed_credential": "fake"})]);
    let o = run(mcpdial(&home).args(["--json", "logout", "fake"]));
    assert_eq!(objects(&o.stdout), [json!({"removed_credential": null})]);
    assert!(o.stderr.is_empty(), "{}", o.stderr);

    let cfg = home.join("import.json");
    std::fs::write(
        &cfg,
        json!({"mcpServers": {
            "fake": {"type": "http", "url": s.url},
            "other": {"type": "http", "url": s.url},
        }})
        .to_string(),
    )
    .unwrap();
    let o = run(mcpdial(&home).args(["--json", "import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    assert_eq!(
        objects(&o.stdout),
        [json!({"imported": ["other"], "skipped": ["fake"]})]
    );
    let o = run(mcpdial(&home).args(["--json", "rm", "other"]));
    assert_eq!(objects(&o.stdout), [json!({"removed": "other"})]);
    assert!(o.stderr.is_empty(), "{}", o.stderr);

    // login's receipt has the same shape; its progress lines stay prose.
    let (_, stdout) = drive_login_out(&home, &auth.url, &["--json"]);
    let out = objects(&stdout);
    assert_eq!(out.len(), 1, "{stdout}");
    assert_eq!(out[0]["login"]["name"], auth.url);
    assert!(out[0]["login"]["expires_at"].is_u64(), "{stdout}");
    assert_eq!(out[0]["login"]["refreshable"], true);

    // In the shell, info is one line like every other command, and the hint
    // under a failed result is an object on stderr.
    let mut child = mcpdial(&home)
        .args(["--json", "shell", &local])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"info\ncall strict {}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines = objects(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines.len(), 2, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines[0]["serverInfo"]["name"], "echo-server");
    assert_eq!(lines[1]["isError"], true);
    let err = objects(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(err.len(), 1, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(err[0]["hint"]
        .as_str()
        .unwrap()
        .starts_with("usage: call strict"));
}

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
fn stdio_answers_what_the_server_asks_mid_call() {
    let home = temp_home("ping");
    let target = format!("stdio:{}", echo_command());

    // In this mode the server interrupts `tools/call` with a notification, a
    // `ping`, and a request we do not serve, and finishes the call only once both
    // requests are answered correctly. A client that just waits for its own id
    // deadlocks here and reports the timeout as the server's fault.
    let o = run(mcpdial(&home).env("ECHO_SERVER_PING", "1").args([
        "-v",
        "--timeout",
        "20",
        "call",
        &target,
        "echo",
        r#"{"message":"mid-call"}"#,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: mid-call");

    // The trace shows all three: the notification skipped, the ping answered with
    // an empty result, and the unsupported method refused with -32601.
    assert!(
        o.stderr.contains("(other message)") && o.stderr.contains("notifications/message"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(r#"{"id":"srv-ping","jsonrpc":"2.0","result":{}}"#),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains(r#""code":-32601"#) && o.stderr.contains("srv-roots"),
        "{}",
        o.stderr
    );
}

#[test]
fn completion_scripts_for_every_shell() {
    let home = temp_home("completions");

    for (shell, registration_line) in [
        ("bash", "complete -F _mcpdial"),
        ("zsh", "#compdef mcpdial"),
        ("fish", "complete -c mcpdial"),
        ("elvish", "edit:completion:arg-completer[mcpdial]"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let o = run(mcpdial(&home).args(["completions", shell]));
        assert_eq!(o.code, 0, "{shell}: {}", o.stderr);
        assert!(!o.stdout.is_empty(), "{shell}: empty script");
        assert!(
            o.stdout.contains(registration_line),
            "{shell}: no {registration_line:?}"
        );
        assert!(o.stdout.contains("token-env"), "{shell}: no global flags");
        assert!(
            o.stdout.contains("no-probe"),
            "{shell}: no per-command flags"
        );
    }

    let o = run(mcpdial(&home).args(["completions", "csh"]));
    assert_eq!(o.code, 2);
    assert!(o.stdout.is_empty());

    // Kept out of the command list, but named once among the examples, so
    // that `--help` alone is enough to find it.
    let o = run(mcpdial(&home).args(["--help"]));
    assert_eq!(o.code, 0);
    let (before_examples, examples) = o.stdout.split_once("examples:").unwrap();
    assert!(
        !before_examples.contains("completions"),
        "{before_examples}"
    );
    assert!(examples.contains("mcpdial completions SHELL"), "{examples}");
}

/// `completions` builds the command tree a second time, under the tree clap
/// already built to parse the arguments, so it is the deepest stack the binary
/// ever reaches. clap_derive expands that tree into one function, and
/// unoptimized its frame is most of the 1 MiB Windows reserves for a main
/// thread, so the command tree growing by a few arguments is enough to overflow
/// it there and nowhere else. `.cargo/config.toml` links Windows binaries with
/// the 8 MiB Unix reserves instead; this is the check that it took.
#[test]
#[cfg(windows)]
fn the_windows_binary_reserves_a_unix_sized_stack() {
    const WANTED: u64 = 8 * 1024 * 1024;
    const PE32_PLUS: u16 = 0x20b;
    const COFF_HEADER_LEN: usize = 24;
    const STACK_RESERVE_IN_OPTIONAL_HEADER: usize = 0x48;

    let image = std::fs::read(env!("CARGO_BIN_EXE_mcpdial")).unwrap();
    let at_u16 = |at: usize| u16::from_le_bytes(image[at..at + 2].try_into().unwrap());
    let at_u32 = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    let at_u64 = |at: usize| u64::from_le_bytes(image[at..at + 8].try_into().unwrap());

    let pe_header = at_u32(0x3c) as usize;
    assert!(
        image[pe_header..pe_header + 4] == *b"PE\0\0",
        "not a PE image"
    );
    let optional_header = pe_header + COFF_HEADER_LEN;
    assert_eq!(
        at_u16(optional_header),
        PE32_PLUS,
        "the stack reserve sits at another offset in a 32-bit image"
    );

    let reserved = at_u64(optional_header + STACK_RESERVE_IN_OPTIONAL_HEADER);
    assert!(
        reserved >= WANTED,
        "the binary reserves {reserved} bytes of stack, short of the {WANTED} \
         .cargo/config.toml asks for; a debug build overflows a 1 MiB stack \
         before it has finished parsing an argument"
    );
}

#[test]
fn a_structured_only_result_still_prints() {
    let s = start(Mode::Stateless);
    let home = temp_home("structured");

    let o = run(mcpdial(&home).args(["call", &s.url, "reading"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["celsius"], 20, "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "reading"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["structuredContent"]["celsius"], 20);
}

#[test]
fn content_blocks_win_over_structured_content() {
    let s = start(Mode::Stateless);
    let home = temp_home("both-shapes");

    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
}

#[test]
fn schema_shows_an_output_schema_and_names_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-schema");

    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["outputSchema"]["required"][0], "sum");
    assert!(o.stderr.contains("structuredContent"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["schema", &s.url, "echo"]));
    assert!(o.stderr.is_empty(), "{}", o.stderr);
}

/// `drive_login` for a login that has a secret to supply: the process needs stdin,
/// or an environment, and a refused login has to be observable rather than fatal.
fn drive_secret_login(cmd: &mut Command, stdin_secret: Option<&str>) -> (bool, String) {
    let mut child = cmd
        .stdin(match stdin_secret {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(secret) = stdin_secret {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(secret.as_bytes()).unwrap();
    }
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let t = line.trim();
            if t.starts_with("http://127.0.0.1:") && t.contains("/authorize?") {
                let _ = tx.send(t.to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let auth_url = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(u) => u,
        Err(_) => {
            let _ = child.kill();
            panic!(
                "login printed no authorization URL:\n{}",
                drain.join().unwrap()
            );
        }
    };
    assert!(auth_url.contains("client_id=conf-client"), "{auth_url}");
    ureq::get(&auth_url).call().expect("authorize -> callback");
    let ok = child.wait().unwrap().success();
    (ok, drain.join().unwrap())
}

fn confidential_login(home: &std::path::Path, extra: &[&str]) -> Command {
    let mut cmd = mcpdial(home);
    cmd.args([
        "login",
        "work",
        "--no-browser",
        "--client-id",
        CONFIDENTIAL_ID,
    ])
    .args(extra);
    cmd
}

fn expire_saved_token(home: &std::path::Path) {
    let path = home.join("credentials.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(path, v.to_string()).unwrap();
}

fn last_token_request(s: &common::FakeServer) -> common::Recorded {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.path == "/token")
        .expect("a token request")
        .clone()
}

#[test]
fn a_confidential_client_posts_its_secret_and_refreshes_with_it() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("confidential-post");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let (ok, log) = drive_secret_login(&mut confidential_login(&home, &[]), None);
    assert!(
        !ok,
        "a confidential client cannot log in without its secret"
    );
    assert!(log.contains("token exchange failed: HTTP 401"), "{log}");

    let (ok, log) = drive_secret_login(
        &mut confidential_login(&home, &["--client-secret"]),
        Some(SECRET),
    );
    assert!(ok, "{log}");
    assert!(log.contains("saved token for work (expires in"), "{log}");

    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    assert!(
        creds.contains(&format!("\"client_secret\": \"{SECRET}\"")),
        "{creds}"
    );
    assert!(
        creds.contains("\"token_endpoint_auth_method\": \"client_secret_post\""),
        "{creds}"
    );

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert!(o
        .stdout
        .contains("client secret: present (client_secret_post)"));
    assert!(!o.stdout.contains(SECRET), "never print the secret");
    let o = run(mcpdial(&home).args(["token", "show", "work", "--json"]));
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap()["has_client_secret"],
        Value::Bool(true)
    );
    assert!(!o.stdout.contains(SECRET), "never print the secret");

    // Unattended from here on: the saved secret is what makes the refresh succeed.
    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"refreshed"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: refreshed");
    let refresh = last_token_request(&s);
    assert!(
        refresh.body.contains("grant_type=refresh_token"),
        "{refresh:?}"
    );
    assert!(refresh
        .body
        .contains("client_secret=c0nf%26s3cr3t%3D%2F%3Ax"));
}

#[test]
fn a_confidential_client_uses_basic_auth_when_that_is_all_the_server_offers() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_basic".into(),
    });
    let home = temp_home("confidential-basic");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let mut cmd = confidential_login(&home, &["--client-secret-env", "MCPDIAL_TEST_SECRET"]);
    cmd.env("MCPDIAL_TEST_SECRET", SECRET);
    let (ok, log) = drive_secret_login(&mut cmd, None);
    assert!(ok, "{log}");

    let exchange = last_token_request(&s);
    assert!(!exchange.body.contains("client_secret"), "{exchange:?}");
    assert_eq!(
        exchange.header("authorization"),
        Some("Basic Y29uZi1jbGllbnQ6YzBuZiUyNnMzY3IzdCUzRCUyRiUzQXg=")
    );

    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"basic"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let refresh = last_token_request(&s);
    assert!(
        refresh.body.contains("grant_type=refresh_token"),
        "{refresh:?}"
    );
    assert!(refresh.header("authorization").is_some(), "{refresh:?}");
}

#[test]
fn a_client_secret_reaches_neither_an_argument_nor_the_trace_output() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("confidential-quiet");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );

    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--client-id",
        "x",
        "--client-secret",
        SECRET,
    ]));
    assert_eq!(o.code, 2, "the flag must take no value: {}", o.stderr);
    let o = run(mcpdial(&home).args(["login", "work", "--client-secret"]));
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("--client-id"), "{}", o.stderr);

    let (ok, log) = drive_secret_login(
        &mut confidential_login(&home, &["--client-secret", "-v"]),
        Some(SECRET),
    );
    assert!(ok, "{log}");
    assert!(
        log.contains("authorization server:"),
        "the log is real: {log}"
    );
    assert!(!log.contains(SECRET), "the secret leaked into -v:\n{log}");

    expire_saved_token(&home);
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"x"}"#, "-v"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("-> POST"),
        "the trace is on: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains(SECRET) && !o.stdout.contains(SECRET),
        "the secret leaked into -v:\n{}",
        o.stderr
    );
}

fn listed(json: &str, name: &str) -> Value {
    let rows: Vec<Value> = serde_json::from_str(json).unwrap();
    rows.into_iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no row for {name} in {json}"))
}

/// Backdate every saved status, so a test can reach a TTL it would otherwise
/// have to wait out.
fn backdate_saved_statuses(home: &std::path::Path, seconds: u64) {
    let path = home.join("probes.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for record in v["probes"].as_object_mut().unwrap().values_mut() {
        record["checked_at"] = Value::from(now - seconds);
    }
    std::fs::write(&path, v.to_string()).unwrap();
}

/// The configured command appends a byte before handing over to the real
/// server, so the file's length is the number of times `ls` has run it.
#[cfg(unix)]
#[test]
fn a_warm_listing_spawns_no_stdio_server() {
    let home = temp_home("warm");
    let spawns = home.join("spawns");
    let command = format!(
        "sh -c 'printf x >> {}; exec {}'",
        spawns.display(),
        echo_server().display()
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "local", "--stdio", &command, "--no-probe"])).code,
        0
    );
    let spawned = || std::fs::read(&spawns).map(|b| b.len()).unwrap_or(0);

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(listed(&o.stdout, "local")["status"]["state"], "connected");
    assert_eq!(listed(&o.stdout, "local")["tools"], 5);
    assert_eq!(spawned(), 1);

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(listed(&o.stdout, "local")["status"]["state"], "connected");
    assert_eq!(listed(&o.stdout, "local")["tools"], 5);
    assert_eq!(spawned(), 1, "listing again ran the command again");

    assert_eq!(run(mcpdial(&home).args(["ls", "--no-probe"])).code, 0);
    assert_eq!(spawned(), 1);

    let o = run(mcpdial(&home).args(["--timeout", "5", "ls", "--refresh"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(spawned(), 2, "--refresh has to run it");
}

#[test]
fn a_remembered_status_shows_its_age_until_the_ttl_runs_out() {
    let s = start(Mode::Stateless);
    let home = temp_home("age");
    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &s.url, "--no-probe"])).code,
        0
    );
    let dialed = || s.requests.lock().unwrap().len();

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(listed(&o.stdout, "web")["age_seconds"], 0);
    let after_the_first_listing = dialed();
    assert!(after_the_first_listing > 0);

    backdate_saved_statuses(&home, 250);
    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    let row = listed(&o.stdout, "web");
    assert_eq!(row["status"]["state"], "connected");
    let age = row["age_seconds"].as_u64().unwrap();
    assert!((250..255).contains(&age), "reported an age of {age}");
    assert_eq!(
        dialed(),
        after_the_first_listing,
        "a four minute old status was dialed again"
    );

    let o = run(mcpdial(&home).args(["--timeout", "5", "ls"]));
    assert!(
        o.stdout.lines().next().unwrap().contains("AGE"),
        "{}",
        o.stdout
    );
    let row = o.stdout.lines().find(|l| l.starts_with("web")).unwrap();
    assert!(
        row.contains("connected") && row.contains("4m"),
        "{}",
        o.stdout
    );

    backdate_saved_statuses(&home, 400);
    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(listed(&o.stdout, "web")["age_seconds"], 0, "{}", o.stdout);
    assert!(
        dialed() > after_the_first_listing,
        "an expired status was reused"
    );

    // A separate process from the one that probed, so the clock may have
    // ticked over since; on a slow runner it does.
    let o = run(mcpdial(&home).args(["--timeout", "5", "ls"]));
    let row = o.stdout.lines().find(|l| l.starts_with("web")).unwrap();
    let age = row.split_whitespace().nth(3).unwrap();
    let just_taken = age == "now"
        || age
            .strip_suffix('s')
            .is_some_and(|n| n.parse::<u64>().is_ok_and(|n| n < 30));
    assert!(just_taken, "{}", o.stdout);

    let o = run(mcpdial(&home).args(["ls", "--refresh", "--no-probe"]));
    assert_eq!(o.code, 2, "{}", o.stdout);
}

#[test]
fn a_saved_status_does_not_survive_the_server_it_described() {
    let first = start(Mode::Stateless);
    let second = start(Mode::Blocked);
    let locked = start(Mode::Auth {
        tokens: vec!["ok".into()],
    });
    let home = temp_home("probe-key");
    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &first.url])).code,
        0
    );
    assert_eq!(
        run(mcpdial(&home).args(["add", "shut", "--http", &locked.url])).code,
        0
    );

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(listed(&o.stdout, "web")["status"]["state"], "connected");
    assert_eq!(
        listed(&o.stdout, "shut")["status"]["state"],
        "auth_required"
    );

    assert_eq!(
        run(mcpdial(&home).args(["add", "web", "--http", &second.url, "--force", "--no-probe"]))
            .code,
        0
    );
    assert_eq!(
        run(mcpdial(&home)
            .env("TOK", "ok")
            .args(["token", "set", "shut", "--env", "TOK"]))
        .code,
        0
    );

    let o = run(mcpdial(&home).args(["--timeout", "5", "--json", "ls"]));
    assert_eq!(listed(&o.stdout, "web")["status"]["state"], "blocked");
    assert_eq!(listed(&o.stdout, "web")["age_seconds"], 0);
    assert_eq!(listed(&o.stdout, "shut")["status"]["state"], "connected");
    assert_eq!(listed(&o.stdout, "shut")["auth"], "saved");
}

#[test]
fn resources_and_templates_are_listed_and_paginated() {
    let s = start(Mode::Stateless);
    let home = temp_home("resources");

    let o = run(mcpdial(&home).args(["resources", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 resource(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("file:///readme.md") && o.stdout.contains("file:///logo.png"),
        "both pages: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains("1 template(s):") && o.stdout.contains("file:///notes/{name}.md"),
        "templates are listed apart from the URIs: {}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("Second line"),
        "short listing shows the first line only"
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "resources/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "res-2");

    let o = run(mcpdial(&home).args(["resources", &s.url, "--long"]));
    assert!(
        o.stdout.contains("Second line") && o.stdout.contains("type: text/markdown"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "resources", &s.url]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    let uris: Vec<&str> = v["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, ["file:///readme.md", "file:///logo.png"]);
    assert_eq!(
        v["resourceTemplates"][0]["uriTemplate"],
        "file:///notes/{name}.md"
    );
}

#[test]
fn a_resource_reads_as_text_and_a_blob_as_raw_bytes() {
    let s = start(Mode::Stateless);
    let home = temp_home("read");

    let o = run(mcpdial(&home).args(["read", &s.url, "file:///readme.md"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout, "# fake-mcp\nA readme.\n",
        "text arrives unchanged"
    );

    // stdout is a pipe here, so a blob lands as the bytes it stands for.
    let out = mcpdial(&home)
        .args(["read", &s.url, "file:///logo.png"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, common::PNG_MAGIC);

    let o = run(mcpdial(&home).args(["--json", "read", &s.url, "file:///logo.png"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["contents"][0]["blob"], "iVBORw0KGgo=");
    assert_eq!(v["contents"][0]["mimeType"], "image/png");

    let o = run(mcpdial(&home).args(["read", &s.url, "file:///nope"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Resource not found"), "{}", o.stderr);
    assert!(
        !o.stderr.contains("offers no resources"),
        "the capability is there; only the URI was wrong: {}",
        o.stderr
    );
}

#[test]
fn prompts_are_listed_paginated_and_expanded() {
    let s = start(Mode::Stateless);
    let home = temp_home("prompts");

    let o = run(mcpdial(&home).args(["prompts", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 prompt(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("summarize") && o.stdout.contains("greet"),
        "both pages: {}",
        o.stdout
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "prompts/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "prompt-2");

    let o = run(mcpdial(&home).args(["prompts", &s.url, "--long"]));
    assert!(
        o.stdout.contains("arguments:") && o.stdout.contains("text (required) - What to summarize"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "prompts", &s.url, "--long"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["prompts"].as_array().unwrap().len(), 2);
    assert_eq!(v["prompts"][0]["arguments"][0]["name"], "text");

    let o = run(mcpdial(&home).args(["prompt", &s.url, "summarize", r#"{"text":"a memo"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "user: Summarize this: a memo\nassistant: Sure.\n");
    assert!(
        o.stderr.contains("Summarize a document."),
        "the description stays off stdout: {}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "prompt", &s.url, "greet"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["messages"][0]["content"]["text"], "Hello.");

    let o = run(mcpdial(&home).args(["prompt", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Prompt nope not found"), "{}", o.stderr);
}

#[test]
fn a_server_without_the_capability_names_it_instead_of_the_code() {
    let home = temp_home("no-capability");
    let target = format!("stdio:{}", echo_command());

    for capability in ["resources", "prompts"] {
        let o = run(mcpdial(&home).args([capability, &target]));
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(
            o.stderr
                .contains(&format!("this server offers no {capability}")),
            "{}",
            o.stderr
        );
    }

    let o = run(mcpdial(&home).args(["prompt", &target, "anything"]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("this server offers no prompts"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "read", &target, "file:///x"]));
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["code"], -32601);
    assert!(
        e["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("offers no resources"),
        "{}",
        o.stderr
    );
}

#[test]
fn the_shell_reaches_resources_and_prompts() {
    let s = start(Mode::Stateful);
    let home = temp_home("shell-resources");
    // A read and a prompt are numbered results too, so they can be shown again
    // and saved: text as text, a blob as the bytes it arrived as.
    let notes = home.join("summary.txt");
    let logo = home.join("logo.png");
    let script = format!(
        "resources\nread file:///readme.md\nprompts\n\
         prompt summarize {{\"text\":\"a memo\"}}\nshow 1\nsave 2 {notes}\n\
         read file:///logo.png\nsave 3 {logo}\nread\nquit\n",
        notes = notes.display(),
        logo = logo.display(),
    );
    let (stdout, stderr, code) = shell(&home, &s.url, false, &script);
    assert!(
        stdout.contains("2 resource(s):") && stdout.contains("file:///logo.png"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1 template(s):") && stdout.contains("file:///notes/{name}.md"),
        "{stdout}"
    );
    assert!(stdout.contains("# fake-mcp"), "{stdout}");
    assert!(stdout.contains("2 prompt(s):"), "{stdout}");
    assert!(stdout.contains("user: Summarize this: a memo"), "{stdout}");
    assert!(stderr.contains("read needs a resource URI"), "{stderr}");
    assert_eq!(code, Some(1), "the read with no URI failed");
    assert_eq!(
        stdout.matches("# fake-mcp").count(),
        2,
        "`show 1` prints the resource again: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "user: Summarize this: a memo\nassistant: Sure.",
        "a prompt is saved as the messages it printed"
    );
    assert_eq!(
        &std::fs::read(&logo).unwrap()[..4],
        b"\x89PNG",
        "a blob is saved as the bytes it arrived as"
    );

    let (stdout, _, _) = shell(&home, &s.url, true, "resources\nprompt greet\nquit\n");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert_eq!(lines[0]["resources"].as_array().unwrap().len(), 2);
    assert_eq!(lines[0]["resourceTemplates"][0]["name"], "note");
    assert_eq!(lines[1]["messages"][0]["content"]["text"], "Hello.");
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

#[test]
fn a_public_credential_saved_before_secrets_existed_still_refreshes() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("public-refresh");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );
    std::fs::write(
        home.join("credentials.json"),
        json!({"credentials": {"work": {
            "access_token": "stale", "refresh_token": "ref-1", "expires_at": 1_000_000u64,
            "token_endpoint": format!("{}/token", s.base), "client_id": "client-abc",
            "resource": s.url, "source": "oauth"}}})
        .to_string(),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"public"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let refresh = last_token_request(&s);
    assert!(!refresh.body.contains("client_secret"), "{refresh:?}");
    assert!(refresh.header("authorization").is_none(), "{refresh:?}");

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert!(!o.stdout.contains("client secret"), "{}", o.stdout);
}

#[test]
fn a_url_that_streams_no_endpoint_event_keeps_its_own_error() {
    let s = start(Mode::Stateless);
    let home = temp_home("not-legacy");

    let o = run(mcpdial(&home).args(["info", &format!("{}/nope", s.base)]));
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("HTTP 404"), "{}", o.stderr);
    assert!(!o.stderr.contains("HTTP+SSE"), "{}", o.stderr);
    assert!(
        s.requests.lock().unwrap().iter().any(|r| r.method == "GET"),
        "the 404 should still have been checked"
    );
}

#[test]
fn add_from_the_registry() {
    let s = start(Mode::Stateless);
    let home = temp_home("registry");
    let at_registry = || {
        let mut c = mcpdial(&home);
        c.env("MCPDIAL_REGISTRY", &s.base);
        c
    };

    // A Streamable HTTP remote is saved as an http server, and it dials.
    let o = run(at_registry().args(["add", "web", "--registry", "io.github.acme/remote"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains(&format!("saved web (http {})", s.url)),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("note:"), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["info", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp"), "{}", o.stdout);
    let after_info = s.requests.lock().unwrap().len();

    // An SSE-only remote is saved too, with the same note import gives.
    let o = run(at_registry().args(["add", "old", "--registry", "io.github.acme/legacy"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("note: configured as SSE"), "{}", o.stderr);

    // A required value the entry leaves to the user: exit 2 and nothing saved
    // without --arg, and the hint says what to pass.
    let o = run(at_registry().args(["add", "fs", "--registry", "io.github.acme/files"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("needs 1 value(s)")
            && o.stderr.contains("--arg VALUE")
            && o.stderr
                .contains("directory (required): Directory to serve"),
        "{}",
        o.stderr
    );
    let saved = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert!(!saved.contains("\"fs\""), "{saved}");

    let o = run(at_registry().args([
        "add",
        "fs",
        "--registry",
        "io.github.acme/files",
        "--arg",
        "/tmp/a b",
        "--env",
        "ACME_LOG=debug",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    let fs = &saved["servers"]["fs"];
    assert_eq!(fs["stdio"], "npx -y @acme/files@1.2.0 '/tmp/a b'");
    assert_eq!(fs["env"]["ACME_TOKEN"], "${ACME_TOKEN}");
    assert_eq!(fs["env"]["ACME_LOG"], "debug", "--env still applies");
    assert!(
        o.stderr
            .contains("ACME_TOKEN (required, secret): API token")
            && o.stderr.contains("directory (required)"),
        "{}",
        o.stderr
    );

    // --package picks among several; the first runnable one is the default.
    let o = run(at_registry().args([
        "add",
        "box",
        "--registry",
        "io.github.acme/box",
        "--package",
        "oci",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o2 = run(at_registry().args(["add", "box-py", "--registry", "io.github.acme/box"]));
    assert_eq!(o2.code, 0, "{}", o2.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    assert_eq!(
        saved["servers"]["box"]["stdio"],
        "docker run -i --rm -e BOX_KEY ghcr.io/acme/box:1.0"
    );
    assert_eq!(saved["servers"]["box-py"]["stdio"], "uvx acme-box==1.0");
    let o = run(at_registry().args(["add", "x", "--registry", "io.github.acme/box", "--remote"]));
    assert_eq!(o.code, 2);
    assert!(
        o.stderr
            .contains("no remote endpoint; it offers stdio (pypi), stdio (oci)"),
        "{}",
        o.stderr
    );

    // A name the registry does not have, and one that is not a registry name.
    let o = run(at_registry().args(["add", "x", "--registry", "io.github.nope/nope"]));
    assert_eq!(o.code, 2);
    assert!(
        o.stderr.contains("no server named io.github.nope/nope")
            && o.stderr.contains("like io.github.owner/server"),
        "{}",
        o.stderr
    );
    let o = run(at_registry().args(["--json", "add", "x", "--registry", "files"]));
    assert_eq!(o.code, 2);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
    assert!(e["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("like io.github.owner/server"));

    // The flags exclude one another, and nothing was run at add time.
    let o = run(at_registry().args([
        "add",
        "x",
        "--registry",
        "io.github.acme/remote",
        "--http",
        "http://x/mcp",
    ]));
    assert_eq!(o.code, 2);
    let o = run(at_registry().args(["add", "x", "--package", "npm"]));
    assert_eq!(o.code, 2);
    let dialed = s.requests.lock().unwrap()[after_info..]
        .iter()
        .any(|r| r.path == "/mcp");
    assert!(!dialed, "add never dials");

    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    for name in ["web", "old", "fs", "box", "box-py"] {
        assert!(
            o.stdout.lines().any(|l| l.starts_with(name)),
            "{name} missing:\n{}",
            o.stdout
        );
    }
}

/// The echo server in the mode where every `tools/call` asks the client for one
/// more fact and blocks until the answer arrives.
fn elicits(home: &std::path::Path, mode: &str) -> Command {
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
fn the_shell_elicit_command_answers_every_call_after_it() {
    let home = temp_home("elicit-shell");
    let target = format!("stdio:{}", echo_command());

    let script = "call echo {\"message\":\"one\"}\n\
                  elicit {\"confirm\":true,\"region\":\"eu\"}\n\
                  call echo {\"message\":\"two\"}\n\
                  elicit\n\
                  quit\n";
    let mut child = elicits(&home, "form")
        .args(["-v", "shell", &target])
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Before any answers were given the elicitation is declined; after, the
    // same call is accepted with them, for the rest of the session.
    assert!(
        stdout.contains(r#"elicited {"action":"decline"}"#),
        "{stdout}"
    );
    assert!(
        stdout.contains(r#"elicited {"action":"accept","content":{"confirm":true,"region":"eu"}}"#),
        "{stdout}"
    );
    assert!(
        stdout.contains("2 answer(s) ready to elicit with"),
        "{stdout}"
    );
    assert!(stderr.contains("elicit needs a JSON object"), "{stderr}");
    // A shell can be handed answers at any point, so it offers to fill in a
    // form from the start, before any have been typed.
    assert!(
        stderr.contains(r#""capabilities":{"elicitation":{"form":{},"url":{}}}"#),
        "{stderr}"
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
        .map(common::Recorded::json)
        .find(|body| body["method"] == "initialize")
        .expect("an initialize request");
    assert_eq!(
        handshake["params"]["capabilities"],
        json!({}),
        "nothing may be declared where nothing can answer"
    );
}

/// The tools the fake server offers, as a snapshot file holds them, plus the
/// path it was written to.
fn snapshot_of(home: &std::path::Path, url: &str, name: &str) -> (std::path::PathBuf, Value) {
    let path = home.join(name);
    let o = run(mcpdial(home).args(["tools", url, "--snapshot", path.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("wrote 2 tool(s) to"), "{}", o.stdout);
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    (path, document)
}

#[test]
fn a_snapshot_holds_the_whole_tool_and_a_check_of_it_passes() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-write");
    let (path, document) = snapshot_of(&home, &s.url, "tools.json");

    assert_eq!(
        document["server"],
        json!({"name": "fake-mcp", "version": "1.0"})
    );
    assert_eq!(document["protocolVersion"], "2025-06-18");
    let names: Vec<&str> = document["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["add", "echo"], "tools are sorted by name");
    assert_eq!(
        document["tools"][1]["inputSchema"]["properties"]["message"]["type"], "string",
        "a snapshot holds the schema a listing drops"
    );
    assert!(
        std::fs::read_to_string(&path).unwrap().ends_with("}\n"),
        "a file a repository commits ends in a newline"
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "ok");

    let o =
        run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap(), "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap(),
        json!({"ok": true, "differences": []})
    );
}

#[test]
fn a_renamed_parameter_is_exit_three_and_stops_the_call_it_would_have_broken() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-drift");
    let (_, mut document) = snapshot_of(&home, &s.url, "tools.json");

    // The snapshot a script committed back when the parameter was `text`.
    let echo = &mut document["tools"][1];
    echo["inputSchema"]["properties"] = json!({"text": {"type": "string"}});
    echo["inputSchema"]["required"] = json!(["text"]);
    let older = home.join("older.json");
    std::fs::write(&older, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let older = older.to_str().unwrap().to_string();

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older]));
    assert_eq!(o.code, 3, "drift has an exit code of its own: {}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "tool echo: required property text was removed",
            "tool echo: property message is new and required",
            "2 differences",
        ]
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older, "--json"]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    let report: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["differences"][0]["tool"], "echo");
    assert_eq!(report["differences"][0]["kind"], "property-removed");
    assert_eq!(report["differences"][0]["level"], "fail");
    assert_eq!(report["differences"][1]["kind"], "now-required");

    // The call the snapshot was written to protect never goes out.
    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "message=hi", "--check", &older]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    assert!(
        o.stdout.is_empty(),
        "stdout holds a result or nothing: {:?}",
        o.stdout
    );
    assert!(
        o.stderr
            .contains("tool echo: required property text was removed"),
        "{}",
        o.stderr
    );
    let sent = s.requests.lock().unwrap();
    assert!(
        !sent[before..]
            .iter()
            .any(|r| r.json()["method"] == "tools/call"),
        "nothing was called"
    );
    drop(sent);

    // A tool the snapshot still describes correctly is called as ever.
    let o = run(mcpdial(&home).args([
        "call",
        &s.url,
        "add",
        r#"{"a":40,"b":2}"#,
        "--check",
        &older,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 40 and 2 is 42.");
    assert!(
        o.stderr.is_empty(),
        "a check that passes says nothing: {}",
        o.stderr
    );
}

#[test]
fn a_change_that_breaks_nobody_passes_until_strict_asks_for_the_object_back() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-strict");
    let (_, mut document) = snapshot_of(&home, &s.url, "tools.json");

    // A snapshot from back when echo also took an optional `encoding`, and
    // said so in other words. A caller holding it breaks on neither.
    document["tools"][1]["description"] = json!("Echo something back.");
    document["tools"][1]["inputSchema"]["properties"]["encoding"] = json!({"type": "string"});
    let older = home.join("older.json");
    std::fs::write(&older, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let older = older.to_str().unwrap().to_string();

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "info: tool echo: optional property encoding was removed",
            "ok",
        ],
        "a compatible change is still shown"
    );

    let o = run(mcpdial(&home).args(["tools", &s.url, "--check", &older, "--strict"]));
    assert_eq!(o.code, 3, "{}", o.stderr);
    assert_eq!(
        o.stdout.lines().collect::<Vec<_>>(),
        [
            "tool echo: optional property encoding was removed",
            "tool echo: description differs from the snapshot",
            "2 differences",
        ]
    );
}

#[test]
fn a_snapshot_path_that_cannot_hold_a_file_is_refused_before_the_server_is_dialed() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-refuse");
    let taken = home.join("taken.json");
    std::fs::write(&taken, "keep me").unwrap();
    let directory = home.join("adir");
    std::fs::create_dir(&directory).unwrap();

    let refused = |path: &std::path::Path, expected: &str| {
        let o = run(mcpdial(&home).args(["tools", &s.url, "--snapshot", path.to_str().unwrap()]));
        assert_eq!(o.code, 2, "{}", o.stderr);
        assert!(o.stderr.contains(expected), "{}", o.stderr);
        assert!(o.stdout.is_empty(), "{}", o.stdout);
    };
    refused(&taken, "already exists; name a path that does not");
    refused(&directory, "is a directory; name the file to write");
    refused(&home.join("nowhere/deep.json"), "is not a directory");

    assert_eq!(
        std::fs::read_to_string(&taken).unwrap(),
        "keep me",
        "a file mcpdial did not make is never written over"
    );
    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a path that could never be written costs no connection"
    );
}

/// Only a Unix test: the mode bits are what make a directory unwritable, and
/// Windows has nothing that answers to `chmod`.
#[cfg(unix)]
#[test]
fn a_directory_the_process_cannot_write_to_leaves_no_snapshot_behind() {
    use std::os::unix::fs::PermissionsExt;

    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-readonly");
    let sealed = home.join("sealed");
    std::fs::create_dir(&sealed).unwrap();
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500)).unwrap();

    // Mode bits do not restrain root, which is what a container runs the suite
    // as, so the premise is checked rather than assumed: a directory this
    // process can still write to is not the one the test is about.
    let probe = sealed.join("writable");
    if std::fs::write(&probe, "").is_ok() {
        std::fs::remove_file(&probe).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: this process writes to a directory it has no write bit on");
        return;
    }

    let path = sealed.join("tools.json");
    let o = run(mcpdial(&home).args(["tools", &s.url, "--snapshot", path.to_str().unwrap()]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("--snapshot"), "{}", o.stderr);
    assert!(!path.exists(), "nothing half-written is left behind");

    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_check_file_that_is_not_a_snapshot_is_a_usage_error_and_never_a_pass() {
    let s = start(Mode::Stateless);
    let home = temp_home("snapshot-unreadable");
    let not_json = home.join("not.json");
    std::fs::write(&not_json, "tools: []").unwrap();
    let wrong_shape = home.join("wrong.json");
    std::fs::write(&wrong_shape, r#"{"servers": []}"#).unwrap();

    let refused = |path: &std::path::Path, expected: &str| {
        let o = run(mcpdial(&home).args(["tools", &s.url, "--check", path.to_str().unwrap()]));
        assert_eq!(
            o.code, 2,
            "not 0, and not the 3 that means drift: {}",
            o.stderr
        );
        assert!(o.stderr.contains(expected), "{}", o.stderr);
        assert!(o.stdout.is_empty(), "{}", o.stdout);
    };
    refused(&home.join("missing.json"), "--check");
    refused(&not_json, "not JSON");
    refused(&wrong_shape, "no tools array");

    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a snapshot that is not one costs no connection"
    );
}
