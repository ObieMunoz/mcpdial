//! Following what a server says has changed, from either side of the 2026-07-28
//! break: `resources/subscribe` before it, `subscriptions/listen` after it.
//!
//! Every run here is a piped shell reading a script and ending at EOF, and every
//! one is held to a wall clock. A shell that waits for something a server will
//! never send is the bug these tests exist to catch, and a test that waits with
//! it reports nothing at all.

mod common;

use common::{echo_command, echo_server, mcpdial, start, temp_home, FakeServer, Mode};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every shell in this file finishes well inside this. It is generous on
/// purpose: what it catches is a wait with no end, not a slow machine.
const PATIENCE: Duration = Duration::from_secs(60);

struct Said {
    stdout: String,
    stderr: String,
    code: i32,
    took: Duration,
}

impl Said {
    /// The `{"notification": ...}` objects `--json` put on stderr, in order.
    fn notifications(&self) -> Vec<Value> {
        self.stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v.get("notification").is_some())
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.notifications()
            .iter()
            .filter_map(|v| Some(v["notification"]["method"].as_str()?.to_string()))
            .collect()
    }
}

/// Pipe `script` into a shell and wait for it, but never for longer than
/// [`PATIENCE`]: a shell that will not end is killed and reported as a failure
/// rather than left to hold the suite open.
fn shell(cmd: &mut Command, script: &str) -> Said {
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcpdial");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())
        .expect("write the script");
    let mut ended = false;
    while !ended && started.elapsed() < PATIENCE {
        ended = matches!(child.try_wait(), Ok(Some(_)));
        if !ended {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if !ended {
        let _ = child.kill();
    }
    let out = child.wait_with_output().expect("collect what it said");
    assert!(ended, "the shell was still running after {PATIENCE:?}");
    Said {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
        took: started.elapsed(),
    }
}

/// A shell on the echo server with subscriptions turned on, reporting `updates`
/// resource changes per `count` call.
fn following_shell(home: &Path, updates: u32, json: bool) -> Command {
    let mut cmd = mcpdial(home);
    if json {
        cmd.arg("--json");
    }
    cmd.env("ECHO_SERVER_SUBSCRIBE", updates.to_string())
        // Long enough that nothing here is a timeout, short enough that a wait
        // with no end is a failure the suite survives.
        .args([
            "--timeout",
            "10",
            "shell",
            &format!("stdio:{}", echo_command()),
        ]);
    cmd
}

#[test]
fn a_followed_resource_is_rewritten_where_it_was_asked_for() {
    let home = temp_home("subs-file");
    let out = home.join("counter.txt");
    let script = format!(
        "subscribe counter://calls {}\nsubscriptions\ncall count {{}}\ncall count {{}}\nquit\n",
        out.display()
    );
    let said = shell(&mut following_shell(&home, 1, false), &script);

    assert_eq!(said.code, 0, "{}", said.stderr);
    assert!(
        said.stdout.contains("following counter://calls"),
        "{}",
        said.stdout
    );
    assert!(
        said.stdout.contains("1 subscription(s):"),
        "{}",
        said.stdout
    );
    // The file holds what the second call left behind, written at the prompt
    // after it rather than in the middle of its output.
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "count=2");
    let beside: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.contains("counter"))
        .collect();
    assert_eq!(beside, ["counter.txt"], "the sibling is renamed, not left");
}

#[test]
fn json_puts_every_notification_on_stderr_for_a_script_to_read() {
    let home = temp_home("subs-json");
    let out = home.join("counter.txt");
    let script = format!(
        "subscribe counter://calls {}\ncall count {{}}\ncall register_tool {{}}\ntools\nquit\n",
        out.display()
    );
    let said = shell(&mut following_shell(&home, 1, true), &script);

    assert_eq!(said.code, 0, "{}", said.stderr);
    assert_eq!(
        said.methods(),
        [
            "notifications/resources/updated",
            "notifications/tools/list_changed"
        ],
        "stderr: {}",
        said.stderr
    );
    let updated = &said.notifications()[0];
    assert_eq!(updated["notification"]["params"]["uri"], "counter://calls");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "count=1");

    // The tool the server added mid-session is in the listing that follows,
    // because the shell re-read it the moment it was told to.
    let listed: Vec<Value> = said
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("tools").is_some())
        .collect();
    let names: Vec<&str> = listed.last().expect("a tools listing")["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"registered"), "{names:?}");
}

/// A piped session's bytes are frozen, and a server it never heard of does not
/// get to move them: the plain run says nothing about either notification.
#[test]
fn a_pipe_reading_plain_text_hears_nothing_a_server_says_on_its_own() {
    let home = temp_home("subs-quiet");
    let out = home.join("counter.txt");
    let script = format!(
        "subscribe counter://calls {}\ncall count {{}}\ncall register_tool {{}}\nquit\n",
        out.display()
    );
    let said = shell(&mut following_shell(&home, 1, false), &script);

    assert_eq!(said.code, 0, "{}", said.stderr);
    assert_eq!(said.stderr, "", "a pipe hears nothing");
    assert!(
        !said.stdout.contains("changed"),
        "and reads nothing about it either: {}",
        said.stdout
    );
    // It still happened, though: the file is in step.
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "count=1");
}

/// The same standard the progress flood is held to: five hundred notifications
/// about one thing are one thing, and the prompt is not starved by them.
#[test]
fn a_flood_of_updates_is_one_refresh_and_leaves_the_output_alone() {
    let home = temp_home("subs-flood");
    let out = home.join("counter.txt");
    let script = format!(
        "subscribe counter://calls {}\ncall count {{}}\nquit\n",
        out.display()
    );
    let calm = shell(&mut following_shell(&home, 1, true), &script);
    let flooded = shell(&mut following_shell(&home, 500, true), &script);

    assert_eq!(flooded.code, calm.code, "{}", flooded.stderr);
    assert_eq!(flooded.stdout, calm.stdout, "stdout moved");
    assert_eq!(
        flooded.methods(),
        ["notifications/resources/updated"],
        "five hundred reports of one change are one refresh: {}",
        flooded.stderr
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "count=1");
    assert!(
        flooded.took < PATIENCE,
        "the prompt came back in {:?}",
        flooded.took
    );
}

#[test]
fn unsubscribing_from_something_never_subscribed_to_says_so() {
    let home = temp_home("subs-unknown");
    let said = shell(
        &mut following_shell(&home, 1, false),
        "unsubscribe counter://nope\nsubscribe counter://calls\nunsubscribe counter://calls\n\
         unsubscribe counter://calls\ncall echo {\"message\":\"still here\"}\nquit\n",
    );

    assert_eq!(said.code, 1, "a shell that printed an error exits 1");
    assert_eq!(
        said.stderr.matches("not subscribed to").count(),
        2,
        "{}",
        said.stderr
    );
    assert!(
        said.stderr.contains("`subscriptions` lists what"),
        "{}",
        said.stderr
    );
    assert!(
        said.stdout.contains("Echo: still here"),
        "and the session carries on: {}",
        said.stdout
    );
}

/// A resource can go away while somebody is following it. The read that follows
/// the notification fails, and that failure is reported once and counted, with
/// the session and every other subscription left running.
#[test]
fn a_followed_resource_that_vanishes_is_reported_and_the_session_goes_on() {
    let home = temp_home("subs-vanished");
    // The server accepts a subscription to a URI it does not have, reports it
    // changed on the next call like any other, and then cannot answer the read.
    let said = shell(
        &mut following_shell(&home, 1, false),
        "subscribe counter://gone\ncall count {}\ncall echo {\"message\":\"after\"}\nquit\n",
    );

    assert_eq!(said.code, 1, "the failure is worth an exit code");
    assert!(
        said.stderr.contains("Resource not found: counter://gone"),
        "{}",
        said.stderr
    );
    assert!(
        said.stdout.contains("Echo: after"),
        "the session survives it: {}",
        said.stdout
    );
}

/// `listen` belongs to the revision that has `subscriptions/listen`. On an
/// older one the updates are already at the prompt, so asking to wait for them
/// is refused with the reason rather than sent as a method the server lacks.
#[test]
fn listening_is_refused_on_a_revision_that_delivers_updates_another_way() {
    let home = temp_home("subs-legacy-listen");
    let said = shell(
        &mut following_shell(&home, 1, false),
        "listen 2\ncall echo {\"message\":\"after\"}\nquit\n",
    );

    assert_eq!(said.code, 1, "{}", said.stderr);
    assert!(
        said.stderr.contains("which has no subscriptions/listen"),
        "{}",
        said.stderr
    );
    assert!(
        said.stdout.contains("Echo: after"),
        "the session carries on: {}",
        said.stdout
    );
}

#[test]
fn a_server_with_no_subscriptions_to_offer_says_so_before_sending_anything() {
    let home = temp_home("subs-refused");
    // Without ECHO_SERVER_SUBSCRIBE the echo server declares no resources at all.
    let said = shell(
        mcpdial(&home).args([
            "--timeout",
            "10",
            "shell",
            &format!("stdio:{}", echo_command()),
        ]),
        "subscribe counter://calls\nquit\n",
    );

    assert_eq!(said.code, 1, "{}", said.stderr);
    assert!(
        said.stderr
            .contains("this server does not offer resource subscriptions"),
        "{}",
        said.stderr
    );
    assert!(
        said.stderr.contains("`info` shows what it does offer"),
        "{}",
        said.stderr
    );
}

// -- 2026-07-28 -----------------------------------------------------------

fn sent(s: &FakeServer, method: &str) -> Option<Value> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .find(|m| m["method"] == method)
}

/// On the revision that removed `resources/subscribe`, `subscribe` sends
/// nothing: the interest travels in the filter of the `listen` that follows.
#[test]
fn the_2026_revision_carries_subscriptions_in_the_listen_filter() {
    let s = start(Mode::Modern);
    let home = temp_home("subs-modern");
    let out = home.join("readme.md");
    let script = format!(
        "subscribe file:///readme.md {}\nsubscriptions\nlisten 10\nquit\n",
        out.display()
    );
    let said = shell(
        mcpdial(&home).args(["--json", "--timeout", "10", "shell", &s.url]),
        &script,
    );

    assert_eq!(said.code, 0, "{}", said.stderr);
    assert!(
        sent(&s, "resources/subscribe").is_none(),
        "the revision has no such method"
    );
    let listen = sent(&s, "subscriptions/listen").expect("one subscriptions/listen");
    let filter = &listen["params"]["notifications"];
    assert_eq!(
        filter["resourceSubscriptions"],
        serde_json::json!(["file:///readme.md"])
    );
    for opted_in in [
        "toolsListChanged",
        "resourcesListChanged",
        "promptsListChanged",
    ] {
        assert_eq!(filter[opted_in], serde_json::json!(true), "{opted_in}");
    }
    // Every request of this revision carries its own metadata, this one included.
    assert_eq!(
        listen["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        common::MODERN_VERSION
    );

    // What the stream said was acted on at the prompt after it: the lists it
    // said had changed first, then the resources it said were new.
    assert_eq!(
        said.methods(),
        [
            "notifications/tools/list_changed",
            "notifications/resources/updated"
        ],
        "stderr: {}",
        said.stderr
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "# fake-mcp\nA readme.\n"
    );
    let listened = said
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("listened").is_some())
        .expect("a listen receipt");
    assert_eq!(
        listened["listened"]["acknowledged"],
        serde_json::json!(true)
    );
    assert_eq!(
        listened["listened"]["closed"],
        serde_json::json!(true),
        "the server closed the subscription itself"
    );
}

/// A server that accepts the request and then says nothing holds the shell for
/// the bound that was asked for, and not one moment past it.
#[test]
fn a_stream_that_is_never_acknowledged_ends_at_its_bound() {
    let s = start(Mode::SilentSubscription);
    let home = temp_home("subs-silent");
    let started = Instant::now();
    let said = shell(
        // The bound is the listen's own, so a --timeout ten times longer than it
        // is what proves the bound is doing the work.
        mcpdial(&home).args(["--json", "--timeout", "600", "shell", &s.url]),
        "listen 2\nquit\n",
    );
    let took = started.elapsed();

    assert_eq!(said.code, 0, "{}", said.stderr);
    let listened = said
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("listened").is_some())
        .expect("a listen receipt");
    assert_eq!(
        listened["listened"]["acknowledged"],
        serde_json::json!(false)
    );
    assert_eq!(listened["listened"]["closed"], serde_json::json!(false));
    assert!(
        took < Duration::from_secs(60),
        "held for {took:?}, and --timeout was 600s"
    );
}

#[test]
fn a_bound_that_is_not_a_number_of_seconds_is_refused_before_anything_is_sent() {
    let s = start(Mode::Modern);
    let home = temp_home("subs-bad-bound");
    let said = shell(
        mcpdial(&home).args(["--timeout", "10", "shell", &s.url]),
        "listen soon\nquit\n",
    );

    assert_eq!(said.code, 1, "{}", said.stderr);
    assert!(
        said.stderr.contains("is not a number of seconds"),
        "{}",
        said.stderr
    );
    assert!(
        said.stderr.contains("usage: listen [SECONDS]"),
        "{}",
        said.stderr
    );
    assert!(sent(&s, "subscriptions/listen").is_none());
}

/// The example server has to be built for the stdio runs above; failing here
/// says so plainly rather than as a spawn error inside a shell.
#[test]
fn the_example_server_is_built() {
    assert!(echo_server().exists());
}
