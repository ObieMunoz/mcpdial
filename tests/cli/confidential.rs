//! Clients that hold a secret, and tokens saved by hand.

use crate::common::{
    mcpdial, run, start, temp_home, Mode, CONFIDENTIAL_ID, CONFIDENTIAL_SECRET as SECRET,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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

pub(crate) fn last_token_request(s: &crate::common::FakeServer) -> crate::common::Recorded {
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
