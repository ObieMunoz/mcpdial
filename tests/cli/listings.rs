//! `ls` and `tools` across saved servers, and the status a probe remembers.

use crate::common::{echo_command, mcpdial, run, start, temp_home, Mode};
// The only test that spawns the real server is the Unix-only one below.
#[cfg(unix)]
use crate::common::echo_server;
use serde_json::{json, Value};

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
