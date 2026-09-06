//! The macOS keychain, through the `security` tool that ships with the system.
//!
//! `add-generic-password` takes the password as an argument, which would put the
//! token in `ps` for the length of the call. Its own manual page says not to: give
//! `-w` last and it prompts instead, reading the password and its confirmation from
//! stdin, which is where they go from here.
//!
//! First use of an item from a given binary raises the keychain's own permission
//! dialog. Declining it is an error out of `security`, reported as one; nothing
//! falls back to the file.

use super::helper::{failed, run};
use crate::protocol::Result;
use std::process::Output;

const TOOL: &str = "security";

/// What `security` exits with for `errSecItemNotFound`, on a lookup and a delete alike.
const NOT_FOUND: i32 = 44;

pub fn get(service: &str, account: &str) -> Result<Option<String>> {
    let out = run(
        TOOL,
        &["find-generic-password", "-s", service, "-a", account, "-w"],
        None,
    )?;
    if missing(&out) {
        return Ok(None);
    }
    if !out.status.success() {
        return Err(failed(
            TOOL,
            &format!("read the item for {account:?}"),
            &out,
        ));
    }
    decode(&out.stdout, account).map(Some)
}

pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
    // -U so a second login replaces the item rather than colliding with it, and a
    // label naming the account so Keychain Access does not show a column of
    // identical "mcpdial" rows.
    let label = format!("{service}: {account}");
    let out = run(
        TOOL,
        &[
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            account,
            "-l",
            label.as_str(),
            "-w",
        ],
        // Once for the password, once for the confirmation it asks for.
        Some(&format!("{secret}\n{secret}\n")),
    )?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failed(
            TOOL,
            &format!("save the item for {account:?}"),
            &out,
        ))
    }
}

pub fn delete(service: &str, account: &str) -> Result<bool> {
    let out = run(
        TOOL,
        &["delete-generic-password", "-s", service, "-a", account],
        None,
    )?;
    if missing(&out) {
        return Ok(false);
    }
    if out.status.success() {
        Ok(true)
    } else {
        Err(failed(
            TOOL,
            &format!("remove the item for {account:?}"),
            &out,
        ))
    }
}

/// Every account under `service`.
///
/// `find-generic-password` returns one item and there is no flag for all of them,
/// so this reads the dump instead. Attributes only - no `-d`, so no secret is
/// decrypted and nothing prompts - and every item that is not ours is dropped.
pub fn accounts(service: &str) -> Result<Vec<String>> {
    let out = run(TOOL, &["dump-keychain"], None)?;
    if !out.status.success() {
        return Err(failed(TOOL, "list the keychain", &out));
    }
    Ok(listed(&String::from_utf8_lossy(&out.stdout), service))
}

fn listed(dump: &str, service: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut account = None;
    for line in dump.lines() {
        let line = line.trim();
        // Each item begins with its class, and its attributes follow in an order
        // that puts the account before the service, so the account is held until
        // the service says whether this item is ours.
        if line.starts_with("class:") {
            account = None;
        } else if let Some(value) = attribute(line, "acct") {
            account = Some(value);
        } else if attribute(line, "svce").as_deref() == Some(service) {
            found.extend(account.take());
        }
    }
    found
}

/// One `"key"<blob>="value"` attribute. A value `security` will not print as text
/// comes back as `0x...` or `<NULL>` instead, and neither is a name mcpdial wrote.
fn attribute(line: &str, key: &str) -> Option<String> {
    let value = line
        .strip_prefix(&format!("\"{key}\"<blob>="))?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    Some(value.to_string())
}

/// `security` reporting that there is no such item, as opposed to any other failure.
fn missing(out: &Output) -> bool {
    out.status.code() == Some(NOT_FOUND)
}

/// The password as `-w` printed it.
///
/// Text comes back as itself. Anything `security` will not print as text - which a
/// credential holding a non-ASCII scope or issuer is - comes back as a hex string
/// instead. Every item mcpdial writes is a JSON object, so the leading brace tells
/// the two apart with nothing left to guess at.
fn decode(stdout: &[u8], account: &str) -> Result<String> {
    let printed = String::from_utf8_lossy(stdout);
    let printed = printed.trim_end_matches(['\r', '\n']);
    if printed.starts_with('{') {
        return Ok(printed.to_string());
    }
    let bytes = from_hex(printed).ok_or_else(|| super::not_a_credential(account))?;
    String::from_utf8(bytes).map_err(|_| super::not_a_credential(account))
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_hex_both_decode() {
        assert_eq!(decode(b"{\"a\":1}\n", "wiki").unwrap(), "{\"a\":1}");
        assert_eq!(decode(b"7b2261223a317d\n", "wiki").unwrap(), "{\"a\":1}");
        assert!(decode(b"not a credential\n", "wiki").is_err());
    }

    #[test]
    fn a_dump_lists_our_service_alone() {
        let dump = "\
class: \"genp\"
attributes:
    \"acct\"<blob>=\"wiki\"
    \"svce\"<blob>=\"mcpdial\"
class: \"genp\"
attributes:
    \"acct\"<blob>=\"someone@example.com\"
    \"svce\"<blob>=\"another app\"
class: \"genp\"
attributes:
    \"acct\"<blob>=\"work\"
    \"svce\"<blob>=\"mcpdial\"
";
        assert_eq!(listed(dump, "mcpdial"), ["wiki", "work"]);
        assert!(listed(dump, "nothing").is_empty());
    }

    #[test]
    fn an_unreadable_attribute_is_no_account() {
        assert_eq!(
            attribute("\"acct\"<blob>=\"wiki\"", "acct").unwrap(),
            "wiki"
        );
        assert!(attribute("\"acct\"<blob>=<NULL>", "acct").is_none());
        assert!(attribute("\"svce\"<blob>=\"other\"", "acct").is_none());
    }
}
