//! `${VAR}` placeholders in a saved server are filled in from the environment when
//! the server is dialed, and only then: the file and the listing keep the name.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode, CONFIDENTIAL_ID};
use serde_json::Value;
use std::process::Stdio;

#[test]
fn a_saved_header_placeholder_is_filled_in_from_the_environment() {
    let s = start(Mode::Stateless);
    let home = temp_home("placeholders");
    let o = run(mcpdial(&home).args([
        "add",
        "t",
        "--http",
        &s.url,
        "-H",
        "X-Key: ${MCPDIAL_TEST_KEY}",
        "-H",
        "X-Plan: ${MCPDIAL_TEST_PLAN:-free} for $$5",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    // The file and the listing keep the placeholder, never the value.
    let saved = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert!(saved.contains("${MCPDIAL_TEST_KEY}"), "{saved}");
    let o =
        run(mcpdial(&home)
            .env("MCPDIAL_TEST_KEY", "s3cret")
            .args(["ls", "--no-probe", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["headers"]["X-Key"], "${MCPDIAL_TEST_KEY}");
    assert!(!o.stdout.contains("s3cret"), "{}", o.stdout);

    let o = run(mcpdial(&home)
        .env("MCPDIAL_TEST_KEY", "s3cret")
        .env_remove("MCPDIAL_TEST_PLAN")
        .args(["info", "t"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let reqs = s.requests.lock().unwrap();
    assert!(!reqs.is_empty());
    for sent in reqs.iter() {
        assert_eq!(sent.header("x-key"), Some("s3cret"), "{sent:?}");
        assert_eq!(sent.header("x-plan"), Some("free for $5"), "{sent:?}");
    }
    let saved_after = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert_eq!(saved_after, saved, "never written back");
}

#[test]
fn an_unset_placeholder_is_a_config_error_before_anything_is_sent() {
    let s = start(Mode::Stateless);
    let home = temp_home("placeholders-unset");
    let o = run(mcpdial(&home).args([
        "add",
        "t",
        "--http",
        &s.url,
        "-H",
        "X-Key: ${MCPDIAL_TEST_KEY}",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home)
        .env_remove("MCPDIAL_TEST_KEY")
        .args(["info", "t"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("header X-Key refers to $MCPDIAL_TEST_KEY, which is not set"),
        "{}",
        o.stderr
    );
    assert!(s.requests.lock().unwrap().is_empty(), "nothing may be sent");

    let o = run(mcpdial(&home)
        .env_remove("MCPDIAL_TEST_KEY")
        .args(["info", "t", "--json"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "config");
    assert!(s.requests.lock().unwrap().is_empty(), "nothing may be sent");

    // `ls` reports it as this server's status rather than failing the listing.
    let o = run(mcpdial(&home)
        .env_remove("MCPDIAL_TEST_KEY")
        .args(["ls", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "error");
    assert!(
        rows[0]["status"]["detail"]
            .as_str()
            .unwrap()
            .contains("$MCPDIAL_TEST_KEY"),
        "{}",
        o.stdout
    );
    assert!(s.requests.lock().unwrap().is_empty(), "nothing may be sent");
}

#[test]
fn a_stdio_server_gets_its_env_and_command_line_expanded() {
    let home = temp_home("placeholders-stdio");
    let echo = echo_command();
    let o = run(mcpdial(&home).args([
        "add",
        "tagged",
        "--stdio",
        &echo,
        "--env",
        "ECHO_SERVER_TAG=${MCPDIAL_TEST_TAG}",
        "--cwd",
        "${MCPDIAL_TEST_DIR:-.}",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home)
        .env("MCPDIAL_TEST_TAG", "from-env")
        .env_remove("MCPDIAL_TEST_DIR")
        .args(["info", "tagged"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("tag=from-env"), "{}", o.stdout);

    let o = run(mcpdial(&home)
        .env_remove("MCPDIAL_TEST_TAG")
        .args(["info", "tagged"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("env ECHO_SERVER_TAG refers to $MCPDIAL_TEST_TAG, which is not set"),
        "{}",
        o.stderr
    );

    // An ad-hoc target goes through the same expansion.
    let o = run(mcpdial(&home)
        .env("MCPDIAL_TEST_ECHO", &echo)
        .args(["info", "stdio:${MCPDIAL_TEST_ECHO}"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

#[test]
fn login_fills_in_a_placeholder_in_the_url_before_discovery() {
    let s = start(Mode::Confidential {
        auth_method: "client_secret_post".into(),
    });
    let home = temp_home("placeholders-login");
    let o = run(mcpdial(&home).args([
        "add",
        "work",
        "--http",
        "http://${MCPDIAL_TEST_HOST}/mcp",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let host = s.base.strip_prefix("http://").unwrap();

    // The client-credentials grant is the login that needs no browser to finish.
    let mut login = mcpdial(&home);
    login
        .args([
            "login",
            "work",
            "--grant",
            "client-credentials",
            "--client-id",
            CONFIDENTIAL_ID,
            "--client-secret-env",
            "MCPDIAL_TEST_SECRET",
        ])
        .env("MCPDIAL_TEST_SECRET", common::CONFIDENTIAL_SECRET)
        .stdin(Stdio::null());

    let o = run(login.env_remove("MCPDIAL_TEST_HOST"));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("http URL refers to $MCPDIAL_TEST_HOST, which is not set"),
        "{}",
        o.stderr
    );
    assert!(s.requests.lock().unwrap().is_empty(), "nothing may be sent");

    let o = run(login.env("MCPDIAL_TEST_HOST", host));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let creds = std::fs::read_to_string(home.join("credentials.json")).unwrap();
    let cred = &serde_json::from_str::<Value>(&creds).unwrap()["credentials"]["work"];
    assert_eq!(
        cred["resource"], s.url,
        "the token is bound to the expanded URL"
    );
    let saved = std::fs::read_to_string(home.join("servers.json")).unwrap();
    assert!(saved.contains("http://${MCPDIAL_TEST_HOST}/mcp"), "{saved}");
    assert!(!saved.contains(host), "{saved}");
}
