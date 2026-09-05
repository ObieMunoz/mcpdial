//! A status probe gives up on a server that never answers after ten seconds, not
//! the sixty a call waits, unless `--timeout` says otherwise.

mod common;

use common::*;
use serde_json::Value;
use std::time::{Duration, Instant};

/// The default probe timeout, and the call timeout it stands in for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Timed, and with the one row's status read back out of `--json`.
fn timed(cmd: &mut std::process::Command) -> (Duration, Out) {
    let started = Instant::now();
    let o = run(cmd);
    (started.elapsed(), o)
}

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

#[test]
fn ls_gives_up_on_a_silent_server_within_the_probe_timeout() {
    let s = start(Mode::BlackHole);
    let home = temp_home("probe-timeout-ls");
    assert_eq!(
        run(mcpdial(&home).args(["add", "hole", "--http", &s.url, "--no-probe"])).code,
        0
    );

    // No --timeout: this is the wait the issue is about.
    let (took, o) = timed(mcpdial(&home).args(["--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
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

    let (took, o) = timed(mcpdial(&home).args(["--json", "add", "hole", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
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

    let (took, o) =
        timed(mcpdial(&home).args(["--timeout", "1", "--json", "add", "hole", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        took < PROBE_TIMEOUT / 2,
        "add waited {took:?} with --timeout 1"
    );
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    unreachable_in_time(&receipt["saved"], &o.stdout);

    let (took, o) = timed(mcpdial(&home).args(["--timeout", "1", "--json", "ls", "--refresh"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        took < PROBE_TIMEOUT / 2,
        "ls waited {took:?} with --timeout 1"
    );
    let rows: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows.len(), 1, "{}", o.stdout);
    unreachable_in_time(&rows[0], &o.stdout);
}
