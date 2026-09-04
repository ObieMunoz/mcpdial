//! On-disk state: the list of named servers and the credentials acquired for them.
//!
//! Layout, under `$MCPDIAL_HOME`, else `$XDG_CONFIG_HOME/mcpdial`, else `~/.config/mcpdial`:
//!
//! ```text
//! servers.json      what you configured: transport, URL or command, headers
//! credentials.json  what was acquired: tokens, refresh tokens, OAuth client ids (mode 0600)
//! ```
//!
//! Tokens live in a separate file so `servers.json` can be shared or committed and
//! the secret-bearing file can stay private.

use crate::protocol::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ENV_HOME: &str = "MCPDIAL_HOME";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    /// Streamable HTTP endpoint. Exactly one of `http` / `stdio` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// Command line that speaks MCP on stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdio: Option<String>,
    /// Extra HTTP headers sent on every request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Env var to read a bearer token from. Takes precedence over a saved credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

impl ServerConfig {
    pub fn http(url: impl Into<String>) -> Self {
        Self { http: Some(url.into()), ..Default::default() }
    }
    pub fn stdio(cmd: impl Into<String>) -> Self {
        Self { stdio: Some(cmd.into()), ..Default::default() }
    }
    pub fn kind(&self) -> &'static str {
        if self.http.is_some() { "http" } else { "stdio" }
    }
    pub fn location(&self) -> &str {
        self.http.as_deref().or(self.stdio.as_deref()).unwrap_or("")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Credential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` means the token does not expire, or we were not told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Where the token came from, kept so it can be refreshed without rediscovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// Dynamically registered client id, reused on the next login to the same issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The loopback port the client id was registered with; the redirect URI must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
    /// The `resource` indicator (RFC 8707) the token was issued for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// How the token was obtained: "oauth" or "manual".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Credential {
    pub fn is_expired(&self) -> bool {
        matches!(self.expires_at, Some(t) if t <= now() + 30)
    }
    pub fn has_token(&self) -> bool {
        self.access_token.as_deref().is_some_and(|t| !t.is_empty())
    }
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.is_some() && self.token_endpoint.is_some()
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServersFile {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    credentials: BTreeMap<String, Credential>,
}

/// The config directory and the two files in it.
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    /// Resolve the config directory from the environment.
    pub fn from_env() -> Result<Self> {
        if let Some(d) = std::env::var_os(ENV_HOME).filter(|v| !v.is_empty()) {
            return Ok(Self { dir: PathBuf::from(d) });
        }
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self { dir: PathBuf::from(x).join("mcpdial") });
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| Error::config("cannot locate a home directory; set MCPDIAL_HOME"))?;
        Ok(Self { dir: PathBuf::from(home).join(".config").join("mcpdial") })
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn servers_path(&self) -> PathBuf {
        self.dir.join("servers.json")
    }
    pub fn credentials_path(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }

    // -- servers -------------------------------------------------------------

    pub fn servers(&self) -> Result<BTreeMap<String, ServerConfig>> {
        Ok(read_json::<ServersFile>(&self.servers_path())?.servers)
    }

    pub fn server(&self, name: &str) -> Result<Option<ServerConfig>> {
        Ok(self.servers()?.remove(name))
    }

    pub fn add_server(&self, name: &str, cfg: ServerConfig) -> Result<()> {
        validate_name(name)?;
        let mut file = read_json::<ServersFile>(&self.servers_path())?;
        file.servers.insert(name.to_string(), cfg);
        write_json(&self.servers_path(), &file, false)
    }

    pub fn remove_server(&self, name: &str) -> Result<bool> {
        let mut file = read_json::<ServersFile>(&self.servers_path())?;
        let removed = file.servers.remove(name).is_some();
        if removed {
            write_json(&self.servers_path(), &file, false)?;
            self.remove_credential(name)?;
        }
        Ok(removed)
    }

    // -- credentials ---------------------------------------------------------

    pub fn credentials(&self) -> Result<BTreeMap<String, Credential>> {
        Ok(read_json::<CredentialsFile>(&self.credentials_path())?.credentials)
    }

    pub fn credential(&self, name: &str) -> Result<Option<Credential>> {
        Ok(self.credentials()?.remove(name))
    }

    pub fn save_credential(&self, name: &str, cred: Credential) -> Result<()> {
        let mut file = read_json::<CredentialsFile>(&self.credentials_path())?;
        file.credentials.insert(name.to_string(), cred);
        write_json(&self.credentials_path(), &file, true)
    }

    pub fn remove_credential(&self, name: &str) -> Result<bool> {
        let mut file = read_json::<CredentialsFile>(&self.credentials_path())?;
        let removed = file.credentials.remove(name).is_some();
        if removed {
            write_json(&self.credentials_path(), &file, true)?;
        }
        Ok(removed)
    }
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("://")
        && !name.starts_with("stdio:");
    if ok {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "invalid server name {name:?}: use letters, digits, '-', '_' or '.'"
        )))
    }
}

fn read_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(T::default()),
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Error::config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(Error::config(format!("{}: {e}", path.display()))),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| Error::config(format!("{}: {e}", dir.display())))?;
    let text = serde_json::to_string_pretty(value).expect("config is serializable") + "\n";
    // Write to a sibling and rename so a crash never leaves a half-written file.
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| Error::config(format!("{}: {e}", tmp.display())))?;
        use std::io::Write;
        f.write_all(text.as_bytes())
            .map_err(|e| Error::config(format!("{}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, path).map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> Store {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        Store::at(std::env::temp_dir().join(format!("mcpdial-cfg-{}-{nanos}", std::process::id())))
    }

    #[test]
    fn round_trips_servers_and_credentials() {
        let s = temp_store();
        assert!(s.servers().unwrap().is_empty());

        s.add_server("wiki", ServerConfig::http("https://mcp.deepwiki.com/mcp")).unwrap();
        s.add_server("fs", ServerConfig::stdio("npx -y fs /tmp")).unwrap();
        let all = s.servers().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["wiki"].kind(), "http");
        assert_eq!(all["fs"].kind(), "stdio");

        s.save_credential("wiki", Credential { access_token: Some("t".into()), ..Default::default() })
            .unwrap();
        assert!(s.credential("wiki").unwrap().unwrap().has_token());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(s.credentials_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials must not be world readable");
        }

        assert!(s.remove_server("wiki").unwrap());
        assert!(s.credential("wiki").unwrap().is_none(), "removing a server drops its token");
        assert!(!s.remove_server("wiki").unwrap());
        fs::remove_dir_all(&s.dir).unwrap();
    }

    #[test]
    fn rejects_names_that_collide_with_targets() {
        let s = temp_store();
        assert!(s.add_server("https://x", ServerConfig::http("u")).is_err());
        assert!(s.add_server("stdio:x", ServerConfig::http("u")).is_err());
        assert!(s.add_server("", ServerConfig::http("u")).is_err());
        assert!(s.add_server("has space", ServerConfig::http("u")).is_err());
    }

    #[test]
    fn expiry_has_a_safety_margin() {
        let c = Credential { expires_at: Some(now() + 10), ..Default::default() };
        assert!(c.is_expired(), "10s left counts as expired");
        let c = Credential { expires_at: Some(now() + 3600), ..Default::default() };
        assert!(!c.is_expired());
        assert!(!Credential::default().is_expired());
    }
}
