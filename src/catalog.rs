//! The curated catalog: a reviewed, categorised list of servers a person can read
//! top to bottom, unlike the registry's twenty thousand unranked names.
//!
//! `catalog.json` at the repository root ships inside the binary and is refreshed
//! from the raw file on `main`, cached for a day under the config directory. A
//! refresh that fails, or `--offline`, falls back to the embedded copy without
//! comment: the list is a convenience, never a reason for a command to fail.
//! [`MCPDIAL_CATALOG`](ENV_SOURCE) points at another URL or a local file instead.
//!
//! An entry either names a registry server, converted through
//! [`registry::convert`] exactly as `add --registry` would, or carries a
//! [`ServerConfig`] of its own for a server the registry lacks. Adding an entry is
//! a pull request; [`validate`] is the schema CI holds the file to.

use crate::config::{ServerConfig, Store};
use crate::protocol::{Error, Result};
use crate::registry::{self, Pick, Registry, Resolved};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const EMBEDDED: &str = include_str!("../catalog.json");
pub const DEFAULT_URL: &str =
    "https://raw.githubusercontent.com/ObieMunoz/mcpdial/main/catalog.json";
/// A URL or a path to read the catalog from instead of the default.
pub const ENV_SOURCE: &str = "MCPDIAL_CATALOG";
/// The refreshed copy, under the config directory.
pub const CACHE_FILE: &str = "catalog.json";
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// A refresh is best effort and must never stall a listing for as long as a
/// tool call is allowed to take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

/// The categories, in the order the listing prints them.
pub const CATEGORIES: [&str; 8] = [
    "Source control",
    "Browsers",
    "Docs and search",
    "Databases",
    "Productivity",
    "Cloud and infra",
    "AI and data",
    "Local files",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Http,
    Stdio,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Http => "http",
            Transport::Stdio => "stdio",
        }
    }
}

/// What adding the server will ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    #[serde(rename = "none")]
    None,
    /// A browser login: `mcpdial login`.
    #[serde(rename = "oauth")]
    OAuth,
    /// One token, in a header or an environment variable.
    #[serde(rename = "api-key")]
    ApiKey,
    /// Other environment: a connection string, a host, a user and password.
    #[serde(rename = "env")]
    Env,
}

impl Auth {
    pub fn as_str(self) -> &'static str {
        match self {
            Auth::None => "none",
            Auth::OAuth => "oauth",
            Auth::ApiKey => "api-key",
            Auth::Env => "env",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// What `add --catalog` takes: lowercase letters, digits and dashes.
    pub id: String,
    pub name: String,
    pub category: String,
    pub summary: String,
    /// A registry server name, like `io.github.owner/server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The config itself, for a server the registry lacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ServerConfig>,
    pub transport: Transport,
    pub auth: Auth,
}

/// Parse and validate a catalog document: a JSON array of entries.
pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let entries: Vec<Entry> = serde_json::from_str(text)
        .map_err(|e| Error::config(format!("catalog is not a list of entries: {e}")))?;
    validate(&entries)?;
    Ok(entries)
}

/// The copy built into the binary.
pub fn embedded() -> Vec<Entry> {
    parse(EMBEDDED).expect("the embedded catalog.json passes its own validation")
}

/// The schema, as code: the first problem found, naming the entry.
pub fn validate(entries: &[Entry]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let label = if entry.id.is_empty() {
            format!("entry {}", i + 1)
        } else {
            format!("entry {:?}", entry.id)
        };
        check(entry).map_err(|problem| Error::config(format!("catalog {label}: {problem}")))?;
        if !seen.insert(entry.id.as_str()) {
            return Err(Error::config(format!("catalog {label}: duplicate id")));
        }
    }
    Ok(())
}

fn check(entry: &Entry) -> std::result::Result<(), String> {
    let id_ok = !entry.id.is_empty()
        && entry.id.len() <= 64
        && entry
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !entry.id.starts_with('-');
    if !id_ok {
        return Err("id must be lowercase letters, digits and dashes".into());
    }
    if entry.name.trim().is_empty() {
        return Err("name is empty".into());
    }
    if entry.summary.trim().is_empty() {
        return Err("summary is empty".into());
    }
    if !CATEGORIES.contains(&entry.category.as_str()) {
        return Err(format!(
            "category {:?} is not one of {}",
            entry.category,
            CATEGORIES.join(", ")
        ));
    }
    match (&entry.registry, &entry.config) {
        (Some(name), None) => {
            if !name.contains('/') || name.contains(char::is_whitespace) {
                return Err(format!(
                    "registry {name:?} is not a registry name like io.github.owner/server"
                ));
            }
        }
        (None, Some(config)) => {
            match (&config.http, &config.stdio) {
                (Some(url), None) => {
                    if !(url.starts_with("https://") || url.starts_with("http://"))
                        || url.contains(char::is_whitespace)
                    {
                        return Err(format!("config.http {url:?} is not an http(s) URL"));
                    }
                }
                (None, Some(cmd)) => {
                    if cmd.trim().is_empty() {
                        return Err("config.stdio is empty".into());
                    }
                }
                _ => return Err("config needs exactly one of http and stdio".into()),
            }
            if config.kind() != entry.transport.as_str() {
                return Err(format!(
                    "transport says {} but config is {}",
                    entry.transport.as_str(),
                    config.kind()
                ));
            }
        }
        _ => return Err("needs exactly one of registry and config".into()),
    }
    Ok(())
}

pub fn find<'a>(entries: &'a [Entry], id: &str) -> Option<&'a Entry> {
    entries.iter().find(|e| e.id == id)
}

/// The entries by category, in [`CATEGORIES`] order, empty categories left out.
pub fn grouped(entries: &[Entry]) -> Vec<(&'static str, Vec<&Entry>)> {
    CATEGORIES
        .iter()
        .map(|c| (*c, entries.iter().filter(|e| e.category == *c).collect()))
        .filter(|(_, group): &(_, Vec<&Entry>)| !group.is_empty())
        .collect()
}

/// The config an entry describes. `server` is the registry's own entry for a
/// `registry` catalog entry, fetched by the caller so this stays pure.
pub fn convert(entry: &Entry, server: Option<&Value>) -> Result<Resolved> {
    convert_given(entry, server, &[])
}

/// [`convert`] with values for what the registry server leaves to the user, as
/// [`registry::convert_given`] takes them. The saved config records the entry's
/// id under `source.catalog`, so `browse` can tell it is installed.
pub fn convert_given(
    entry: &Entry,
    server: Option<&Value>,
    given: &[Option<String>],
) -> Result<Resolved> {
    let mut resolved = match &entry.config {
        Some(config) => Resolved {
            config: config.clone(),
            ..Default::default()
        },
        None => {
            let Some(server) = server else {
                return Err(Error::config(format!(
                    "catalog entry {} names a registry server but none was looked up",
                    entry.id
                )));
            };
            registry::convert_given(server, &pick(entry.transport, server), given)?
        }
    };
    resolved
        .config
        .source
        .get_or_insert_with(Default::default)
        .catalog = Some(entry.id.clone());
    Ok(resolved)
}

/// What to take from the registry entry so the saved server has the transport
/// the catalog promised: the remote for `http`, a runnable package for `stdio`.
fn pick(transport: Transport, server: &Value) -> Pick {
    match transport {
        Transport::Http => Pick::Remote,
        Transport::Stdio => server["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["registryType"].as_str())
            .find(|kind| matches!(*kind, "npm" | "pypi" | "oci"))
            .map(|kind| Pick::Package(kind.to_string()))
            .unwrap_or(Pick::Any),
    }
}

/// The registry's server for an entry that names one, as [`convert`] takes it;
/// `None` for an entry that carries its own config.
pub fn lookup(entry: &Entry, registry: &Registry) -> Result<Option<Value>> {
    match &entry.registry {
        Some(name) => Ok(Some(registry.latest(name)?.ok_or_else(|| {
            Error::config(format!(
                "the registry no longer lists {name}, which catalog entry {} points at",
                entry.id
            ))
        })?)),
        None => Ok(None),
    }
}

/// [`convert`], looking the registry server up first when the entry names one.
pub fn resolve(entry: &Entry, registry: &Registry) -> Result<Resolved> {
    convert(entry, lookup(entry, registry)?.as_ref())
}

/// Where the catalog is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Url(String),
    File(PathBuf),
}

impl Source {
    pub fn from_env() -> Self {
        Self::parse(std::env::var(ENV_SOURCE).ok().as_deref())
    }

    /// `MCPDIAL_CATALOG` as given: a URL when it starts with a scheme, else a path,
    /// else the default URL when unset or blank.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) if v.starts_with("https://") || v.starts_with("http://") => {
                Source::Url(v.to_string())
            }
            Some(v) => Source::File(PathBuf::from(v)),
            None => Source::Url(DEFAULT_URL.to_string()),
        }
    }
}

pub struct Loaded {
    pub entries: Vec<Entry>,
    /// Where the entries came from, for a trace: the file, the cache, the fetch, or
    /// the embedded copy and why.
    pub origin: String,
}

/// The freshest catalog at hand. A file named by the source is read as is, and a
/// bad one is an error: it was asked for by name. A URL is fetched at most once a
/// day, and anything that goes wrong on the way falls back to the embedded copy.
pub fn load(
    store: &Store,
    source: &Source,
    offline: bool,
    timeout: Duration,
    user_agent: &str,
) -> Result<Loaded> {
    let url = match source {
        Source::File(path) => {
            let text = fs::read_to_string(path)
                .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
            let entries =
                parse(&text).map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
            return Ok(Loaded {
                entries,
                origin: format!("file {}", path.display()),
            });
        }
        Source::Url(url) => url,
    };
    if offline {
        return Ok(Loaded {
            entries: embedded(),
            origin: "embedded copy (--offline)".into(),
        });
    }
    let cache = store.dir.join(CACHE_FILE);
    if let Some(entries) = read_cache(&cache, url, SystemTime::now()) {
        return Ok(Loaded {
            entries,
            origin: format!("cached copy of {url}"),
        });
    }
    match fetch(url, timeout.min(REFRESH_TIMEOUT), user_agent).and_then(|text| parse(&text)) {
        Ok(entries) => {
            write_cache(&cache, url, &entries);
            Ok(Loaded {
                entries,
                origin: format!("fetched from {url}"),
            })
        }
        Err(e) => Ok(Loaded {
            entries: embedded(),
            origin: format!("embedded copy ({e})"),
        }),
    }
}

/// The cache remembers which URL it came from, so pointing `MCPDIAL_CATALOG` at
/// another one never serves yesterday's default list.
#[derive(Serialize, Deserialize)]
struct Cache {
    source: String,
    entries: Vec<Entry>,
}

fn read_cache(path: &Path, url: &str, now: SystemTime) -> Option<Vec<Entry>> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    if now.duration_since(modified).unwrap_or(Duration::ZERO) > CACHE_TTL {
        return None;
    }
    let cache: Cache = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    if cache.source != url || validate(&cache.entries).is_err() {
        return None;
    }
    Some(cache.entries)
}

/// Best effort: a cache that cannot be written only costs a fetch next time.
fn write_cache(path: &Path, url: &str, entries: &[Entry]) {
    let cache = Cache {
        source: url.to_string(),
        entries: entries.to_vec(),
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    if let Ok(text) = serde_json::to_string_pretty(&cache) {
        fs::write(path, text).ok();
    }
}

fn fetch(url: &str, timeout: Duration, user_agent: &str) -> Result<String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut resp = agent
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", user_agent)
        .call()
        .map_err(|e| Error::transport(format!("could not fetch {url}: {e}")))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(Error::transport(format!("{url} answered HTTP {status}")));
    }
    resp.body_mut()
        .read_to_string()
        .map_err(|e| Error::transport(format!("could not read {url}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str) -> Entry {
        Entry {
            id: id.into(),
            name: "Acme".into(),
            category: "Databases".into(),
            summary: "A test entry".into(),
            registry: Some("io.github.acme/box".into()),
            config: None,
            transport: Transport::Stdio,
            auth: Auth::Env,
        }
    }

    /// com.supabase/mcp, as the registry serves it: a remote and an npm package.
    fn remote_and_package() -> Value {
        json!({
            "name": "com.supabase/mcp",
            "version": "0.12.0",
            "remotes": [{"type": "streamable-http", "url": "https://mcp.supabase.com/mcp"}],
            "packages": [{
                "registryType": "npm", "identifier": "@supabase/mcp-server-supabase",
                "version": "0.12.0", "transport": {"type": "stdio"}
            }]
        })
    }

    #[test]
    fn the_embedded_catalog_validates_and_covers_every_category() {
        let entries = embedded();
        assert!(entries.len() >= 50, "{} entries", entries.len());
        for category in CATEGORIES {
            assert!(
                entries.iter().any(|e| e.category == category),
                "no entry under {category}"
            );
        }
        assert!(entries.iter().any(|e| e.registry.is_some()));
        assert!(entries.iter().any(|e| e.config.is_some()));
        assert_eq!(grouped(&entries).len(), CATEGORIES.len());
        assert_eq!(grouped(&entries)[0].0, "Source control");
        assert_eq!(find(&entries, "github").unwrap().name, "GitHub");
        assert!(find(&entries, "nope").is_none());
    }

    #[test]
    fn a_registry_entry_converts_with_the_transport_the_catalog_promised() {
        let server = remote_and_package();
        let mut e = entry("supabase");
        e.registry = Some("com.supabase/mcp".into());

        e.transport = Transport::Http;
        let r = convert(&e, Some(&server)).unwrap();
        assert_eq!(
            r.config.http.as_deref(),
            Some("https://mcp.supabase.com/mcp")
        );
        assert!(r.missing.is_empty());
        let source = r.config.source.as_ref().unwrap();
        assert_eq!(
            (
                source.catalog.as_deref(),
                source.registry.as_deref(),
                source.version.as_deref()
            ),
            (Some("supabase"), Some("com.supabase/mcp"), Some("0.12.0")),
            "the catalog id joins the registry's provenance"
        );

        e.transport = Transport::Stdio;
        let r = convert(&e, Some(&server)).unwrap();
        assert_eq!(
            r.config.stdio.as_deref(),
            Some("npx -y @supabase/mcp-server-supabase@0.12.0"),
            "the package, although a remote exists"
        );

        let mut package_only = remote_and_package();
        package_only["remotes"] = json!([]);
        e.transport = Transport::Http;
        assert!(convert(&e, Some(&package_only)).is_err());
        assert!(
            convert(&e, None).is_err(),
            "a registry entry needs the registry's server"
        );
    }

    #[test]
    fn a_config_entry_round_trips_and_converts_to_itself() {
        let mut cfg = ServerConfig::stdio("uvx postgres-mcp --access-mode=restricted");
        cfg.env
            .insert("DATABASE_URI".into(), "${DATABASE_URI}".into());
        let e = Entry {
            id: "postgres".into(),
            name: "Postgres".into(),
            category: "Databases".into(),
            summary: "Query a database".into(),
            registry: None,
            config: Some(cfg.clone()),
            transport: Transport::Stdio,
            auth: Auth::Env,
        };
        let text = serde_json::to_string_pretty(std::slice::from_ref(&e)).unwrap();
        assert!(!text.contains("registry"), "{text}");
        assert_eq!(parse(&text).unwrap(), std::slice::from_ref(&e));
        let first: Value = serde_json::from_str::<Value>(&text).unwrap()[0].take();
        assert_eq!(first["config"]["env"]["DATABASE_URI"], "${DATABASE_URI}");
        assert_eq!(first["auth"], "env");

        let r = convert(&e, None).unwrap();
        let mut expected = cfg;
        expected.source = Some(crate::config::Source {
            catalog: Some("postgres".into()),
            ..Default::default()
        });
        assert_eq!(r.config, expected, "as it is, plus where it came from");
        assert!(r.notes.is_empty() && r.missing.is_empty());
    }

    #[test]
    fn validation_names_the_entry_and_the_problem() {
        let problem = |entries: &[Entry]| validate(entries).unwrap_err().to_string();

        let mut e = entry("ok");
        assert!(validate(&[e.clone()]).is_ok());
        e.id = "Not OK".into();
        assert!(problem(&[e.clone()]).contains("id must be"));
        e.id = "ok".into();

        e.category = "Games".into();
        let msg = problem(&[e.clone()]);
        assert!(
            msg.starts_with(
                "catalog entry \"ok\": category \"Games\" is not one of Source control"
            ),
            "{msg}"
        );
        e.category = "Databases".into();

        e.config = Some(ServerConfig::stdio("x"));
        assert!(problem(&[e.clone()]).contains("exactly one of registry and config"));
        e.registry = None;
        assert!(validate(&[e.clone()]).is_ok());
        e.transport = Transport::Http;
        assert!(problem(&[e.clone()]).contains("transport says http but config is stdio"));
        e.config = Some(ServerConfig::http("ftp://x"));
        assert!(problem(&[e.clone()]).contains("not an http(s) URL"));
        e.config = Some(ServerConfig::default());
        assert!(problem(&[e.clone()]).contains("exactly one of http and stdio"));
        e.config = None;
        assert!(problem(&[e.clone()]).contains("exactly one of registry and config"));

        e.registry = Some("context7".into());
        assert!(problem(&[e.clone()]).contains("not a registry name"));

        assert!(problem(&[entry("twice"), entry("twice")]).contains("duplicate id"));
        assert!(parse("{}").unwrap_err().to_string().contains("not a list"));
        let unknown_field = r#"[{"id":"a","name":"A","category":"Databases","summary":"s",
            "registry":"io.github.a/b","transport":"http","auth":"none","stars":5}]"#;
        assert!(parse(unknown_field).is_err());
        assert!(parse(
            r#"[{"id":"a","name":"A","category":"Databases","summary":"s",
            "registry":"io.github.a/b","transport":"http","auth":"password"}]"#
        )
        .is_err());
    }

    #[test]
    fn the_source_is_a_url_a_path_or_the_default() {
        assert_eq!(Source::parse(None), Source::Url(DEFAULT_URL.into()));
        assert_eq!(Source::parse(Some("  ")), Source::Url(DEFAULT_URL.into()));
        assert_eq!(
            Source::parse(Some("https://example.test/c.json")),
            Source::Url("https://example.test/c.json".into())
        );
        assert_eq!(
            Source::parse(Some("./catalog.json")),
            Source::File(PathBuf::from("./catalog.json"))
        );
    }

    #[test]
    fn the_cache_lasts_a_day_and_is_keyed_by_its_source() {
        let dir = std::env::temp_dir().join(format!(
            "mcpdial-catalog-cache-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join(CACHE_FILE);
        let entries = vec![entry("box")];
        assert!(read_cache(&path, DEFAULT_URL, SystemTime::now()).is_none());

        write_cache(&path, DEFAULT_URL, &entries);
        let now = SystemTime::now();
        assert_eq!(read_cache(&path, DEFAULT_URL, now), Some(entries.clone()));
        assert!(
            read_cache(&path, "https://other.test/catalog.json", now).is_none(),
            "another source does not read this cache"
        );
        assert!(
            read_cache(&path, DEFAULT_URL, now + CACHE_TTL + Duration::from_secs(1)).is_none(),
            "a day later it is stale"
        );

        fs::write(&path, "{not json").unwrap();
        assert!(read_cache(&path, DEFAULT_URL, now).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}
