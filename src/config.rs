//! On-disk state: the list of named servers and the credentials acquired for them.
//!
//! Layout, under `$MCPDIAL_HOME`, else `$XDG_CONFIG_HOME/mcpdial`, else `~/.config/mcpdial`:
//!
//! ```text
//! servers.json      what you configured: transport, URL or command, headers
//! credentials.json  what was acquired: tokens, refresh tokens, OAuth client ids
//! probes.json       what `ls` last saw: one status per server, with its timestamp
//! *.json.lock       empty; held while a file is rewritten, see [`FileLock`]
//! ```
//!
//! Tokens live in a separate file so `servers.json` can be shared or committed and
//! the secret-bearing file can stay private: mode 0600 on unix, and on Windows a
//! discretionary access list naming the owning account alone.

use crate::protocol::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Extra environment for a stdio server's process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory for a stdio server's process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl ServerConfig {
    pub fn http(url: impl Into<String>) -> Self {
        Self {
            http: Some(url.into()),
            ..Default::default()
        }
    }
    pub fn stdio(cmd: impl Into<String>) -> Self {
        Self {
            stdio: Some(cmd.into()),
            ..Default::default()
        }
    }
    pub fn kind(&self) -> &'static str {
        if self.http.is_some() {
            "http"
        } else {
            "stdio"
        }
    }
    pub fn location(&self) -> &str {
        self.http.as_deref().or(self.stdio.as_deref()).unwrap_or("")
    }

    /// This server with every `${VAR}` placeholder filled in, as [`expand`] does
    /// it: what gets dialed, never what gets saved, so the file keeps naming the
    /// variable instead of holding its value. `token_env` names a variable rather
    /// than containing one and is left alone.
    pub fn expanded(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let fill = |what: &str, text: &str| {
            substitute(text, &lookup)
                .map_err(|var| Error::config(format!("{what} refers to ${var}, which is not set")))
        };
        let mut headers = BTreeMap::new();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), fill(&format!("header {name}"), value)?);
        }
        let mut env = BTreeMap::new();
        for (name, value) in &self.env {
            env.insert(name.clone(), fill(&format!("env {name}"), value)?);
        }
        Ok(Self {
            http: self
                .http
                .as_deref()
                .map(|u| fill("http URL", u))
                .transpose()?,
            stdio: self
                .stdio
                .as_deref()
                .map(|c| fill("stdio command", c))
                .transpose()?,
            headers,
            token_env: self.token_env.clone(),
            env,
            cwd: self.cwd.as_deref().map(|d| fill("cwd", d)).transpose()?,
        })
    }
}

/// `text` with each `${VAR}` replaced by what `lookup` gives for `VAR`, and each
/// `${VAR:-default}` by `default` when it gives nothing. `$$` is a literal `$`; any
/// other `$` is left as it is. An empty value counts as unset, as it does for `:-`
/// in a shell. A `${VAR}` with no default and no value is an error naming it.
pub fn expand(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    substitute(text, &lookup).map_err(|var| Error::config(format!("${var} is not set")))
}

/// [`expand`], handing back the unset variable's name so the caller can say
/// where it was found.
fn substitute(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("$$") {
            out.push('$');
            rest = after;
            continue;
        }
        let Some((inner, after)) = rest
            .strip_prefix("${")
            .and_then(|after| after.split_once('}'))
        else {
            // A `$` that opens no placeholder is text.
            out.push('$');
            rest = &rest[1..];
            continue;
        };
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        if name.is_empty() {
            // `${}` is nobody's variable.
            out.push('$');
            rest = &rest[1..];
            continue;
        }
        match lookup(name)
            .filter(|v| !v.is_empty())
            .as_deref()
            .or(default)
        {
            Some(value) => out.push_str(value),
            None => return Err(name.to_string()),
        }
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
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
    /// Secret of a confidential client registered out of band. Kept so a refresh can
    /// authenticate on its own; the file it lives in is mode 0600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// How that secret is presented: "client_secret_post" or "client_secret_basic".
    /// Refresh does no discovery, so the choice made at login has to survive with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    /// The loopback port the client id was registered with; the redirect URI must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
    /// The loopback host in that redirect URI: "127.0.0.1" or "localhost".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_host: Option<String>,
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

/// One remembered status, so a routine `mcpdial ls` need not dial anything.
///
/// `status` and `auth` are kept as the JSON the probe produced rather than as
/// their enums: a status variant this build has never heard of is then a cache
/// miss and a fresh probe, not a file this build refuses to read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProbeRecord {
    /// Unix seconds when the probe ran.
    pub checked_at: u64,
    /// What the probe was of; see `client::probe_keys`.
    pub key: u64,
    pub status: serde_json::Value,
    pub auth: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<usize>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProbesFile {
    #[serde(default)]
    probes: BTreeMap<String, ProbeRecord>,
}

/// The config directory and the files in it.
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    /// Resolve the config directory from the environment.
    pub fn from_env() -> Result<Self> {
        if let Some(d) = std::env::var_os(ENV_HOME).filter(|v| !v.is_empty()) {
            return Ok(Self {
                dir: PathBuf::from(d),
            });
        }
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self {
                dir: PathBuf::from(x).join("mcpdial"),
            });
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| Error::config("cannot locate a home directory; set MCPDIAL_HOME"))?;
        Ok(Self {
            dir: PathBuf::from(home).join(".config").join("mcpdial"),
        })
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
    /// Where the interactive shell keeps its line history. A saved server gets
    /// its own file, since the tool names recalled there are its own; ad-hoc
    /// targets share one. `name` has been through [`validate_name`], so it is a
    /// single safe path segment.
    pub fn history_path(&self, name: Option<&str>) -> PathBuf {
        match name {
            Some(name) => self.dir.join(format!("history-{name}")),
            None => self.dir.join("history"),
        }
    }
    pub fn probes_path(&self) -> PathBuf {
        self.dir.join("probes.json")
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
        let path = self.servers_path();
        let _lock = FileLock::acquire(&path)?;
        let mut file = read_json::<ServersFile>(&path)?;
        file.servers.insert(name.to_string(), cfg);
        write_json(&path, &file, false)
    }

    pub fn remove_server(&self, name: &str) -> Result<bool> {
        let path = self.servers_path();
        let removed = {
            let _lock = FileLock::acquire(&path)?;
            let mut file = read_json::<ServersFile>(&path)?;
            let removed = file.servers.remove(name).is_some();
            if removed {
                write_json(&path, &file, false)?;
            }
            removed
        };
        // Outside the servers lock: the credential has a lock of its own, and
        // holding two at once is how lock orders start to matter.
        if removed {
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

    /// Insert or replace one credential, leaving every other entry alone even
    /// if another thread or another mcpdial is saving at the same moment.
    pub fn save_credential(&self, name: &str, cred: Credential) -> Result<()> {
        let path = self.credentials_path();
        let _lock = FileLock::acquire(&path)?;
        let mut file = read_json::<CredentialsFile>(&path)?;
        file.credentials.insert(name.to_string(), cred);
        write_json(&path, &file, true)
    }

    pub fn remove_credential(&self, name: &str) -> Result<bool> {
        let path = self.credentials_path();
        let _lock = FileLock::acquire(&path)?;
        let mut file = read_json::<CredentialsFile>(&path)?;
        let removed = file.credentials.remove(name).is_some();
        if removed {
            write_json(&path, &file, true)?;
        }
        Ok(removed)
    }

    // -- remembered probes ---------------------------------------------------

    pub fn probes(&self) -> Result<BTreeMap<String, ProbeRecord>> {
        Ok(read_json::<ProbesFile>(&self.probes_path())?.probes)
    }

    /// Record the probes just taken, keeping what other servers last reported
    /// and dropping whatever is left over from a server that no longer exists.
    pub fn save_probes(&self, taken: BTreeMap<String, ProbeRecord>) -> Result<()> {
        let saved = self.servers()?;
        let path = self.probes_path();
        let _lock = FileLock::acquire(&path)?;
        let mut file = read_json::<ProbesFile>(&path)?;
        file.probes.extend(taken);
        file.probes.retain(|name, _| saved.contains_key(name));
        write_json(&path, &file, false)
    }
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
    let tmp = temp_path(path);
    if let Err(e) = write_file(&tmp, text.as_bytes(), private) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::config(format!("{}: {e}", tmp.display())));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::config(format!("{}: {e}", path.display())));
    }
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Create `path` and fill it. `private` asks for owner-only access from the
/// start, so the secrets are never briefly readable by anyone else.
fn write_file(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    #[cfg(not(any(unix, windows)))]
    let _ = private; // nothing to restrict the file with here
    use std::io::Write;
    #[cfg(windows)]
    if private {
        return sys::create_owner_only(path)?.write_all(bytes);
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

/// A sibling of `path` to write into before the rename, unique per call.
///
/// One shared temp name would put two writers in the same file, which is the
/// half-written state the rename exists to prevent. Process id separates
/// processes, the clock and the counter separate writes within one.
fn temp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{nanos}.{seq}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// The lock guarding `path`: `credentials.json` -> `credentials.json.lock`.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

#[cfg(unix)]
mod sys {
    use std::os::raw::c_int;

    pub const LOCK_EX: c_int = 2;
    pub const LOCK_UN: c_int = 8;

    extern "C" {
        pub fn flock(fd: c_int, operation: c_int) -> c_int;
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::{c_void, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr;
    use windows_sys::Win32::Foundation::{LocalFree, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Create `path` reachable by the account that runs mcpdial and nobody else,
    /// which is as close as Windows comes to mode 0600.
    ///
    /// `D:P(A;;FA;;;<sid>)` is one access-allowed entry for that account; the `P`
    /// stops the entries a profile directory hands down - Users, Authenticated
    /// Users - from being merged in alongside it.
    ///
    /// The descriptor goes to `CreateFileW` rather than onto the finished file,
    /// because a credentials file that exists for even an instant under inherited
    /// permissions is one another account has already had its chance at. That is
    /// also why the disposition is `CREATE_NEW`: `CREATE_ALWAYS` would silently
    /// ignore the descriptor and keep whatever a file already sitting there had.
    pub fn create_owner_only(path: &Path) -> io::Result<File> {
        let descriptor = descriptor(&format!("D:P(A;;FA;;;{})", current_user_sid()?))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    /// The SID of the account this process runs as, in `S-1-5-...` form.
    pub fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let handle = token.as_raw_handle();

        let mut len = 0u32;
        // The first call only sizes the buffer, so it is expected to fail.
        unsafe { GetTokenInformation(handle, TokenUser, ptr::null_mut(), 0, &mut len) };
        // TOKEN_USER holds a pointer, so the buffer has to be aligned for one.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let read = unsafe {
            GetTokenInformation(handle, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut text = ptr::null_mut();
        let converted = unsafe {
            ConvertSidToStringSidW((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut text)
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let text = LocalBuffer(text.cast());
        Ok(unsafe { from_wide(text.0.cast()) })
    }

    /// Build a security descriptor on the local heap from its SDDL form.
    fn descriptor(sddl: &str) -> io::Result<LocalBuffer> {
        let sddl = wide(sddl);
        let mut built: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut built,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalBuffer(built))
    }

    /// The discretionary access list on `path`, in SDDL form: the list's own
    /// flags, then one parenthesised entry per trustee.
    #[cfg(test)]
    pub fn dacl(path: &Path) -> io::Result<String> {
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

        let name = wide(path);
        let mut found: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut found,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        dacl_of(&LocalBuffer(found))
    }

    /// `sddl` as Windows hands an access list back. SDDL abbreviates well-known
    /// accounts, so a list naming the built-in administrator returns `LA` where
    /// it went in as a SID; an expected list has to make the same trip before it
    /// can be compared with one read off a file.
    #[cfg(test)]
    pub fn rendered(sddl: &str) -> io::Result<String> {
        dacl_of(&descriptor(sddl)?)
    }

    #[cfg(test)]
    fn dacl_of(descriptor: &LocalBuffer) -> io::Result<String> {
        use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

        let mut text = ptr::null_mut();
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let text = LocalBuffer(text.cast());
        Ok(unsafe { from_wide(text.0.cast()) })
    }

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// # Safety
    /// `p` must point at a NUL-terminated UTF-16 string.
    unsafe fn from_wide(p: *const u16) -> String {
        let mut len = 0;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
    }

    /// Anything the security APIs hand back on the local heap for us to release.
    struct LocalBuffer(*mut c_void);

    impl Drop for LocalBuffer {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// An exclusive lock on one config file, held across a read-modify-write.
///
/// Saving a credential reads the whole file, inserts one entry and writes it
/// back. Two of those interleaved lose an entry: both read before either
/// writes, so the second write drops what the first added. The window is the
/// whole operation, and `probe_all` walks into it by running a thread per
/// server, each of which may refresh and save a token. If the clobbered server
/// rotates its refresh token, the one left on disk has already been spent, so
/// listing servers can log you out of one. Two shells running mcpdial at once
/// race the same way, so the lock covers processes as well as threads.
///
/// Reads need no lock: [`write_json`] renames a complete file into place, so a
/// reader sees either the old contents or the new ones.
struct FileLock {
    #[cfg(unix)]
    file: fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl FileLock {
    /// Take the lock guarding `path`, waiting for any current holder to finish.
    fn acquire(path: &Path) -> Result<Self> {
        let lock = lock_path(path);
        if let Some(dir) = lock.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| Error::config(format!("{}: {e}", dir.display())))?;
        }
        Self::take(&lock).map_err(|e| Error::config(format!("{}: {e}", lock.display())))
    }
}

#[cfg(unix)]
impl FileLock {
    fn take(lock: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        // The lock file stays on disk once created. Unlinking it would race: a
        // waiter can be holding the old inode while the next writer creates a
        // new one, and then neither sees the other.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock)?;
        // flock lives on the open file description rather than the process, so
        // threads that each opened the file serialize against each other just
        // as separate processes do, and the kernel releases it when the
        // descriptor closes, so a crash cannot leave the config locked.
        loop {
            if unsafe { sys::flock(file.as_raw_fd(), sys::LOCK_EX) } == 0 {
                return Ok(Self { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Closing the descriptor would release it anyway; this says so aloud.
        let _ = unsafe { sys::flock(self.file.as_raw_fd(), sys::LOCK_UN) };
    }
}

#[cfg(not(unix))]
impl FileLock {
    fn take(lock: &Path) -> std::io::Result<Self> {
        use std::time::{Duration, Instant};
        // Without flock the lock is the existence of the file, created with the
        // atomic O_CREAT|O_EXCL. Nothing releases that if the holder dies, so a
        // file left untouched for long enough is treated as abandoned. Writes
        // are short, so the threshold is far above any honest hold.
        const STALE: Duration = Duration::from_secs(30);
        const GIVE_UP: Duration = Duration::from_secs(60);
        // How long a permission error is allowed to look like a departing holder.
        const DELETE_PENDING: Duration = Duration::from_secs(2);
        let deadline = Instant::now() + GIVE_UP;
        let mut denied_since: Option<Instant> = None;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock)
            {
                Ok(_) => {
                    return Ok(Self {
                        path: lock.to_path_buf(),
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => denied_since = None,
                // Windows answers `create_new` with a permission error both for a
                // lock file whose deletion has not finished and for a directory we
                // may not write to. Testing for the file cannot tell them apart: a
                // delete-pending file refuses GetFileAttributes the same way, so it
                // reads as absent. Only time separates them - a departing holder is
                // gone in milliseconds, an unwritable directory never is.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    if denied_since.get_or_insert_with(Instant::now).elapsed() > DELETE_PENDING {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
            let stale = fs::metadata(lock)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE));
            if stale {
                let _ = fs::remove_file(lock);
                continue;
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "another mcpdial is still holding this lock",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(not(unix))]
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own. The counter matters: the clock here is
    /// only microsecond-grained, so two tests starting together would otherwise
    /// share a directory and delete it out from under each other.
    fn temp_store() -> Store {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Store::at(
            std::env::temp_dir().join(format!("mcpdial-cfg-{}-{nanos}-{seq}", std::process::id())),
        )
    }

    #[test]
    fn round_trips_servers_and_credentials() {
        let s = temp_store();
        assert!(s.servers().unwrap().is_empty());

        s.add_server("wiki", ServerConfig::http("https://mcp.deepwiki.com/mcp"))
            .unwrap();
        s.add_server("fs", ServerConfig::stdio("npx -y fs /tmp"))
            .unwrap();
        let all = s.servers().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["wiki"].kind(), "http");
        assert_eq!(all["fs"].kind(), "stdio");

        s.save_credential(
            "wiki",
            Credential {
                access_token: Some("t".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.credential("wiki").unwrap().unwrap().has_token());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(s.credentials_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "credentials must not be world readable");
        }

        #[cfg(windows)]
        {
            let owner_only =
                sys::rendered(&format!("D:(A;;FA;;;{})", sys::current_user_sid().unwrap()))
                    .unwrap();
            let dacl = sys::dacl(&s.credentials_path()).unwrap();
            let (flags, entries) = dacl.split_once('(').expect("a discretionary list");
            let (_, expected) = owner_only.split_once('(').expect("a discretionary list");
            assert!(
                flags.contains('P'),
                "inherited entries must not apply: {dacl}"
            );
            assert_eq!(
                entries, expected,
                "credentials must name their owner and nobody else"
            );
        }

        assert!(s.remove_server("wiki").unwrap());
        assert!(
            s.credential("wiki").unwrap().is_none(),
            "removing a server drops its token"
        );
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
    fn concurrent_saves_keep_every_credential() {
        let store = temp_store();
        let names: Vec<String> = (0..8).map(|i| format!("server-{i}")).collect();

        std::thread::scope(|scope| {
            for name in &names {
                let store = store.clone();
                scope.spawn(move || {
                    store
                        .save_credential(
                            name,
                            Credential {
                                access_token: Some(name.clone()),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                });
            }
        });

        let saved = store.credentials().unwrap();
        assert_eq!(saved.len(), names.len(), "a concurrent save was clobbered");
        for name in &names {
            assert_eq!(saved[name].access_token.as_deref(), Some(name.as_str()));
        }
        fs::remove_dir_all(&store.dir).unwrap();
    }

    /// Two handles stand in for two mcpdial processes: nothing is shared in
    /// memory between them, so only the lock file can keep them apart.
    #[test]
    fn separate_store_handles_do_not_lose_entries() {
        const ROUNDS: usize = 25;
        let dir = temp_store().dir;
        let (a, b) = (Store::at(&dir), Store::at(&dir));

        std::thread::scope(|scope| {
            for (store, tag) in [(&a, "a"), (&b, "b")] {
                scope.spawn(move || {
                    for i in 0..ROUNDS {
                        store
                            .save_credential(
                                &format!("{tag}-{i}"),
                                Credential {
                                    access_token: Some(tag.to_string()),
                                    ..Default::default()
                                },
                            )
                            .unwrap();
                    }
                });
            }
        });

        assert_eq!(a.credentials().unwrap().len(), ROUNDS * 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temp_paths_are_unique_per_write() {
        let path = Path::new("/somewhere/mcpdial/credentials.json");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let tmp = temp_path(path);
            assert_eq!(
                tmp.parent(),
                path.parent(),
                "the temp file is a sibling, so the rename stays on one filesystem"
            );
            assert_eq!(tmp.extension().and_then(|e| e.to_str()), Some("tmp"));
            assert!(seen.insert(tmp), "two writes must not share a temp file");
        }
    }

    #[test]
    fn lock_paths_sit_beside_the_file_they_guard() {
        let path = Path::new("/somewhere/mcpdial/credentials.json");
        assert_eq!(
            lock_path(path),
            Path::new("/somewhere/mcpdial/credentials.json.lock")
        );
    }

    #[test]
    fn expiry_has_a_safety_margin() {
        let c = Credential {
            expires_at: Some(now() + 10),
            ..Default::default()
        };
        assert!(c.is_expired(), "10s left counts as expired");
        let c = Credential {
            expires_at: Some(now() + 3600),
            ..Default::default()
        };
        assert!(!c.is_expired());
        assert!(!Credential::default().is_expired());
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn expands_placeholders_from_the_lookup() {
        let env = vars(&[("A", "apple"), ("EMPTY", "")]);
        assert_eq!(expand("plain text", &env).unwrap(), "plain text");
        assert_eq!(expand("", &env).unwrap(), "");
        assert_eq!(expand("Bearer ${A}", &env).unwrap(), "Bearer apple");
        assert_eq!(expand("${A}${A}", &env).unwrap(), "appleapple");
        assert_eq!(expand("${B:-x}", &env).unwrap(), "x");
        assert_eq!(expand("${A:-x}", &env).unwrap(), "apple");
        assert_eq!(expand("${B:-}", &env).unwrap(), "");
        assert_eq!(expand("${B:-a:-b}", &env).unwrap(), "a:-b");
        assert_eq!(expand("costs $$5", &env).unwrap(), "costs $5");
        assert_eq!(expand("$${A}", &env).unwrap(), "${A}");
        assert_eq!(expand("$A and $ alone", &env).unwrap(), "$A and $ alone");
        assert_eq!(expand("${A", &env).unwrap(), "${A");
        assert_eq!(expand("${}", &env).unwrap(), "${}");
        assert_eq!(expand("$", &env).unwrap(), "$");
    }

    #[test]
    fn an_unset_placeholder_without_a_default_names_the_variable() {
        let env = vars(&[("EMPTY", "")]);
        let err = expand("Bearer ${GITHUB_TOKEN}", &env).unwrap_err();
        assert!(matches!(&err, Error::Config(m) if m == "$GITHUB_TOKEN is not set"));
        // Empty is as good as unset, as `:-` treats it in a shell.
        assert!(expand("${EMPTY}", &env).is_err());
        assert_eq!(expand("${EMPTY:-fallback}", &env).unwrap(), "fallback");
    }

    #[test]
    fn an_expanded_config_names_the_field_and_leaves_the_original_alone() {
        let env = vars(&[("KEY", "k"), ("DIR", "/srv"), ("PORT", "8080")]);
        let mut cfg = ServerConfig::stdio("serve ${DIR} --port ${PORT:-80}");
        cfg.env.insert("API_KEY".into(), "${KEY}".into());
        cfg.env.insert("FLAG".into(), "${FLAG:-off}".into());
        cfg.cwd = Some("${DIR}/work".into());
        let dialed = cfg.expanded(&env).unwrap();
        assert_eq!(dialed.stdio.as_deref(), Some("serve /srv --port 8080"));
        assert_eq!(dialed.env["API_KEY"], "k");
        assert_eq!(dialed.env["FLAG"], "off");
        assert_eq!(dialed.cwd.as_deref(), Some("/srv/work"));
        assert_eq!(cfg.cwd.as_deref(), Some("${DIR}/work"), "never rewritten");

        let mut http = ServerConfig::http("https://${HOST}/mcp");
        http.token_env = Some("${NOT_A_PLACEHOLDER}".into());
        let err = http.expanded(&env).unwrap_err();
        assert_eq!(
            err.to_string(),
            "http URL refers to $HOST, which is not set"
        );
        http.http = Some("https://x/mcp".into());
        http.headers
            .insert("Authorization".into(), "Bearer ${GITHUB_TOKEN}".into());
        let err = http.expanded(&env).unwrap_err();
        assert_eq!(
            err.to_string(),
            "header Authorization refers to $GITHUB_TOKEN, which is not set"
        );
        http.headers.clear();
        let dialed = http.expanded(&env).unwrap();
        assert_eq!(dialed.token_env.as_deref(), Some("${NOT_A_PLACEHOLDER}"));
    }
}
