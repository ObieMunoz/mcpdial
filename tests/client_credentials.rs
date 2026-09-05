//! `login --grant client-credentials`: a confidential client gets a token on its own
//! behalf, with no browser, and keeps it renewed the same way.

mod common;

use common::{mcpdial, run, start, temp_home, FakeServer, Mode, Out, Recorded, CONFIDENTIAL_ID};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

const SECRET: &str = common::CONFIDENTIAL_SECRET;
const SECRET_ENV: &str = "MCPDIAL_TEST_CLIENT_SECRET";

fn add_work(home: &std::path::Path, s: &FakeServer) {
    let o = run(mcpdial(home).args(["add", "work", "--http", &s.url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

/// The grant with the secret in the environment. stdin is closed, so a login that
/// tried to read or wait for anything there would fail rather than hang.
fn login_with_env_secret(home: &std::path::Path, extra: &[&str]) -> Command {
    let mut cmd = mcpdial(home);
    cmd.args([
        "login",
        "work",
        "--grant",
        "client-credentials",
        "--client-id",
        CONFIDENTIAL_ID,
        "--client-secret-env",
        SECRET_ENV,
    ])
    .args(extra)
    .env(SECRET_ENV, SECRET)
    .stdin(Stdio::null());
    cmd
}

fn login_with_stdin_secret(home: &std::path::Path) -> Out {
    let mut child = mcpdial(home)
        .args([
            "login",
            "work",
            "--grant",
            "client-credentials",
            "--client-id",
            CONFIDENTIAL_ID,
            "--client-secret",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SECRET.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn saved_credential(home: &std::path::Path) -> Value {
    let text = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["credentials"]["work"].clone()
}

fn expire_saved_token(home: &std::path::Path) {
    let path = home.join("credentials.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(path, v.to_string()).unwrap();
}

fn token_requests(s: &FakeServer) -> Vec<Recorded> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/token")
        .cloned()
        .collect()
}

fn browser_flow_requests(s: &FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.starts_with("/authorize") || r.path == "/register")
        .map(|r| r.path.clone())
        .collect()
}

fn form_field<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    body.split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
}

#[test]
fn the_grant_posts_the_secret_and_saves_a_credential_that_renews_itself() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("client-credentials-post");
    add_work(&home, &s);

    let o = run(&mut login_with_env_secret(&home, &[]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("saved token for work (expires in 1m, refreshable)"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("authorization server:"), "{}", o.stderr);
    assert!(
        browser_flow_requests(&s).is_empty(),
        "no registration and no authorization request"
    );

    let requests = token_requests(&s);
    assert_eq!(requests.len(), 1, "{requests:?}");
    let grant = &requests[0];
    assert_eq!(
        form_field(&grant.body, "grant_type"),
        Some("client_credentials")
    );
    assert_eq!(form_field(&grant.body, "client_id"), Some(CONFIDENTIAL_ID));
    assert_eq!(
        form_field(&grant.body, "client_secret"),
        Some("c0nf%26s3cr3t%3D%2F%3Ax")
    );
    assert_eq!(form_field(&grant.body, "scope"), Some("mcp"));
    assert_eq!(
        form_field(&grant.body, "resource"),
        Some(mcpdial::oauth::urlencode(&s.url).as_str())
    );
    assert!(form_field(&grant.body, "code").is_none(), "{grant:?}");
    assert!(
        form_field(&grant.body, "redirect_uri").is_none(),
        "{grant:?}"
    );
    assert_eq!(grant.header("authorization"), None, "post auth, not basic");

    let cred = saved_credential(&home);
    assert_eq!(cred["source"], "client_credentials");
    assert_eq!(cred["client_id"], CONFIDENTIAL_ID);
    assert_eq!(cred["registration"], "pre-registered");
    assert_eq!(cred["client_secret"], SECRET);
    assert_eq!(cred["token_endpoint"], format!("{}/token", s.base));
    assert_eq!(cred["token_endpoint_auth_method"], "client_secret_post");
    assert_eq!(cred["scope"], "mcp");
    assert_eq!(cred["resource"], s.url);
    assert_eq!(cred["access_token"], "tok-1");
    assert!(cred["expires_at"].as_u64().unwrap() > mcpdial::config::now());
    assert!(cred.get("refresh_token").is_none(), "{cred}");

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("source:        client_credentials"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("expiry:        expires in"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout
            .contains("client secret: present (client_secret_post)"),
        "{}",
        o.stdout
    );
    assert!(!o.stdout.contains(SECRET), "never print the secret");
    let o = run(mcpdial(&home).args(["token", "show", "work", "--json"]));
    let shown: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(shown["source"], "client_credentials");
    assert_eq!(shown["has_refresh_token"], false);
    assert_eq!(shown["has_client_secret"], true);
    assert!(shown["expires_at"].is_u64(), "{shown}");
    assert!(!o.stdout.contains(SECRET), "never print the secret");

    // Unattended from here on: the expired token is replaced by running the grant
    // again, with nothing read from stdin and no browser.
    expire_saved_token(&home);
    let o = run(mcpdial(&home)
        .args(["call", "work", "echo", r#"{"message":"renewed"}"#])
        .stdin(Stdio::null()));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: renewed");
    let requests = token_requests(&s);
    assert_eq!(requests.len(), 2, "{requests:?}");
    let renewal = &requests[1];
    assert_eq!(
        form_field(&renewal.body, "grant_type"),
        Some("client_credentials")
    );
    assert!(
        form_field(&renewal.body, "refresh_token").is_none(),
        "{renewal:?}"
    );
    assert_eq!(
        form_field(&renewal.body, "client_secret"),
        Some("c0nf%26s3cr3t%3D%2F%3Ax")
    );
    assert_eq!(form_field(&renewal.body, "scope"), Some("mcp"));
    assert_eq!(
        form_field(&renewal.body, "resource"),
        Some(mcpdial::oauth::urlencode(&s.url).as_str())
    );
    assert!(browser_flow_requests(&s).is_empty());
    let cred = saved_credential(&home);
    assert_eq!(cred["access_token"], "tok-2");
    assert_eq!(cred["source"], "client_credentials");
    assert!(cred["expires_at"].as_u64().unwrap() > mcpdial::config::now());
}

#[test]
fn the_grant_uses_basic_auth_when_that_is_all_the_server_offers() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_basic".into(),
    });
    let home = temp_home("client-credentials-basic");
    add_work(&home, &s);

    let o = login_with_stdin_secret(&home);
    assert_eq!(o.code, 0, "{}", o.stderr);

    let requests = token_requests(&s);
    assert_eq!(requests.len(), 1, "{requests:?}");
    let grant = &requests[0];
    assert!(!grant.body.contains("client_secret"), "{grant:?}");
    assert_eq!(
        grant.header("authorization"),
        Some("Basic Y29uZi1jbGllbnQ6YzBuZiUyNnMzY3IzdCUzRCUyRiUzQXg=")
    );
    assert_eq!(
        saved_credential(&home)["token_endpoint_auth_method"],
        "client_secret_basic"
    );

    expire_saved_token(&home);
    let o = run(mcpdial(&home)
        .args(["call", "work", "echo", r#"{"message":"basic"}"#])
        .stdin(Stdio::null()));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: basic");
    let renewal = token_requests(&s).pop().unwrap();
    assert_eq!(
        form_field(&renewal.body, "grant_type"),
        Some("client_credentials")
    );
    assert!(!renewal.body.contains("client_secret"), "{renewal:?}");
    assert!(renewal.header("authorization").is_some(), "{renewal:?}");
}

#[test]
fn a_wrong_secret_is_the_servers_refusal() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("client-credentials-wrong");
    add_work(&home, &s);

    let o = run(login_with_env_secret(&home, &[]).env(SECRET_ENV, "not-it"));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("client credentials grant failed: HTTP 401"),
        "{}",
        o.stderr
    );
    assert!(!home.join("credentials.json").exists(), "nothing to save");
}

#[test]
fn a_server_without_the_grant_is_refused_with_what_it_does_offer() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("client-credentials-unsupported");
    add_work(&home, &s);

    let o = run(&mut login_with_env_secret(&home, &[]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains(
            "does not offer the client_credentials grant; it supports: authorization_code, refresh_token"
        ),
        "{}",
        o.stderr
    );
    assert!(token_requests(&s).is_empty(), "nothing may be requested");

    // The progress lines come first; the error object is the last line.
    let o = run(&mut login_with_env_secret(&home, &["--json"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let last = o.stderr.trim().lines().last().unwrap();
    let e: Value = serde_json::from_str(last).unwrap();
    assert_eq!(e["error"]["kind"], "usage", "{}", o.stderr);
}

#[test]
fn the_browser_flags_and_a_missing_client_are_usage_errors() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("client-credentials-flags");
    add_work(&home, &s);

    for flags in [
        &["--port", "1"][..],
        &["--redirect-host", "localhost"][..],
        &["--no-browser"][..],
    ] {
        let o = run(&mut login_with_env_secret(&home, flags));
        assert_eq!(o.code, 2, "{flags:?}: {}", o.stderr);
        assert!(
            o.stderr.contains(flags[0]),
            "{flags:?} must be named: {}",
            o.stderr
        );
    }

    let o = run(mcpdial(&home)
        .args(["login", "work", "--grant", "client-credentials"])
        .stdin(Stdio::null()));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("--client-id"), "{}", o.stderr);

    let o = run(mcpdial(&home)
        .args([
            "login",
            "work",
            "--grant",
            "client-credentials",
            "--client-id",
            CONFIDENTIAL_ID,
        ])
        .stdin(Stdio::null()));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("--client-secret"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["login", "work", "--grant", "password"]));
    assert_eq!(o.code, 2, "{}", o.stderr);

    assert!(
        s.requests.lock().unwrap().is_empty(),
        "a usage error sends nothing"
    );
}

#[test]
fn the_secret_reaches_neither_an_argument_nor_the_trace_output() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("client-credentials-quiet");
    add_work(&home, &s);

    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--grant",
        "client-credentials",
        "--client-id",
        CONFIDENTIAL_ID,
        "--client-secret",
        SECRET,
    ]));
    assert_eq!(o.code, 2, "the flag must take no value: {}", o.stderr);

    let o = run(&mut login_with_env_secret(&home, &["-v"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("authorization server:"), "{}", o.stderr);
    assert!(
        !o.stderr.contains(SECRET) && !o.stdout.contains(SECRET),
        "the secret leaked into -v:\n{}",
        o.stderr
    );

    expire_saved_token(&home);
    let o = run(mcpdial(&home)
        .args(["call", "work", "echo", r#"{"message":"x"}"#, "-v"])
        .stdin(Stdio::null()));
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
