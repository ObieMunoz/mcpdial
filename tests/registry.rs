//! The registry client against the fake registry: the list endpoint and the
//! by-name lookup, as `add --registry` and the browsing that follows it use them.

mod common;

use common::{start, Mode};
use mcpdial::registry::Registry;
use std::time::Duration;

#[test]
fn the_registry_client_searches_and_looks_up_by_name() {
    let s = start(Mode::Stateless);
    let registry = Registry::new(&s.base, Duration::from_secs(5), "mcpdial-test");

    // The list endpoint, asked for the latest version of each match, and the
    // registry's own objects handed back untouched.
    let found = registry.search("acme", 1).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["server"]["name"], "io.github.acme/files");
    assert!(found[0]["_meta"].is_object());
    let asked = s.requests.lock().unwrap().last().unwrap().path.clone();
    assert!(
        asked.starts_with("/v0.1/servers?")
            && asked.contains("search=acme")
            && asked.contains("limit=1")
            && asked.contains("version=latest"),
        "{asked}"
    );
    assert_eq!(registry.search("acme", 100).unwrap().len(), 4);
    assert!(registry.search("zzz", 30).unwrap().is_empty());

    // One server by name, the slash in the name percent-encoded on the way.
    let server = registry.latest("io.github.acme/box").unwrap().unwrap();
    assert_eq!(server["name"], "io.github.acme/box");
    assert_eq!(server["packages"][0]["registryType"], "pypi");
    let asked = s.requests.lock().unwrap().last().unwrap().path.clone();
    assert_eq!(asked, "/v0.1/servers/io.github.acme%2Fbox/versions/latest");
    assert!(registry.latest("io.github.nope/nope").unwrap().is_none());

    // A registry that cannot be reached is a transport error, not a panic.
    let down = Registry::new("http://127.0.0.1:1", Duration::from_secs(2), "mcpdial-test");
    let e = down.latest("io.github.acme/box").unwrap_err();
    let unreachable = e.to_string();
    assert!(
        unreachable.contains("registry at http://127.0.0.1:1"),
        "{unreachable}"
    );
}
