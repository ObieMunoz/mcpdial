//! Getting a token, holding one, and giving one back.
//!
//! The browser step happens once: `login` saves what it gets, and every later
//! command reads it. `token` is the same store reached by hand, for a server
//! that mints credentials somewhere this cannot see.

use crate::cmd::{credential_key, Ctx};
use crate::failure::Failure;
use crate::render::{credential_store, expiry_label, print_json, print_value};
use crate::validate::read_secret;
use mcpdial::{client, keychain, oauth, Backend, Error};
use serde_json::json;
use std::time::Duration;

#[derive(clap::Args)]
pub(crate) struct LoginFlags {
    /// A saved name, or an http(s):// URL
    pub(crate) target: String,
    /// authorization-code (a browser, once) or client-credentials (a confidential
    /// client's --client-id and secret, no human)
    #[arg(
        long,
        value_name = "GRANT",
        default_value = "authorization-code",
        value_parser = ["authorization-code", "client-credentials"]
    )]
    pub(crate) grant: String,
    /// Space-separated scopes (default: whatever the server advertises)
    #[arg(long)]
    pub(crate) scope: Option<String>,
    /// Fixed loopback port for the redirect (default: any free port)
    #[arg(long)]
    pub(crate) port: Option<u16>,
    /// Use a pre-registered client id instead of a client metadata document or
    /// dynamic registration
    #[arg(long)]
    pub(crate) client_id: Option<String>,
    /// Present this client ID metadata document as the client id, instead of the
    /// one the project publishes, whether or not the server advertises support
    #[arg(long, value_name = "URL", conflicts_with_all = ["client_id", "no_client_metadata"])]
    pub(crate) client_metadata_url: Option<String>,
    /// Register dynamically even when the server accepts client metadata documents
    #[arg(long)]
    pub(crate) no_client_metadata: bool,
    /// Read that client's secret from stdin. Never from an argument.
    #[arg(long, requires = "client_id")]
    pub(crate) client_secret: bool,
    /// Read that client's secret from $VAR instead of stdin
    #[arg(
        long,
        value_name = "VAR",
        requires = "client_id",
        conflicts_with = "client_secret"
    )]
    pub(crate) client_secret_env: Option<String>,
    /// Loopback host in the redirect URI: 127.0.0.1 (default, with a localhost
    /// fallback if the server refuses it) or localhost
    #[arg(long, value_name = "HOST", value_parser = ["127.0.0.1", "localhost"])]
    pub(crate) redirect_host: Option<String>,
    /// Print the URL but do not try to open a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
}

pub(crate) fn login(cx: &mut Ctx<'_>, flags: LoginFlags) -> Result<u8, Failure> {
    let LoginFlags {
        target,
        grant,
        scope,
        port,
        client_id,
        client_metadata_url,
        no_client_metadata,
        client_secret,
        client_secret_env,
        redirect_host,
        no_browser,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let r = client::resolve(store, &target)?;
    let dialed = r.config.expanded(|var| std::env::var(var).ok())?;
    let Some(url) = dialed.http else {
        return Err(Error::usage(
            "login only applies to HTTP servers; stdio servers need no token",
        )
        .into());
    };
    let client_secret = (client_secret || client_secret_env.is_some())
        .then(|| read_secret(ui, client_secret_env.as_deref(), "client secret"))
        .transpose()?;
    let existing = store.credential(&r.name)?;
    let http = client::oauth_http(opts, &r.name, opts.timeout_for(&r)?);
    let client_metadata = match client_metadata_url {
        Some(url) => oauth::ClientMetadata::Url(url),
        None if no_client_metadata => oauth::ClientMetadata::Never,
        None => oauth::ClientMetadata::IfAdvertised,
    };
    let login_opts = oauth::LoginOptions {
        scope,
        port,
        client_id,
        client_secret,
        client_metadata,
        redirect_host,
        open_browser: !no_browser,
        timeout: Duration::from_secs(300),
    };
    let notify = |line: &str| ui.err_line(line);
    let cred = match grant.as_str() {
        "client-credentials" => oauth::login_client_credentials(&http, &url, &login_opts, notify)?,
        _ => oauth::login(&http, &url, existing.as_ref(), &login_opts, notify)?,
    };
    store.save_credential(&r.name, cred.clone())?;
    let refreshable = cred.can_refresh();
    if json {
        ui.line(&format!(
            "{}",
            json!({ "login": {
                    "name": r.name,
                    "expires_at": cred.expires_at,
                    "refreshable": refreshable,
                    "registration": cred.registration,
                } })
        ));
    } else {
        ui.err_line(&format!(
            "saved token for {} ({}{})",
            r.name,
            expiry_label(&cred),
            if refreshable { ", refreshable" } else { "" }
        ));
    }
    Ok(0)
}

pub(crate) fn logout(cx: &mut Ctx<'_>, target: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let (name, dialable) = credential_key(store, target);
    let removed = store.remove_credential(&name)?;
    // Removing a credential a server never had is the idempotent
    // success it looks like; a name that stands for nothing at all is
    // the typo `rm` already refuses, and is refused here the same way.
    if !removed && !dialable {
        return Err(Error::usage(format!(
            "no server named {name:?} and no credential saved for it"
        ))
        .into());
    }
    if json {
        ui.line(&format!(
            "{}",
            json!({ "removed_credential": removed.then_some(&name) })
        ));
    } else if removed {
        ui.err_line(&format!("removed credential for {name}"));
    } else {
        ui.err_line(&format!("no credential saved for {name}"));
    }
    Ok(0)
}

pub(crate) fn token_set(
    cx: &mut Ctx<'_>,
    name: String,
    env: Option<String>,
) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let (key, _) = credential_key(store, name);
    let token = read_secret(ui, env.as_deref(), "token")?;
    let mut cred = store.credential(&key)?.unwrap_or_default();
    cred.access_token = Some(token);
    cred.expires_at = None;
    cred.source = Some("manual".into());
    store.save_credential(&key, cred)?;
    if json {
        print_value(ui, &json!({ "saved_credential": key }), true);
    } else {
        ui.err_line(&format!("saved token for {key}"));
    }
    Ok(0)
}

pub(crate) fn token_show(cx: &mut Ctx<'_>, name: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let (key, _) = credential_key(store, name);
    let Some(cred) = store.credential(&key)? else {
        return Err(Error::config(format!("no credential saved for {key}")).into());
    };
    // Where it is kept earns its line only once that is not the default,
    // which is also the only time reading it off `token show` tells anyone
    // anything they could not assume.
    let backend = store.backend()?;
    let elsewhere = (backend != Backend::File).then(|| backend.label());
    if json {
        // Metadata only. The secrets never leave the store through this path.
        let mut shown = json!({
            "name": key,
            "has_access_token": cred.has_token(),
            "has_refresh_token": cred.refresh_token.is_some(),
            "expires_at": cred.expires_at,
            "expired": cred.is_expired(),
            "scope": cred.scope,
            "source": cred.source,
            "client_id": cred.client_id,
            "registration": cred.registration,
            "has_client_secret": cred.client_secret.is_some(),
            "issuer": cred.issuer,
            "token_endpoint": cred.token_endpoint,
        });
        if let Some(label) = elsewhere {
            shown["backend"] = json!(label);
        }
        print_json(ui, &shown);
    } else {
        ui.line(&key);
        if let Some(label) = elsewhere {
            ui.line(&format!("  kept in:       {label}"));
        }
        ui.line(&format!(
            "  access token:  {}",
            if cred.has_token() { "present" } else { "none" }
        ));
        ui.line(&format!("  expiry:        {}", expiry_label(&cred)));
        ui.line(&format!(
            "  refresh token: {}",
            if cred.refresh_token.is_some() {
                "present"
            } else {
                "none"
            }
        ));
        ui.line(&format!(
            "  source:        {}",
            cred.source.as_deref().unwrap_or("?")
        ));
        if let Some(s) = &cred.scope {
            ui.line(&format!("  scope:         {s}"));
        }
        if let Some(c) = &cred.client_id {
            ui.line(&format!("  client id:     {c}"));
        }
        if let Some(r) = cred
            .registration
            .as_deref()
            .and_then(oauth::Registration::parse)
        {
            ui.line(&format!("  registered:    {}", r.describe()));
        }
        if cred.client_secret.is_some() {
            ui.line(&format!(
                "  client secret: present ({})",
                cred.token_endpoint_auth_method
                    .as_deref()
                    .unwrap_or(oauth::CLIENT_SECRET_POST)
            ));
        }
        if let Some(i) = &cred.issuer {
            ui.line(&format!("  issuer:        {i}"));
        }
        if let Some(t) = &cred.token_endpoint {
            ui.line(&format!("  token url:     {t}"));
        }
    }
    Ok(0)
}

pub(crate) fn credentials(cx: &mut Ctx<'_>, chosen: Option<String>) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let Some(chosen) = chosen else {
        let (backend, source) = match store.forced_backend() {
            Some(forced) => (forced?, keychain::ENV_BACKEND),
            None => match store.saved_backend()? {
                Some(saved) => (saved, "config.json"),
                None => (Backend::default(), "default"),
            },
        };
        if json {
            print_json(
                ui,
                &json!({ "credentials": backend.label(), "source": source }),
            );
        } else {
            ui.line(&format!("{} ({source})", backend.label()));
        }
        return Ok(0);
    };
    let to = Backend::parse(&chosen)?;
    let moved = store.use_backend(to)?;
    if json {
        print_json(ui, &json!({ "credentials": to.label(), "moved": moved }));
    } else {
        ui.err_line(&match moved {
            None => format!("credentials are already kept in {}", credential_store(to)),
            Some(0) => format!(
                "credentials are kept in {} now; there were none to move",
                credential_store(to)
            ),
            Some(n) => format!(
                "moved {n} credential{} to {}",
                if n == 1 { "" } else { "s" },
                credential_store(to)
            ),
        });
    }
    Ok(0)
}
