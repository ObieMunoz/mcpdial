//! The curated catalog end to end: read from a fixture file, from a URL with the
//! day-long cache, and from the embedded copy when a refresh fails.

mod common;

use common::{catalog_entries, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn embedded() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_catalog_comes_from_the_file_named_in_the_environment() {
    let s = start(Mode::Stateless);
    let home = temp_home("catalog-file");
    let fixture = home.join("fixture.json");
    std::fs::write(&fixture, catalog_entries(&s.base).to_string()).unwrap();
    let at_fixture = || {
        let mut c = mcpdial(&home);
        c.env("MCPDIAL_CATALOG", &fixture)
            .env("MCPDIAL_REGISTRY", &s.base);
        c
    };

    // --json prints the entries as they are, and nothing else anywhere.
    let o = run(at_fixture().args(["catalog", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let printed: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(printed, catalog_entries(&s.base));
    assert_eq!(o.stderr, "");

    // The readable form groups by category, in the fixed order, and skips the
    // categories the file has nothing under.
    let o = run(at_fixture().arg("catalog"));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let docs = o.stdout.find("Docs and search").expect("a Docs heading");
    let databases = o.stdout.find("Databases").expect("a Databases heading");
    let local = o.stdout.find("Local files").expect("a Local files heading");
    assert!(docs < databases && databases < local, "{}", o.stdout);
    assert!(!o.stdout.contains("Browsers"), "{}", o.stdout);
    let row = o
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with("box "))
        .unwrap_or_else(|| panic!("no row for box:\n{}", o.stdout));
    for piece in ["Acme Box", "stdio", "env", "A sandbox, as a Python package"] {
        assert!(row.contains(piece), "{row}");
    }
    assert!(o.stderr.contains("--catalog ID"), "{}", o.stderr);

    // A registry entry is saved as --registry would save it, with the transport
    // the catalog promised, and it dials.
    let o = run(at_fixture().args(["add", "web", "--catalog", "remote"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains(&format!("saved web (http {})", s.url)),
        "{}",
        o.stderr
    );
    let o = run(mcpdial(&home).args(["info", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("fake-mcp"), "{}", o.stdout);
    let o = run(at_fixture().args(["add", "sandbox", "--catalog", "box"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("saved sandbox (stdio uvx acme-box==1.0)"),
        "{}",
        o.stderr
    );

    // A config entry is saved as it is.
    let o = run(at_fixture().args(["add", "direct", "--catalog", "fake"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap();
    assert_eq!(saved["servers"]["direct"]["http"], s.url);

    // An entry whose registry server leaves a value to the user cannot be added
    // from the catalog: exit 2, nothing saved, and the hint says how instead.
    let o = run(at_fixture().args(["add", "fs", "--catalog", "files"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("files needs 1 value(s)")
            && o.stderr.contains("--registry io.github.acme/files --arg")
            && o.stderr.contains("directory (required)"),
        "{}",
        o.stderr
    );
    assert!(!saved.to_string().contains("\"fs\""));

    // An id the catalog does not have, with the nearest one named.
    let o = run(at_fixture().args(["--json", "add", "x", "--catalog", "remot"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no catalog entry named \"remot\""),
        "{e}"
    );
    assert!(
        e["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("did you mean remote?"),
        "{e}"
    );

    // --catalog is one way of three to say where a server is.
    let o = run(at_fixture().args(["add", "x", "--catalog", "fake", "--http", "http://x/mcp"]));
    assert_eq!(o.code, 2);
    let o = run(at_fixture().args(["add", "x", "--catalog", "fake", "--registry", "a/b"]));
    assert_eq!(o.code, 2);

    // A file that was asked for by name and does not validate is an error.
    let mut broken = catalog_entries(&s.base);
    broken[0]["category"] = Value::String("Games".into());
    std::fs::write(&fixture, broken.to_string()).unwrap();
    let o = run(at_fixture().arg("catalog"));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("fixture.json")
            && o.stderr.contains("entry \"remote\": category \"Games\""),
        "{}",
        o.stderr
    );
    let o = run(at_fixture().args(["--json", "add", "x", "--catalog", "remote"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "config");
    let o = run(at_fixture()
        .args(["catalog", "--json"])
        .env("MCPDIAL_CATALOG", home.join("missing.json")));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("missing.json"), "{}", o.stderr);
}

#[test]
fn a_failed_refresh_falls_back_to_the_embedded_copy_silently() {
    let home = temp_home("catalog-embedded");
    let unreachable = "http://127.0.0.1:1/catalog.json";

    let o = run(mcpdial(&home)
        .args(["catalog", "--json"])
        .env("MCPDIAL_CATALOG", unreachable));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "", "a failed refresh is not reported");
    let printed: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(printed, embedded());
    assert!(
        !home.join("catalog.json").exists(),
        "nothing was fetched, so nothing is cached"
    );

    // --offline never tries, and says so only under -v.
    let o = run(mcpdial(&home)
        .args(["catalog", "--json", "--offline"])
        .env("MCPDIAL_CATALOG", "http://127.0.0.1:1/catalog.json"));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "");
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap(),
        embedded()
    );
    let o = run(mcpdial(&home)
        .args(["-v", "catalog", "--offline"])
        .env("MCPDIAL_CATALOG", unreachable));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr.contains("catalog: embedded copy (--offline)"),
        "{}",
        o.stderr
    );

    // The readable form of the shipped list has every category, and the ids
    // the README names.
    let o = run(mcpdial(&home)
        .arg("catalog")
        .env("MCPDIAL_CATALOG", unreachable));
    assert_eq!(o.code, 0, "{}", o.stderr);
    for heading in [
        "Source control",
        "Browsers",
        "Docs and search",
        "Databases",
        "Productivity",
        "Cloud and infra",
        "AI and data",
        "Local files",
    ] {
        assert!(
            o.stdout.lines().any(|l| l == heading),
            "{heading} missing:\n{}",
            o.stdout
        );
    }
    assert!(
        o.stdout.contains("  context7 ") && o.stdout.contains("  chrome-devtools "),
        "{}",
        o.stdout
    );
}

#[test]
fn a_fetched_catalog_is_cached_for_a_day_under_its_source() {
    let s = start(Mode::Stateless);
    let home = temp_home("catalog-cache");
    let url = format!("{}/catalog.json", s.base);
    let fetches = || {
        s.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.starts_with("/catalog.json"))
            .count()
    };
    let listing = |source: &str| {
        let o = run(mcpdial(&home)
            .args(["catalog", "--json"])
            .env("MCPDIAL_CATALOG", source));
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert_eq!(o.stderr, "");
        serde_json::from_str::<Value>(&o.stdout).unwrap()
    };

    assert_eq!(listing(&url), catalog_entries(&s.base));
    assert_eq!(fetches(), 1);
    let cache = home.join("catalog.json");
    assert!(cache.exists());

    // Within the day the cache answers and the network is not asked.
    assert_eq!(listing(&url), catalog_entries(&s.base));
    assert_eq!(fetches(), 1, "served from the cache");

    // Another source is fetched even while the cache is fresh.
    let other = format!("{url}?other");
    assert_eq!(listing(&other), catalog_entries(&s.base));
    assert_eq!(fetches(), 2);

    // A day later the cache is stale and the fetch happens again.
    let yesterday = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(yesterday)
        .unwrap();
    assert_eq!(listing(&other), catalog_entries(&s.base));
    assert_eq!(fetches(), 3);

    // Under -v the origin is named.
    let o = run(mcpdial(&home)
        .args(["-v", "catalog"])
        .env("MCPDIAL_CATALOG", &other));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stderr
            .contains(&format!("catalog: cached copy of {other}")),
        "{}",
        o.stderr
    );
    assert_eq!(fetches(), 3);
}

/// Every registry entry in the shipped catalog must still resolve against the
/// live registry and convert to the transport it promises, with nothing left to
/// the user. CI runs this on its own; it needs the network.
#[test]
#[ignore = "talks to the live registry; run with --ignored"]
fn live_registry_resolves_every_shipped_entry() {
    let registry = mcpdial::registry::Registry::from_env(Duration::from_secs(30), "mcpdial-ci");
    let mut problems = Vec::new();
    for entry in mcpdial::catalog::embedded() {
        if entry.registry.is_none() {
            continue;
        }
        match mcpdial::catalog::resolve(&entry, &registry) {
            Ok(r) if !r.missing.is_empty() => problems.push(format!(
                "{}: leaves values to the user: {:?}",
                entry.id, r.missing
            )),
            Ok(r) if r.config.kind() != entry.transport.as_str() => problems.push(format!(
                "{}: converts to {} but the catalog says {}",
                entry.id,
                r.config.kind(),
                entry.transport.as_str()
            )),
            Ok(_) => {}
            Err(e) => problems.push(format!("{}: {e}", entry.id)),
        }
    }
    assert!(problems.is_empty(), "\n  {}", problems.join("\n  "));
}
