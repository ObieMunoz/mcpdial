//! The browser step, and everything that decides whether a token is redeemed.

use crate::common::{mcpdial, run, start, temp_home, Iss, Mode};
use crate::confidential::last_token_request;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::Stdio;

/// Run `login` with no browser, scrape the auth URL from stderr, and play the browser
/// ourselves: the fake authorization server 302s straight back to the loopback callback.
fn drive_login(home: &std::path::Path, target: &str, extra: &[&str]) -> String {
    drive_login_out(home, target, extra).0
}

/// [`drive_login`], handing back stderr and then whatever went to stdout.
pub(crate) fn drive_login_out(
    home: &std::path::Path,
    target: &str,
    extra: &[&str],
) -> (String, String) {
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

fn add_auth_server(home: &std::path::Path, s: &crate::common::FakeServer) {
    assert_eq!(
        run(mcpdial(home).args(["add", "work", "--http", &s.url])).code,
        0
    );
}

fn registrations(s: &crate::common::FakeServer) -> usize {
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
