//! Client ID metadata documents: a URL as the client id, in place of dynamic
//! registration, when the authorization server says it fetches such documents.

mod common;

use common::{mcpdial, run, start, temp_home, FakeServer, Mode};
use mcpdial::oauth::{urlencode, CLIENT_METADATA_URL};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn home_with_work(tag: &str, s: &FakeServer) -> PathBuf {
    let home = temp_home(tag);
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", &s.url])).code,
        0
    );
    home
}

/// `login --no-browser`, with this test playing the browser: the fake authorization
/// server 302s straight back to the loopback callback. Returns the authorization URL
/// and the stderr log.
fn drive_login(home: &Path, extra: &[&str]) -> (String, String) {
    let mut child = mcpdial(home)
        .args(["login", "work", "--no-browser"])
        .args(extra)
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
    assert!(status.success(), "login exited {status}:\n{log}");
    (auth_url, log)
}

fn registered_dynamically(s: &FakeServer) -> bool {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.path == "/register")
}

fn client_id_param(url: &str) -> String {
    format!("client_id={}", urlencode(url))
}

fn credentials(home: &Path) -> String {
    std::fs::read_to_string(home.join("credentials.json")).unwrap()
}

#[test]
fn login_presents_the_document_url_when_the_server_advertises_support() {
    let s = start(Mode::AuthClientMetadata {
        client_id: CLIENT_METADATA_URL.into(),
    });
    let home = home_with_work("cimd", &s);

    let (auth_url, log) = drive_login(&home, &[]);
    assert!(
        auth_url.contains(&client_id_param(CLIENT_METADATA_URL)),
        "{auth_url}"
    );
    assert!(
        log.contains("registered via client metadata document"),
        "{log}"
    );
    assert!(!log.contains("registered client"), "{log}");
    assert!(!registered_dynamically(&s), "nothing to register");

    let creds = credentials(&home);
    assert!(
        creds.contains(&format!("\"client_id\": \"{CLIENT_METADATA_URL}\"")),
        "{creds}"
    );
    assert!(
        creds.contains("\"registration\": \"client_metadata_document\""),
        "{creds}"
    );

    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert_eq!(o.code, 0);
    assert!(
        o.stdout
            .contains("registered:    via client metadata document"),
        "{}",
        o.stdout
    );
    let o = run(mcpdial(&home).args(["--json", "token", "show", "work"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["client_id"], CLIENT_METADATA_URL);
    assert_eq!(v["registration"], "client_metadata_document");

    // The token works, and a refresh needs nothing beyond the URL as client id.
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"ok"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let mut v: Value = serde_json::from_str(&creds).unwrap();
    v["credentials"]["work"]["expires_at"] = Value::from(1_000_000u64);
    std::fs::write(home.join("credentials.json"), v.to_string()).unwrap();
    let o = run(mcpdial(&home).args(["call", "work", "echo", r#"{"message":"refreshed"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "Echo: refreshed");
    let refresh = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.path == "/token")
        .cloned()
        .expect("a token request");
    assert!(
        refresh.body.contains("grant_type=refresh_token"),
        "{}",
        refresh.body
    );
    assert!(
        refresh.body.contains(&client_id_param(CLIENT_METADATA_URL)),
        "{}",
        refresh.body
    );

    // A second login presents the document again; there is no registration to reuse.
    let (auth_url, _) = drive_login(&home, &[]);
    assert!(
        auth_url.contains(&client_id_param(CLIENT_METADATA_URL)),
        "{auth_url}"
    );
    assert!(!registered_dynamically(&s));
}

#[test]
fn login_registers_dynamically_when_the_server_does_not_advertise_documents() {
    let s = start(Mode::Auth { tokens: vec![] });
    let home = home_with_work("cimd-dcr", &s);

    let (auth_url, log) = drive_login(&home, &[]);
    assert!(auth_url.contains("client_id=client-abc"), "{auth_url}");
    assert!(log.contains("registered client client-abc"), "{log}");
    assert!(!log.contains("client metadata document"), "{log}");
    assert!(registered_dynamically(&s));
    assert!(
        credentials(&home).contains("\"registration\": \"dynamic\""),
        "{}",
        credentials(&home)
    );
    let o = run(mcpdial(&home).args(["token", "show", "work"]));
    assert!(
        o.stdout.contains("registered:    dynamically"),
        "{}",
        o.stdout
    );
}

#[test]
fn a_custom_document_url_is_what_the_authorize_request_carries() {
    let url = "https://example.com/mcp/my-client.json";
    let s = start(Mode::AuthClientMetadata {
        client_id: url.into(),
    });
    let home = home_with_work("cimd-custom", &s);

    let (auth_url, log) = drive_login(&home, &["--client-metadata-url", url]);
    assert!(auth_url.contains(&client_id_param(url)), "{auth_url}");
    assert!(
        log.contains(&format!(
            "client {url}, registered via client metadata document"
        )),
        "{log}"
    );
    assert!(!registered_dynamically(&s));
    assert!(
        credentials(&home).contains(&format!("\"client_id\": \"{url}\"")),
        "{}",
        credentials(&home)
    );
}

#[test]
fn no_client_metadata_registers_dynamically_on_a_document_server() {
    let s = start(Mode::AuthClientMetadata {
        client_id: CLIENT_METADATA_URL.into(),
    });
    let home = home_with_work("cimd-off", &s);

    let (auth_url, log) = drive_login(&home, &["--no-client-metadata"]);
    assert!(auth_url.contains("client_id=client-abc"), "{auth_url}");
    assert!(log.contains("registered client client-abc"), "{log}");
    assert!(registered_dynamically(&s));
    assert!(
        credentials(&home).contains("\"client_id\": \"client-abc\""),
        "{}",
        credentials(&home)
    );
}

#[test]
fn a_document_url_must_be_https_with_a_path() {
    let home = temp_home("cimd-bad-url");
    assert_eq!(
        run(mcpdial(&home).args(["add", "work", "--http", "http://127.0.0.1:9/mcp"])).code,
        0
    );
    for bad in [
        "http://example.com/client.json",
        "https://example.com",
        "https://example.com/",
    ] {
        let o = run(mcpdial(&home).args([
            "login",
            "work",
            "--no-browser",
            "--client-metadata-url",
            bad,
        ]));
        assert_eq!(o.code, 2, "{bad}: {}", o.stderr);
        assert!(
            o.stderr.contains("must be https with a path"),
            "{bad}: {}",
            o.stderr
        );
    }

    // The two flags contradict each other.
    let o = run(mcpdial(&home).args([
        "login",
        "work",
        "--no-browser",
        "--client-metadata-url",
        "https://example.com/client.json",
        "--no-client-metadata",
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
}
