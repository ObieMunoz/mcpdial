//! Saving a server, and the four places a configuration can come from.

use crate::common::{echo_command, echo_server, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;

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
