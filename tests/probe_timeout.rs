//! A status probe gives up on a server that never answers after ten seconds, not
//! the sixty a call waits, unless `--timeout` says otherwise.
//!
//! Each test reads two clocks: how long the command took, and - from `--trace` -
//! how long the client itself waited before giving up. The second is the one
//! that says which timeout was in force. Only the wall clock was checked here
//! before, and a wall clock cannot tell a probe that reached for the wrong
//! bound from a machine that was busy.

mod common;

use common::*;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// The default probe timeout, and the call timeout it stands in for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

fn state_of(row: &Value) -> String {
    row["status"]["state"].as_str().unwrap_or("").to_string()
}

fn unreachable_in_time(row: &Value, stdout: &str) {
    assert_eq!(state_of(row), "unreachable", "{stdout}");
    assert!(
        row["status"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("in time")),
        "{stdout}"
    );
}

/// Every wait `--trace` recorded the client giving up after, in the order it
/// made them.
///
/// The wall clock says how long the command took; this says which timeout was
/// in force, which is the thing `--timeout` is there to decide. Read from
/// inside the client, so a busy machine, a slow spawn and anything else the
/// suite is doing at the time are not in the number. A probe that reached for
/// the default instead of the flag reads ten seconds here on every run, rather
/// than on the one run in fifty where the wall clock happens to notice.
fn gave_up_after(trace: &Path) -> Vec<Duration> {
    std::fs::read_to_string(trace)
        .unwrap_or_else(|e| panic!("{}: {e}", trace.display()))
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a trace line is JSON"))
        .filter(|rec| {
            rec["detail"]["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("timeout"))
        })
        .filter_map(|rec| rec["detail"]["elapsed_ms"].as_u64())
        .map(Duration::from_millis)
        .collect()
}

/// The trace's account of what the client waited, and that it waited at all: a
/// black hole that started answering, or failing some other way, would leave
/// every bound below trivially true.
fn every_wait(trace: &Path) -> Vec<Duration> {
    let waits = gave_up_after(trace);
    assert!(
        !waits.is_empty(),
        "{}: the client never timed out at all",
        trace.display()
    );
    waits
}

#[test]
fn ls_gives_up_on_a_silent_server_within_the_probe_timeout() {
    let s = start(Mode::BlackHole);
    let home = temp_home("probe-timeout-ls");
    let trace = home.join("ls.trace");
    assert_eq!(
        run(mcpdial(&home).args(["add", "hole", "--http", &s.url, "--no-probe"])).code,
        0
    );

    // No --timeout: this is the wait the issue is about.
    let (took, o) = timed(
        mcpdial(&home)
            .arg("--trace")
            .arg(&trace)
            .args(["--json", "ls"]),
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    for waited in every_wait(&trace) {
        assert!(
            waited >= PROBE_TIMEOUT.mul_f64(0.9),
            "the probe gave up after {waited:?}, before the probe timeout"
        );
        assert!(
            waited < CALL_TIMEOUT / 2,
            "the probe waited {waited:?}: the call timeout, not the probe timeout"
        );
    }
    assert!(
        took >= PROBE_TIMEOUT,
        "gave up after {took:?}, before the probe timeout"
    );
    assert!(
        took < CALL_TIMEOUT / 2,
        "waited {took:?}: the call timeout, not the probe timeout"
    );
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows.len(), 1, "{}", o.stdout);
    unreachable_in_time(&rows[0], &o.stdout);
}

#[test]
fn add_gives_up_on_a_silent_server_within_the_probe_timeout() {
    let s = start(Mode::BlackHole);
    let home = temp_home("probe-timeout-add");
    let trace = home.join("add.trace");

    let (took, o) = timed(
        mcpdial(&home)
            .arg("--trace")
            .arg(&trace)
            .args(["--json", "add", "hole", "--http", &s.url]),
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    for waited in every_wait(&trace) {
        assert!(
            waited >= PROBE_TIMEOUT.mul_f64(0.9),
            "the probe gave up after {waited:?}, before the probe timeout"
        );
        assert!(
            waited < CALL_TIMEOUT / 2,
            "the probe waited {waited:?}: the call timeout, not the probe timeout"
        );
    }
    assert!(
        took >= PROBE_TIMEOUT,
        "gave up after {took:?}, before the probe timeout"
    );
    assert!(
        took < CALL_TIMEOUT / 2,
        "waited {took:?}: the call timeout, not the probe timeout"
    );
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    unreachable_in_time(&receipt["saved"], &o.stdout);
}

#[test]
fn an_explicit_timeout_bounds_the_probe_too() {
    let s = start(Mode::BlackHole);
    let home = temp_home("probe-timeout-explicit");
    let adding = home.join("add.trace");

    let (took, o) = timed(mcpdial(&home).arg("--trace").arg(&adding).args([
        "--timeout",
        "1",
        "--json",
        "add",
        "hole",
        "--http",
        &s.url,
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    for waited in every_wait(&adding) {
        assert!(
            waited < PROBE_TIMEOUT / 2,
            "add's probe waited {waited:?} with --timeout 1"
        );
    }
    assert!(
        took < PROBE_TIMEOUT / 2,
        "add waited {took:?} with --timeout 1"
    );
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    unreachable_in_time(&receipt["saved"], &o.stdout);

    // A home of its own, holding a server saved with no timeout of its own:
    // `add` writes its `--timeout` into the entry, so a listing over the one
    // above would be bounded by what was saved whether or not the flag ever
    // reached the probe. Here the flag is the only thing that can bound it.
    let home = temp_home("probe-timeout-explicit-ls");
    let listing = home.join("ls.trace");
    assert_eq!(
        run(mcpdial(&home).args(["add", "hole", "--http", &s.url, "--no-probe"])).code,
        0
    );

    let (took, o) = timed(mcpdial(&home).arg("--trace").arg(&listing).args([
        "--timeout",
        "1",
        "--json",
        "ls",
        "--refresh",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    for waited in every_wait(&listing) {
        assert!(
            waited < PROBE_TIMEOUT / 2,
            "ls's probe waited {waited:?} with --timeout 1"
        );
    }
    assert!(
        took < PROBE_TIMEOUT / 2,
        "ls waited {took:?} with --timeout 1"
    );
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows.len(), 1, "{}", o.stdout);
    unreachable_in_time(&rows[0], &o.stdout);
}
