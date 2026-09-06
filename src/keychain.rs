//! The OS keychain, as an alternative home for the tokens `credentials.json` holds.
//!
//! `credentials.json` at mode 0600 is the default and stays the default. This is for
//! the reviewer who will not accept a plaintext file however it is permissioned: set
//! `credentials` to `keychain` and every token moves into the macOS keychain, the
//! Linux Secret Service or the Windows Credential Manager, one item per credential,
//! under service [`SERVICE`] with the credential's own key as the account.
//!
//! No crate stands behind this. macOS drives the `security` tool that ships with the
//! system, Linux drives `secret-tool` from libsecret, and Windows calls the credential
//! API through `windows-sys`, which is already a dependency for the access list on
//! `credentials.json`. The binary stays one static file.
//!
//! Two rules hold everywhere in here. A secret is never an argument, because a command
//! line is public to every process the user runs; it goes in on stdin, or in a buffer
//! this process owns. And no error message interpolates a stored value: a keychain
//! item is a token, and failing to parse one must not print it.

use crate::protocol::{Error, Result};

/// Set to `file` or `keychain` to override the saved setting, so CI can hold a run
/// to the file whatever the config directory it inherited says.
pub const ENV_BACKEND: &str = "MCPDIAL_CREDENTIALS";

/// The service every item is filed under, so one search finds all of them and
/// nothing else in the user's keychain is ever touched.
pub const SERVICE: &str = "mcpdial";

/// Where saved credentials are kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// `credentials.json`, owner only. The default.
    #[default]
    File,
    /// One item per credential in the OS keychain.
    Keychain,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::File => "file",
            Backend::Keychain => "keychain",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "file" => Ok(Backend::File),
            "keychain" => Ok(Backend::Keychain),
            other => Err(Error::config(format!(
                "unknown credential store {other:?}: use \"file\" or \"keychain\""
            ))),
        }
    }
}

/// What is stored for `account`, or `None` when there is no such item.
pub fn get(account: &str) -> Result<Option<String>> {
    imp::get(SERVICE, account)
}

/// Store `secret` for `account`, replacing whatever was there.
pub fn set(account: &str, secret: &str) -> Result<()> {
    imp::set(SERVICE, account, secret)
}

/// Drop `account`'s item, reporting whether there was one.
pub fn delete(account: &str) -> Result<bool> {
    imp::delete(SERVICE, account)
}

/// Every account mcpdial has an item for, sorted.
pub fn accounts() -> Result<Vec<String>> {
    let mut found = imp::accounts(SERVICE)?;
    found.sort();
    found.dedup();
    Ok(found)
}

/// Whether this machine has a keychain mcpdial can reach: a search that succeeds,
/// even one that finds nothing. A headless Linux session with no Secret Service, a
/// `secret-tool` that is not installed and a locked keychain all fail here, which is
/// where they should fail - before anything has been moved.
pub fn check() -> Result<()> {
    accounts().map(|_| ())
}

/// A keychain item that is not a credential mcpdial wrote. Named, never quoted.
pub(crate) fn not_a_credential(account: &str) -> Error {
    Error::config(format!(
        "the {SERVICE} keychain item for {account:?} is not one mcpdial wrote; \
         remove it and log in again"
    ))
}

// The platform's own backend, compiled whether or not this is a test build, so that
// what it parses out of a keychain tool is type checked, linted and tested on every
// build. A test build reads the fake below instead of the machine's real keychain,
// which is what leaves these unreached there.
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
mod macos;
#[cfg(all(not(test), target_os = "macos"))]
use macos as imp;

#[cfg(all(unix, not(target_os = "macos")))]
#[cfg_attr(test, allow(dead_code))]
mod secret_service;
#[cfg(all(not(test), unix, not(target_os = "macos")))]
use secret_service as imp;

/// What the two unix backends share: they are both a command line away.
#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
mod helper;

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
mod credential_manager;
#[cfg(all(not(test), windows))]
use credential_manager as imp;

#[cfg(all(not(test), not(any(unix, windows))))]
use unsupported as imp;

/// Somewhere with neither a keychain nor a way to ask for one. Saying so is the
/// whole contract: the caller chose a store, and falling back to the file would
/// write the token somewhere it did not ask for.
#[cfg(not(any(unix, windows)))]
#[cfg_attr(test, allow(dead_code))]
mod unsupported {
    use crate::protocol::{Error, Result};

    fn refuse<T>() -> Result<T> {
        Err(Error::config(
            "no keychain on this platform; keep credentials in the file store",
        ))
    }
    pub fn get(_: &str, _: &str) -> Result<Option<String>> {
        refuse()
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        refuse()
    }
    pub fn delete(_: &str, _: &str) -> Result<bool> {
        refuse()
    }
    pub fn accounts(_: &str) -> Result<Vec<String>> {
        refuse()
    }
}

/// The keychain a unit test gets: the same contract, in this process, so the store's
/// own tests exercise the keychain paths on every platform and no test ever reaches
/// the machine's real keychain. The test that does reach it is opt-in; see
/// `tests/keychain.rs`.
#[cfg(test)]
use fake as imp;

#[cfg(test)]
pub mod fake {
    use crate::protocol::Result;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    type Items = Mutex<BTreeMap<(String, String), String>>;

    fn items() -> &'static Items {
        static ITEMS: OnceLock<Items> = OnceLock::new();
        ITEMS.get_or_init(Items::default)
    }

    /// Held for the length of a test that uses the keychain, and empty when it is
    /// handed over: one map stands for the machine's one keychain, so two such
    /// tests can no more run at once than two of them could on a real one.
    pub fn exclusive() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        items().lock().unwrap_or_else(|e| e.into_inner()).clear();
        guard
    }

    fn locked() -> impl std::ops::DerefMut<Target = BTreeMap<(String, String), String>> {
        items().lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>> {
        Ok(locked().get(&(service.into(), account.into())).cloned())
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        locked().insert((service.into(), account.into()), secret.into());
        Ok(())
    }

    pub fn delete(service: &str, account: &str) -> Result<bool> {
        Ok(locked().remove(&(service.into(), account.into())).is_some())
    }

    pub fn accounts(service: &str) -> Result<Vec<String>> {
        Ok(locked()
            .keys()
            .filter(|(s, _)| s == service)
            .map(|(_, a)| a.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_round_trip() {
        for backend in [Backend::File, Backend::Keychain] {
            assert_eq!(Backend::parse(backend.label()).unwrap(), backend);
        }
        assert_eq!(Backend::default(), Backend::File);
    }

    #[test]
    fn an_unknown_store_names_the_two_that_exist() {
        let message = Backend::parse("vault").unwrap_err().to_string();
        assert!(message.contains("vault"), "{message}");
        assert!(message.contains("file"), "{message}");
        assert!(message.contains("keychain"), "{message}");
    }

    #[test]
    fn a_broken_item_is_named_but_never_quoted() {
        let message = not_a_credential("wiki").to_string();
        assert!(message.contains("wiki"), "{message}");
        assert!(message.contains("log in again"), "{message}");
    }

    #[test]
    fn items_are_kept_per_account() {
        let _keychain = fake::exclusive();
        assert!(accounts().unwrap().is_empty());
        assert!(get("wiki").unwrap().is_none());

        set("wiki", "{\"access_token\":\"t\"}").unwrap();
        set("work", "{\"access_token\":\"u\"}").unwrap();
        assert_eq!(accounts().unwrap(), ["wiki", "work"]);
        assert_eq!(get("wiki").unwrap().unwrap(), "{\"access_token\":\"t\"}");

        set("wiki", "{\"access_token\":\"t2\"}").unwrap();
        assert_eq!(get("wiki").unwrap().unwrap(), "{\"access_token\":\"t2\"}");

        assert!(delete("wiki").unwrap());
        assert!(!delete("wiki").unwrap());
        assert_eq!(accounts().unwrap(), ["work"]);
    }
}
