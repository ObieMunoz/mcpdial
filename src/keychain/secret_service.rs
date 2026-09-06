//! The freedesktop Secret Service - GNOME Keyring, KWallet - through `secret-tool`.
//!
//! `secret-tool` is libsecret's own command line and speaks D-Bus for us, which is
//! what keeps a D-Bus binding out of this binary. It is not always installed, and on
//! a headless session there may be no Secret Service behind it at all; both are
//! errors here rather than a quiet return to the file.
//!
//! `secret-tool search` prints the secret it found alongside the attributes. Its
//! output is parsed for account names and is never logged, quoted or returned.

use super::helper::{failed, run};
use crate::protocol::Result;

const TOOL: &str = "secret-tool";

pub fn get(service: &str, account: &str) -> Result<Option<String>> {
    let out = run(
        TOOL,
        &["lookup", "service", service, "account", account],
        None,
    )?;
    if out.status.success() {
        // The secret goes out with no trailing newline, but a store that added one
        // would not change the JSON; trimming makes both readable.
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ));
    }
    // A lookup that found nothing exits non-zero and says nothing. Anything that
    // went wrong says so on stderr, and that is a failure, not an absent token.
    if out.stderr.iter().all(u8::is_ascii_whitespace) {
        Ok(None)
    } else {
        Err(failed(
            TOOL,
            &format!("read the item for {account:?}"),
            &out,
        ))
    }
}

pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
    let label = format!("{service}: {account}");
    let out = run(
        TOOL,
        &[
            "store",
            "--label",
            label.as_str(),
            "service",
            service,
            "account",
            account,
        ],
        // `store` reads the password from stdin, one line, when there is no terminal.
        Some(&format!("{secret}\n")),
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
    // `clear` succeeds whether or not there was anything to clear, so what was
    // there has to be established first.
    let had = get(service, account)?.is_some();
    let out = run(
        TOOL,
        &["clear", "service", service, "account", account],
        None,
    )?;
    if out.status.success() {
        Ok(had)
    } else {
        Err(failed(
            TOOL,
            &format!("remove the item for {account:?}"),
            &out,
        ))
    }
}

pub fn accounts(service: &str) -> Result<Vec<String>> {
    let out = run(TOOL, &["search", "--all", "service", service], None)?;
    if !out.status.success() {
        return Err(failed(TOOL, "list the keychain", &out));
    }
    // Which stream carries the listing has moved between libsecret releases, and
    // reading only one of them would silently lose every token on the way back to
    // the file. Both are read; neither is ever printed.
    let mut listing = String::from_utf8_lossy(&out.stdout).into_owned();
    listing.push('\n');
    listing.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(listed(&listing))
}

fn listed(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.trim().strip_prefix("attribute.account = "))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_lists_accounts_and_no_secrets() {
        let listing = "\
[/org/freedesktop/secrets/collection/login/1]
label = mcpdial: wiki
secret = {\"access_token\":\"nobody should see this\"}
attribute.account = wiki
attribute.service = mcpdial
[/org/freedesktop/secrets/collection/login/2]
label = mcpdial: work
secret = {\"access_token\":\"nor this\"}
attribute.account = work
attribute.service = mcpdial
";
        assert_eq!(listed(listing), ["wiki", "work"]);
        assert!(listed("").is_empty());
    }
}
