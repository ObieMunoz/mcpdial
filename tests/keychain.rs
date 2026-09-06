//! A round trip through the machine's own keychain, opt in.
//!
//! Every other test of the keychain backend runs against an in-process stand-in, so
//! `cargo test` never reaches the account's real secrets. This one does, which is why
//! it does nothing unless asked:
//!
//!     MCPDIAL_TEST_KEYCHAIN=1 cargo test --test keychain
//!
//! CI does not ask. A hosted runner has no unlocked keychain worth the name, and on
//! macOS the first item raises a dialog with nobody there to answer it.

use mcpdial::keychain;

/// A name nothing else uses, so a run that dies part way through leaves one obvious
/// item behind rather than something that looks like a saved server.
const ACCOUNT: &str = "mcpdial-test-round-trip";

fn opted_in() -> bool {
    std::env::var("MCPDIAL_TEST_KEYCHAIN").is_ok_and(|asked| asked == "1")
}

#[test]
fn a_credential_round_trips_through_the_real_keychain() {
    if !opted_in() {
        eprintln!("set MCPDIAL_TEST_KEYCHAIN=1 to run this against the real keychain");
        return;
    }
    keychain::check().expect("a keychain to talk to");
    let _ = keychain::delete(ACCOUNT);

    let saved = r#"{"access_token":"round-trip","issuer":"https://as.example"}"#;
    keychain::set(ACCOUNT, saved).unwrap();
    assert_eq!(keychain::get(ACCOUNT).unwrap().as_deref(), Some(saved));
    assert!(
        keychain::accounts().unwrap().iter().any(|a| a == ACCOUNT),
        "a stored item has to be findable again to be moved back out"
    );

    // What a second login does: replace the item rather than collide with it.
    let again = r#"{"access_token":"round-trip-2","scope":"café"}"#;
    keychain::set(ACCOUNT, again).unwrap();
    assert_eq!(keychain::get(ACCOUNT).unwrap().as_deref(), Some(again));

    assert!(keychain::delete(ACCOUNT).unwrap());
    assert!(!keychain::delete(ACCOUNT).unwrap());
    assert!(keychain::get(ACCOUNT).unwrap().is_none());
}
