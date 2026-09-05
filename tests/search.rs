//! `search` over a local copy of the registry: fetched page by page the first
//! time, refreshed from the watermark once a day old, ranked, and off the
//! network when told to be.

mod common;

use common::{
    mcpdial, run, start, temp_home, FakeServer, Mode, REGISTRY_UPDATED, REGISTRY_UPDATED_LATER,
};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn copy_on_disk(home: &Path) -> Value {
    let text = std::fs::read_to_string(home.join("registry").join("servers.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Make the copy a day old, as the clock would.
fn age_copy(home: &Path) {
    let mut copy = copy_on_disk(home);
    copy["synced_at"] = Value::from(0);
    std::fs::write(home.join("registry").join("servers.json"), copy.to_string()).unwrap();
}

/// The list requests the fake registry saw, newest last.
fn listings(s: &FakeServer) -> Vec<String> {
    s.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.starts_with("/v0.1/servers?"))
        .map(|r| r.path.clone())
        .collect()
}

/// Against the fake registry, and the fake catalog it also serves.
fn at_registry(home: &Path, base: &str) -> Command {
    let mut c = mcpdial(home);
    c.env("MCPDIAL_REGISTRY", base)
        .env("MCPDIAL_CATALOG", format!("{base}/catalog.json"));
    c
}

#[test]
fn the_first_search_fetches_every_page_and_a_day_later_only_what_changed() {
    let s = start(Mode::Stateless);
    let home = temp_home("search");

    let o = run(at_registry(&home, &s.base).args(["search", "acme"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = listings(&s);
    assert_eq!(asked.len(), 2, "two pages of three: {asked:?}");
    assert!(
        asked[0].contains("limit=100")
            && asked[0].contains("version=latest")
            && !asked[0].contains("cursor=")
            && !asked[0].contains("search="),
        "{asked:?}"
    );
    assert!(
        asked[1].contains("cursor=io.github.acme%2Fbox"),
        "the second page starts where the first said: {asked:?}"
    );
    let copy = copy_on_disk(&home);
    assert_eq!(copy["registry"], s.base);
    assert_eq!(copy["servers"].as_array().unwrap().len(), 4);
    assert_eq!(copy["updated_through"], REGISTRY_UPDATED);
    assert!(copy["synced_at"].as_u64().unwrap() > 0);

    // The table, ranked: the three the catalog lists first, tagged, and among
    // them the ones that dial without an install ahead of the pypi/oci one.
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["NAME", "TRANSPORTS", "SOURCE", "DESCRIPTION"],
        "{}",
        o.stdout
    );
    let names: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "io.github.acme/files",
            "io.github.acme/remote",
            "io.github.acme/box",
            "io.github.acme/legacy"
        ],
        "{}",
        o.stdout
    );
    assert!(
        lines[1..4].iter().all(|l| l.contains("  catalog  ")) && lines[4].contains("  registry  "),
        "{}",
        o.stdout
    );
    assert!(
        lines[2].contains("http, sse") && lines[3].contains("stdio (pypi), stdio (oci)"),
        "{}",
        o.stdout
    );
    assert!(
        lines[1].ends_with("...") && !lines[1].contains("somewhere"),
        "the description is trimmed as `ls` trims: {}",
        o.stdout
    );

    // A fresh copy is searched as it is, and `--json` hands the registry's
    // objects back untouched.
    let before = s.requests.lock().unwrap().len();
    let o = run(at_registry(&home, &s.base).args(["search", "sandbox", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let hits: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(hits.len(), 1, "{}", o.stdout);
    assert_eq!(hits[0]["server"]["name"], "io.github.acme/box");
    assert_eq!(hits[0]["server"]["packages"][1]["registryType"], "oci");
    assert!(hits[0]["_meta"].is_object(), "{}", o.stdout);
    assert_eq!(s.requests.lock().unwrap().len(), before);

    // A day later the registry is asked only for what changed since the
    // watermark, and the copy on disk reflects the answer.
    age_copy(&home);
    let o = run(at_registry(&home, &s.base).args(["search", "directory"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = listings(&s);
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert!(
        asked[2].contains("updated_since=2026-09-01T00%3A00%3A00Z")
            && !asked[2].contains("cursor="),
        "{asked:?}"
    );
    let copy = copy_on_disk(&home);
    assert_eq!(copy["servers"].as_array().unwrap().len(), 4);
    let files = copy["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["server"]["name"] == "io.github.acme/files")
        .unwrap();
    assert_eq!(files["server"]["version"], "1.3.0");
    assert_eq!(copy["updated_through"], REGISTRY_UPDATED_LATER);
    assert!(o.stdout.contains("now with search"), "{}", o.stdout);

    // `--refresh` fetches everything again; `--offline` fetches nothing.
    let o = run(at_registry(&home, &s.base).args(["search", "acme", "--refresh"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let asked = listings(&s);
    assert_eq!(asked.len(), 5, "{asked:?}");
    assert!(!asked[3].contains("updated_since"), "{asked:?}");
    assert_eq!(
        copy_on_disk(&home)["updated_through"],
        REGISTRY_UPDATED,
        "the watermark is the newest the registry reported this time"
    );
    age_copy(&home);
    let o = run(at_registry(&home, &s.base).args(["search", "acme", "--offline"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(listings(&s).len(), 5);

    // With the registry unreachable, a stale copy is searched anyway, and the
    // note says so.
    let base = s.base.clone();
    drop(s);
    let o = run(at_registry(&home, &base).args(["search", "acme"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("note: could not reach the registry")
            && o.stderr.contains("searching the copy from"),
        "{}",
        o.stderr
    );
    assert!(o.stdout.contains("io.github.acme/files"), "{}", o.stdout);
}

#[test]
fn search_needs_a_query_and_a_copy_and_says_when_nothing_matches() {
    let s = start(Mode::Stateless);
    let home = temp_home("search-errors");

    // `--offline` before any copy exists is a config error, and nothing is fetched.
    let o = run(at_registry(&home, &s.base).args(["search", "acme", "--offline", "--json"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(e["error"]["kind"], "config");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no local copy of the registry yet"),
        "{}",
        o.stderr
    );
    assert!(listings(&s).is_empty());

    // No query under a pipe is a usage error.
    let o = run(at_registry(&home, &s.base).args(["search"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("search needs a query"), "{}", o.stderr);
    assert!(listings(&s).is_empty(), "nothing is fetched for no query");

    // Nothing matched is exit 1: a line on stderr, or an empty list under `--json`.
    let o = run(at_registry(&home, &s.base).args(["search", "zzz"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(
        o.stderr.contains("no registry entry matches \"zzz\""),
        "{}",
        o.stderr
    );
    assert_eq!(o.stdout, "");
    let o = run(at_registry(&home, &s.base).args(["search", "zzz", "--json"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "[]");

    // Every word is required, and `--limit` trims the ranked list and says so.
    let o = run(at_registry(&home, &s.base).args(["search", "fake", "server", "sse"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let names: Vec<&str> = o
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(names, ["io.github.acme/legacy"], "{}", o.stdout);
    let o = run(at_registry(&home, &s.base).args(["search", "acme", "--limit", "1"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.lines().count(), 2, "{}", o.stdout);
    assert!(
        o.stderr.contains("1 of 4 matches; --limit N shows more"),
        "{}",
        o.stderr
    );
    let o = run(at_registry(&home, &s.base).args(["search", "acme", "--limit", "1", "--json"]));
    let hits: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["server"]["name"], "io.github.acme/files");
}
