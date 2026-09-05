//! Ranked search over the local copy of the registry.
//!
//! The registry's own search matches names alone, alphabetically, so `github`
//! lists an Obsidian vault and three mirrors before GitHub's own server and
//! `browser automation` finds nothing. [`rank`] runs the query over name, title
//! and description instead and scores each match: one signal is one line, so
//! the next one costs no more than that.

use serde_json::Value;
use std::collections::HashMap;

/// Namespaces this big are mirrors and generated stacks, not a vendor's servers.
const CROWDED: usize = 100;

/// The entries matching every word of `query`, best first. `entries` are the
/// registry's list objects (`server` and `_meta`); `catalog` names the servers
/// the curated catalog lists, which go first of all.
pub fn rank<'a>(query: &str, entries: &'a [Value], catalog: &[String]) -> Vec<&'a Value> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut per_namespace: HashMap<&str, usize> = HashMap::new();
    for e in entries {
        *per_namespace.entry(namespace(&e["server"])).or_default() += 1;
    }

    let mut hits: Vec<(i64, String, &Value)> = entries
        .iter()
        .filter_map(|e| {
            let server = &e["server"];
            let text = Text::of(server);
            text.matches_all(&words).then(|| {
                let name = server["name"].as_str().unwrap_or("");
                let crowded = per_namespace.get(namespace(server)).copied().unwrap_or(0) > CROWDED;
                let listed = catalog.iter().any(|c| c == name);
                (
                    score(&text, &words, server, listed, crowded),
                    name.to_lowercase(),
                    e,
                )
            })
        })
        .collect();
    hits = newest_per_repository(hits);
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, _, e)| e).collect()
}

/// Higher is better. The tiers are far enough apart that the signals within
/// one never reorder the tiers.
fn score(text: &Text, words: &[String], server: &Value, listed: bool, crowded: bool) -> i64 {
    let whole = words.join(" ");
    let n = words.len() as i64;
    let hits = |field: &str| words.iter().filter(|w| field.contains(w.as_str())).count() as i64;
    let short_name = text.name.rsplit('/').next().unwrap_or("");
    let mut s = 0;
    s += 10_000_000 * i64::from(listed);
    s += 1_000_000 * i64::from(text.name == whole || text.title == whole);
    s += 100_000
        * i64::from(
            words
                .iter()
                .any(|w| official_vendor(&text.name) == Some(w.as_str())),
        );
    s += 10_000 * hits(&text.title) / n;
    s += 1_000 * hits(short_name) / n;
    s += match dialable(server) {
        Dialable::Easily => 100,
        Dialable::WithSetup => 50,
        Dialable::No => 0,
    };
    s -= 10 * i64::from(crowded);
    s
}

/// What the ranking reads, lowercased once. The `io.github.` prefix names the
/// host, not the server, so it is left out: otherwise `github` matches every
/// server hosted there.
struct Text {
    name: String,
    title: String,
    description: String,
}

impl Text {
    fn of(server: &Value) -> Self {
        let field = |k: &str| server[k].as_str().unwrap_or("").to_lowercase();
        let name = field("name");
        Self {
            name: match name.strip_prefix("io.github.") {
                Some(rest) => rest.to_string(),
                None => name,
            },
            title: field("title"),
            description: field("description"),
        }
    }

    fn matches_all(&self, words: &[String]) -> bool {
        words.iter().all(|w| {
            self.name.contains(w.as_str())
                || self.title.contains(w.as_str())
                || self.description.contains(w.as_str())
        })
    }
}

/// The vendor in `io.github.<vendor>/...`, the one namespace the registry
/// verifies against an account.
fn official_vendor(lowercase_name: &str) -> Option<&str> {
    // `Text::name` has the prefix stripped already; the vendor is what precedes the slash.
    lowercase_name
        .split_once('/')
        .filter(|(vendor, _)| !vendor.contains('.'))
        .map(|(vendor, _)| vendor)
}

fn namespace(server: &Value) -> &str {
    let name = server["name"].as_str().unwrap_or("");
    name.split_once('/').map_or(name, |(ns, _)| ns)
}

enum Dialable {
    /// A remote to POST to, or an npm package `npx` fetches on first use.
    Easily,
    /// A PyPI package or a container image: `uvx` or `docker` first.
    WithSetup,
    No,
}

fn dialable(server: &Value) -> Dialable {
    let list = |k: &str| server[k].as_array().map_or(&[][..], Vec::as_slice);
    let remote = list("remotes")
        .iter()
        .any(|r| matches!(r["type"].as_str(), Some("streamable-http") | Some("sse")));
    let package = |kind: &str| list("packages").iter().any(|p| p["registryType"] == kind);
    if remote || package("npm") {
        Dialable::Easily
    } else if package("pypi") || package("oci") {
        Dialable::WithSetup
    } else {
        Dialable::No
    }
}

/// One entry per repository: a server republished under a second name, or
/// mirrored by another namespace, shows once, as its newest listing. A
/// monorepo's servers name their `subfolder`, which keeps them apart.
fn newest_per_repository(hits: Vec<(i64, String, &Value)>) -> Vec<(i64, String, &Value)> {
    let mut newest: HashMap<String, usize> = HashMap::new();
    let mut keep = vec![true; hits.len()];
    for (i, (_, _, e)) in hits.iter().enumerate() {
        let Some(key) = repository_key(&e["server"]) else {
            continue;
        };
        match newest.get(&key) {
            Some(&j) if published(hits[j].2) >= published(e) => keep[i] = false,
            Some(&j) => {
                keep[j] = false;
                newest.insert(key, i);
            }
            None => {
                newest.insert(key, i);
            }
        }
    }
    hits.into_iter()
        .zip(keep)
        .filter_map(|(hit, kept)| kept.then_some(hit))
        .collect()
}

fn repository_key(server: &Value) -> Option<String> {
    let url = server["repository"]["url"].as_str()?.trim();
    if url.is_empty() {
        return None;
    }
    let url = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    let subfolder = server["repository"]["subfolder"]
        .as_str()
        .unwrap_or("")
        .trim_matches('/');
    Some(format!("{url}#{subfolder}"))
}

/// When the listing was published, as the registry writes it: RFC 3339 in UTC,
/// so the strings order as the instants do.
fn published(entry: &Value) -> &str {
    let official = &entry["_meta"]["io.modelcontextprotocol.registry/official"];
    official["publishedAt"]
        .as_str()
        .or(official["updatedAt"].as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A few hundred entries as the registry served them in September 2026: the
    /// pages for `github`, `postgres`, `browser` and a few other queries, the
    /// official GitHub server, and enough of `ai.smithery` to make it crowded.
    fn fixture() -> Vec<Value> {
        serde_json::from_str(include_str!("../tests/fixtures/registry-index.json")).unwrap()
    }

    fn names<'a>(hits: &[&'a Value]) -> Vec<&'a str> {
        hits.iter()
            .map(|e| e["server"]["name"].as_str().unwrap())
            .collect()
    }

    fn entry(name: &str, title: Option<&str>, description: &str, extra: Value) -> Value {
        let mut server = json!({"name": name, "description": description});
        if let Some(t) = title {
            server["title"] = json!(t);
        }
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                server[k] = v.clone();
            }
        }
        json!({"server": server, "_meta": {"io.modelcontextprotocol.registry/official":
            {"status": "active", "publishedAt": "2026-01-01T00:00:00Z"}}})
    }

    #[test]
    fn github_ranks_the_official_server_above_the_mirrors() {
        let index = fixture();
        let hits = rank("github", &index, &[]);
        let top = names(&hits);
        assert_eq!(top[0], "io.github.github/github-mcp-server", "{top:?}");
        // The mirrors sit in a namespace of two hundred, so they follow every
        // other server that dials as easily.
        let at = |name: &str| top.iter().position(|n| *n == name).unwrap_or(usize::MAX);
        let first_mirror = top
            .iter()
            .position(|n| n.starts_with("ai.smithery/"))
            .unwrap();
        assert!(at("dev.reapx/github-repos") < first_mirror, "{top:?}");
        assert!(
            at("ai.smithery/Hint-Services-obsidian-github-mcp") > 5,
            "the Obsidian vault led the registry's own list: {top:?}"
        );
        // The registry's own search matched `github` in every `io.github.*` name.
        assert!(
            !top.contains(&"io.github.0xChron/polymarket-mcp"),
            "hosted on GitHub is not about GitHub: {top:?}"
        );
    }

    #[test]
    fn postgres_ranks_a_database_server_above_the_stack_generators() {
        let index = fixture();
        let hits = rank("postgres", &index, &[]);
        let top = names(&hits);
        assert!(top[0].to_lowercase().contains("postgres"), "{top:?}");
        let generators = top
            .iter()
            .take(8)
            .filter(|n| n.starts_with("ai.getvda/"))
            .count();
        assert_eq!(
            generators, 0,
            "stack generators in the first eight: {top:?}"
        );
        let first = hits[0]["server"]["title"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        assert!(
            first.contains("postgres"),
            "a title match comes first: {top:?}"
        );
    }

    #[test]
    fn browser_automation_finds_servers_across_title_and_description() {
        let index = fixture();
        let hits = rank("browser automation", &index, &[]);
        let top = names(&hits);
        assert!(!top.is_empty(), "the registry's own search found nothing");
        assert_eq!(
            top[0], "io.github.mindstone/mcp-server-browser-automation",
            "an exact title match: {top:?}"
        );
        assert!(
            top.contains(&"io.github.runbook-ai/browser-agent"),
            "matched in the description: {top:?}"
        );

        // One word in the title, the other in the description.
        let hits = rank("browserbase cloud", &index, &[]);
        assert!(
            names(&hits).contains(&"io.github.mindstone/mcp-server-browserbase"),
            "{:?}",
            names(&hits)
        );
        assert!(
            rank("browser zzzzzz", &index, &[]).is_empty(),
            "every word is required"
        );
        assert!(rank("   ", &index, &[]).is_empty());
    }

    #[test]
    fn entries_sharing_a_repository_collapse_to_the_newest() {
        let index = fixture();
        let hits = rank("browserbase", &index, &[]);
        let top = names(&hits);
        let browserbase: Vec<&&str> = top
            .iter()
            .filter(|n| {
                **n == "ai.smithery/browserbasehq-mcp-browserbase"
                    || **n == "io.github.browserbase/mcp-server-browserbase"
            })
            .collect();
        assert_eq!(
            browserbase,
            [&"ai.smithery/browserbasehq-mcp-browserbase"],
            "one listing per repository, the one published last: {top:?}"
        );

        let older = entry(
            "io.github.a/old",
            None,
            "twin",
            json!({"repository": {"url": "https://github.com/a/twin.git"}}),
        );
        let mut newer = entry(
            "io.github.a/new",
            None,
            "twin",
            json!({"repository": {"url": "https://github.com/a/twin/"}}),
        );
        newer["_meta"]["io.modelcontextprotocol.registry/official"]["publishedAt"] =
            json!("2026-06-01T00:00:00Z");
        let mut apart = entry(
            "io.github.a/part",
            None,
            "twin",
            json!({"repository": {"url": "https://github.com/a/twin", "subfolder": "part"}}),
        );
        apart["_meta"]["io.modelcontextprotocol.registry/official"]["publishedAt"] =
            json!("2025-01-01T00:00:00Z");
        let no_repo = entry("io.github.b/twin", None, "twin", json!({}));
        let index = vec![older, newer.clone(), apart, no_repo];
        assert_eq!(
            names(&rank("twin", &index, &[])),
            ["io.github.b/twin", "io.github.a/new", "io.github.a/part"],
            "a .git suffix and a trailing slash name the same repository; a subfolder does not"
        );
    }

    #[test]
    fn the_tiers_hold_in_order() {
        let exact = entry(
            "com.x/anything",
            Some("Acme"),
            "a thing",
            json!({"packages": [{"registryType": "pypi"}]}),
        );
        let vendor = entry("io.github.acme/other", None, "acme by acme", json!({}));
        let titled = entry(
            "com.y/thing",
            Some("Acme tools"),
            "tools",
            json!({"packages": [{"registryType": "oci"}]}),
        );
        let described = entry(
            "com.z/thing",
            Some("Thing"),
            "an acme client",
            json!({"remotes": [{"type": "streamable-http", "url": "https://z/mcp"}]}),
        );
        let named = entry("com.v/acme-kit", None, "a kit", json!({}));
        let unlisted = entry("com.w/thing", None, "acme, nothing to run", json!({}));
        let index = vec![
            unlisted.clone(),
            described.clone(),
            named,
            titled.clone(),
            vendor.clone(),
            exact.clone(),
        ];
        assert_eq!(
            names(&rank("acme", &index, &[])),
            [
                "com.x/anything",
                "io.github.acme/other",
                "com.y/thing",
                "com.v/acme-kit",
                "com.z/thing",
                "com.w/thing"
            ]
        );
        assert_eq!(
            names(&rank("ACME", &index, &[]))[0],
            "com.x/anything",
            "case does not matter"
        );
        assert_eq!(
            names(&rank("acme", &index, &["com.w/thing".to_string()]))[0],
            "com.w/thing",
            "a catalog entry goes first of all"
        );

        // Within a tier: a remote or npm package first, then pypi and oci, then
        // nothing to dial; a crowded namespace after the rest.
        let mut index: Vec<Value> = (0..CROWDED + 1)
            .map(|i| {
                entry(
                    &format!("ai.mirror/x{i}"),
                    None,
                    "widget",
                    json!({"remotes": [{"type": "sse", "url": "https://m/x"}]}),
                )
            })
            .collect();
        index.push(entry(
            "com.a/npm",
            None,
            "widget",
            json!({"packages": [{"registryType": "npm"}]}),
        ));
        index.push(entry(
            "com.b/pypi",
            None,
            "widget",
            json!({"packages": [{"registryType": "pypi"}]}),
        ));
        index.push(entry("com.c/none", None, "widget", json!({})));
        index.push(entry(
            "com.d/remote",
            None,
            "widget",
            json!({"remotes": [{"type": "streamable-http", "url": "https://d/mcp"}]}),
        ));
        let top = names(&rank("widget", &index, &[]));
        assert_eq!(
            &top[..3],
            ["com.a/npm", "com.d/remote", "ai.mirror/x0"],
            "{top:?}"
        );
        assert_eq!(top[CROWDED + 3], "com.b/pypi");
        assert_eq!(top[CROWDED + 4], "com.c/none");
    }
}
