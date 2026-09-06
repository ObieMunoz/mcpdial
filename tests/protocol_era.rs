//! Which era of the protocol a saved server speaks is remembered beside its
//! status, so a server from before 2026-07-28 is not asked for the
//! `server/discover` it has never heard of on every connection.
//!
//! The note can go wrong - a server is upgraded under it - so what these check
//! is mostly what happens when it has: the round trip it was saving is paid
//! back, the command still runs, and the note is corrected on the way.

mod common;

use common::{mcpdial, run, start, temp_home, FakeServer, Mode};
use serde_json::Value;
use std::path::Path;

fn methods(s: &FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| Some(r.json()["method"].as_str()?.to_string()))
        .collect()
}

fn probes(home: &Path) -> Value {
    let written = std::fs::read_to_string(home.join("probes.json")).expect("probes.json");
    serde_json::from_str(&written).expect("probes.json holds JSON")
}

fn rewrite_probes(home: &Path, file: &Value) {
    std::fs::write(home.join("probes.json"), file.to_string()).unwrap();
}

fn noted_era(home: &Path, name: &str) -> Value {
    probes(home)["probes"][name]["era"]["era"].clone()
}

fn saved(home: &Path, name: &str, url: &str) {
    let o = run(mcpdial(home).args(["add", name, "--http", url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
}

#[test]
fn an_older_server_is_asked_to_discover_once_and_never_again() {
    let s = start(Mode::Stateless);
    let home = temp_home("era-remembered");
    saved(&home, "old", &s.url);

    let o = run(mcpdial(&home).args(["tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let first = methods(&s);
    assert_eq!(first[0], "server/discover", "nothing known yet: {first:?}");
    assert_eq!(first[1], "initialize", "{first:?}");
    assert_eq!(noted_era(&home, "old"), "handshake");

    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let again = methods(&s);
    assert_eq!(again[0], "initialize", "straight to it: {again:?}");
    assert!(
        !again.iter().any(|m| m == "server/discover"),
        "the round trip #155 is about: {again:?}"
    );

    // The record so far holds an era and no status: `ls` has never run here.
    let o = run(mcpdial(&home).args(["--json", "ls"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let rows: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(rows[0]["status"]["state"], "connected", "{}", o.stdout);
}

/// Every `probes.json` in the world was written without an era. Reading one has
/// to behave exactly as this did before there was anything to read.
#[test]
fn a_record_with_no_era_is_read_as_find_out_and_gains_one() {
    let s = start(Mode::Stateless);
    let home = temp_home("era-absent");
    saved(&home, "old", &s.url);
    assert_eq!(run(mcpdial(&home).args(["tools", "old"])).code, 0);

    let mut file = probes(&home);
    assert!(file["probes"]["old"]["era"].is_object(), "{file}");
    file["probes"]["old"].as_object_mut().unwrap().remove("era");
    let status_before = file["probes"]["old"]["status"].clone();
    let checked_before = file["probes"]["old"]["checked_at"].clone();
    rewrite_probes(&home, &file);

    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = methods(&s);
    assert_eq!(asked[0], "server/discover", "detect, as before: {asked:?}");
    assert_eq!(noted_era(&home, "old"), "handshake", "then remember");
    let after = probes(&home);
    assert_eq!(after["probes"]["old"]["status"], status_before);
    assert_eq!(after["probes"]["old"]["checked_at"], checked_before);
}

/// A note naming an era no build has ever heard of is a cache miss and one extra
/// round trip, not a file mcpdial refuses to read.
#[test]
fn a_note_this_build_cannot_read_is_a_cache_miss() {
    let s = start(Mode::Stateless);
    let home = temp_home("era-unreadable");
    saved(&home, "old", &s.url);
    assert_eq!(run(mcpdial(&home).args(["tools", "old"])).code, 0);

    let mut file = probes(&home);
    file["probes"]["old"]["era"]["era"] = Value::from("telepathy");
    rewrite_probes(&home, &file);

    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(methods(&s)[0], "server/discover");
    assert_eq!(noted_era(&home, "old"), "handshake", "written over");
}

/// A server with no note at all behaves as it always has, and a listing that
/// remembers a status must not forget what the connection behind it worked out.
#[test]
fn deleting_the_file_costs_the_round_trip_back_and_nothing_else() {
    let s = start(Mode::Stateless);
    let home = temp_home("era-deleted");
    saved(&home, "old", &s.url);
    assert_eq!(run(mcpdial(&home).args(["tools", "old"])).code, 0);
    std::fs::remove_file(home.join("probes.json")).unwrap();

    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "old"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(methods(&s)[0], "server/discover", "detect, then remember");
    assert_eq!(noted_era(&home, "old"), "handshake");

    // `ls` rewrites the record around the note; the note has to survive it.
    let o = run(mcpdial(&home).args(["ls", "--refresh"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(noted_era(&home, "old"), "handshake", "kept by the listing");
}

/// The one that decides whether remembering an era is safe at all.
#[test]
fn a_server_upgraded_under_its_note_corrects_it_instead_of_failing() {
    let s = start(Mode::Upgradable);
    let home = temp_home("era-upgraded");
    saved(&home, "moving", &s.url);
    assert_eq!(run(mcpdial(&home).args(["tools", "moving"])).code, 0);
    assert_eq!(noted_era(&home, "moving"), "handshake");

    s.upgrade();
    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "moving"]));
    assert_eq!(
        o.code, 0,
        "a stale note must not fail a command: {}",
        o.stderr
    );
    assert!(o.stdout.contains("echo"), "{}", o.stdout);
    let asked = methods(&s);
    assert_eq!(asked[0], "initialize", "what the note asked for: {asked:?}");
    assert_eq!(asked[1], "server/discover", "what it really is: {asked:?}");
    assert_eq!(noted_era(&home, "moving"), "discovery", "corrected");

    s.forget_requests();
    let o = run(mcpdial(&home).args(["tools", "moving"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = methods(&s);
    assert_eq!(
        asked[0], "server/discover",
        "and stays corrected: {asked:?}"
    );
    assert!(!asked.iter().any(|m| m == "initialize"), "{asked:?}");
}

/// The note sits below `--protocol-version`, which still skips detection
/// entirely and still says nothing about what the server would have answered.
#[test]
fn a_named_revision_beats_the_note_and_leaves_it_alone() {
    let s = start(Mode::Modern);
    let home = temp_home("era-pinned");
    saved(&home, "new", &s.url);
    assert_eq!(run(mcpdial(&home).args(["tools", "new"])).code, 0);
    assert_eq!(noted_era(&home, "new"), "discovery");

    let mut file = probes(&home);
    file["probes"]["new"]["era"]["era"] = Value::from("handshake");
    rewrite_probes(&home, &file);

    s.forget_requests();
    let o = run(mcpdial(&home).args(["--protocol-version", "2026-07-28", "tools", "new"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = methods(&s);
    assert_eq!(
        asked[0], "server/discover",
        "the pin, not the note: {asked:?}"
    );
    assert_eq!(
        noted_era(&home, "new"),
        "handshake",
        "a pinned run has heard nothing worth noting"
    );
}

/// An ad-hoc URL is nobody's saved server, so nothing is remembered for it and
/// nothing is left behind either.
#[test]
fn an_ad_hoc_target_is_neither_read_nor_written() {
    let s = start(Mode::Stateless);
    let home = temp_home("era-ad-hoc");

    for _ in 0..2 {
        s.forget_requests();
        let o = run(mcpdial(&home).args(["tools", &s.url]));
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert_eq!(methods(&s)[0], "server/discover");
    }
    assert!(!home.join("probes.json").exists(), "nothing to remember");
}
