//! Saving servers, listing them, and moving them in and out of host configs.
//!
//! What `add` writes is what every later dial reads, so most of this file is
//! about settling a location, a transport and a set of lists before anything is
//! saved rather than after.

use crate::cli::SAVED_NAME_HELP;
use crate::cmd::Ctx;
use crate::diagnose::{closest, NO_SERVERS};
use crate::failure::Failure;
use crate::present::Presenter;
use crate::render::{
    daemon_label, listing_row, print_json, print_value, saved_row, tool_lists, tool_lists_lines,
    LISTING_HEADERS,
};
use crate::validate::{validate_location, validate_patterns, validate_timeout};
use mcpdial::client::Options;
use mcpdial::config::Source;
use mcpdial::registry::{Pick, Registry, Resolved};
use mcpdial::{catalog, client, daemon, Credential, Error, ServerConfig, Store};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(clap::Args)]
pub(crate) struct AddFlags {
    /// The name to save it under, and to dial it by from then on
    pub(crate) name: String,
    /// Streamable HTTP endpoint
    #[arg(
        long,
        value_name = "URL",
        conflicts_with_all = ["stdio", "registry", "catalog"],
        required_unless_present_any = ["stdio", "registry", "catalog"]
    )]
    pub(crate) http: Option<String>,
    /// An entry of the curated catalog, by its id (`mcpdial catalog` lists them)
    #[arg(long, value_name = "ID", conflicts_with_all = ["stdio", "registry"])]
    pub(crate) catalog: Option<String>,
    /// Command that speaks MCP on stdio
    #[arg(long, value_name = "CMD", conflicts_with = "registry")]
    pub(crate) stdio: Option<String>,
    /// A server in the MCP registry, by its registry name (io.github.owner/server)
    #[arg(long, value_name = "NAME")]
    pub(crate) registry: Option<String>,
    /// Run this package type from the registry entry rather than the first offered
    #[arg(
        long,
        value_name = "TYPE",
        value_parser = ["npm", "pypi", "oci"],
        requires = "registry",
        conflicts_with = "remote"
    )]
    pub(crate) package: Option<String>,
    /// Use the registry entry's remote endpoint rather than a package
    #[arg(long, requires = "registry")]
    pub(crate) remote: bool,
    /// A value the registry entry leaves to you, repeatable, in the order listed
    #[arg(long, value_name = "VALUE", requires = "registry")]
    pub(crate) arg: Vec<String>,
    /// Environment variable for the stdio process, repeatable
    #[arg(long, value_name = "KEY=VALUE")]
    pub(crate) env: Vec<String>,
    /// Working directory for the stdio process
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<String>,
    /// Offer only tools matching this glob (`*`, `?`), repeatable
    #[arg(long, value_name = "PATTERN")]
    pub(crate) allow: Vec<String>,
    /// Hide and refuse tools matching this glob, repeatable; beats --allow
    #[arg(long, value_name = "PATTERN")]
    pub(crate) deny: Vec<String>,
    /// Replace a server already saved under this name
    #[arg(long)]
    pub(crate) force: bool,
    /// Save without dialing the server for its status
    #[arg(long)]
    pub(crate) no_probe: bool,
}

#[derive(clap::Args)]
pub(crate) struct SetFlags {
    #[arg(help = SAVED_NAME_HELP)]
    pub(crate) name: String,
    /// Replace the allow list with these globs (`*`, `?`), repeatable
    #[arg(long, value_name = "PATTERN", conflicts_with = "clear_allow")]
    pub(crate) allow: Vec<String>,
    /// Replace the deny list with these globs, repeatable; beats --allow
    #[arg(long, value_name = "PATTERN", conflicts_with = "clear_deny")]
    pub(crate) deny: Vec<String>,
    /// Remove the allow list, so every tool not denied is offered
    #[arg(long)]
    pub(crate) clear_allow: bool,
    /// Remove the deny list
    #[arg(long)]
    pub(crate) clear_deny: bool,
}

#[derive(clap::Args)]
pub(crate) struct ImportFlags {
    /// A host's config: JSON with an `mcpServers`, `servers` (VS Code) or `mcp`
    /// (OpenCode) object, or Codex's `config.toml`. Omit to scan the usual locations.
    pub(crate) file: Option<std::path::PathBuf>,
    /// Scan one host's locations only: vscode, codex, opencode, claude, cursor or windsurf
    #[arg(long, value_name = "HOST", conflicts_with = "file")]
    pub(crate) from: Option<mcpdial::import_config::Host>,
    /// Overwrite servers that already exist under the same name
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(clap::Args)]
pub(crate) struct ExportFlags {
    /// Servers to export; every saved server when none is named
    #[arg(value_name = "NAME")]
    pub(crate) names: Vec<String>,
    /// Shape to write: mcpservers (Claude, Cursor, Windsurf), vscode or codex
    #[arg(long, default_value = "mcpservers", value_name = "FORMAT")]
    pub(crate) format: mcpdial::export_config::Format,
    /// Print this host file with the exported servers merged in; it is never written
    #[arg(long, value_name = "FILE")]
    pub(crate) merge: Option<std::path::PathBuf>,
}

#[derive(clap::Args)]
pub(crate) struct LsFlags {
    /// Do not connect; just show the configuration
    #[arg(long)]
    pub(crate) no_probe: bool,
    /// Dial every server again instead of reusing a status from the last few minutes
    #[arg(long, conflicts_with = "no_probe")]
    pub(crate) refresh: bool,
}

pub(crate) fn add(cx: &mut Ctx<'_>, flags: AddFlags) -> Result<u8, Failure> {
    let AddFlags {
        name,
        http,
        catalog,
        stdio,
        registry,
        package,
        remote,
        arg,
        env,
        cwd,
        allow,
        deny,
        force,
        no_probe,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    // A registry entry is saved without being run, as documented: its
    // command usually needs values the user has yet to supply.
    let dial = !no_probe && registry.is_none();
    let timeout = cx.timeout.map(validate_timeout).transpose()?;
    let mut notes = Vec::new();
    let mut cfg = match (http, stdio, registry) {
        (None, None, None) if catalog.is_some() => {
            let id = catalog.as_deref().unwrap_or("");
            let resolved = from_catalog(ui, store, opts, id)?;
            let elsewhere = mcpdial::config::saved_from_catalog(&store.servers()?, id)
                .filter(|saved| *saved != name && !force)
                .map(String::from);
            if let Some(saved) = elsewhere {
                return Err(Error::usage(format!(
                    "catalog entry {id} is already saved as {saved}; pass --force to save it again"
                ))
                .into());
            }
            notes = resolved.notes;
            resolved.config
        }
        (Some(url), None, None) => ServerConfig::http(url),
        (None, Some(cmd), None) => ServerConfig::stdio(cmd),
        (None, None, Some(entry)) => {
            let pick = if remote {
                Pick::Remote
            } else if let Some(kind) = package {
                Pick::Package(kind)
            } else {
                Pick::Any
            };
            let resolved = from_registry(opts, &entry, &pick, &arg)?;
            notes = resolved.notes;
            resolved.config
        }
        _ => {
            return Err(Error::usage(
                "pass exactly one of --http URL, --stdio CMD or --registry NAME",
            )
            .into())
        }
    };
    if cfg.stdio.is_some() && (!opts.extra_headers.is_empty() || opts.token_env.is_some()) {
        return Err(Error::usage("--header and --token-env only apply to --http servers").into());
    }
    if cfg.http.is_some() && (!env.is_empty() || cwd.is_some()) {
        return Err(Error::usage("--env and --cwd only apply to --stdio servers").into());
    }
    for item in &env {
        match item.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => {
                cfg.env.insert(k.trim().to_string(), v.to_string());
            }
            _ => {
                return Err(
                    Error::usage(format!("--env must look like KEY=VALUE, got {item:?}")).into(),
                )
            }
        }
    }
    if cwd.is_some() {
        cfg.cwd = cwd;
    }
    cfg.headers.extend(opts.extra_headers.iter().cloned());
    cfg.token_env = opts.token_env.clone();
    cfg.protocol_version = opts.protocol_version.map(|v| v.to_string());
    cfg.timeout = timeout;
    cfg.allow = validate_patterns(allow, "--allow")?;
    cfg.deny = validate_patterns(deny, "--deny")?;
    validate_location(&cfg)?;
    let replaced = store.server(&name)?;
    if let Some(old) = replaced.as_ref().filter(|_| !force) {
        return Err(Error::usage(format!(
            "{name} is already saved ({} {}); pass --force to replace it",
            old.kind(),
            old.location()
        ))
        .into());
    }
    let summary = format!("{} {}", cfg.kind(), cfg.location());
    let mut saved = json!({ "name": name, "kind": cfg.kind(), "location": cfg.location() });
    let lists = tool_lists(&cfg);
    store.add_server(&name, cfg)?;
    if force {
        store.forget_probe(&name)?;
    }
    let row = dial
        .then(|| client::listing_one(store, opts, &name))
        .transpose()?;
    if json {
        if let Some(row) = &row {
            saved = serde_json::to_value(row).expect("a listing is serializable");
        }
        if !notes.is_empty() {
            saved["notes"] = json!(notes);
        }
        for (list, patterns) in &lists {
            saved[list] = json!(patterns);
        }
        print_value(ui, &json!({ "saved": saved }), true);
    } else {
        ui.err_line(&match &replaced {
            Some(old) => format!(
                "saved {name} ({summary}), replacing {} {}",
                old.kind(),
                old.location()
            ),
            None => format!("saved {name} ({summary})"),
        });
        for line in tool_lists_lines(&lists) {
            ui.err_line(&format!("  {line}"));
        }
        for note in &notes {
            ui.note(note);
        }
        if let Some(row) = &row {
            ui.table(&LISTING_HEADERS, &[listing_row(row)]);
        }
    }
    Ok(0)
}

pub(crate) fn set(cx: &mut Ctx<'_>, flags: SetFlags) -> Result<u8, Failure> {
    let SetFlags {
        name,
        allow,
        deny,
        clear_allow,
        clear_deny,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let Some(mut cfg) = store.server(&name)? else {
        return Err(Error::usage(format!("no server named {name:?}")).into());
    };
    let changing = !allow.is_empty() || !deny.is_empty() || clear_allow || clear_deny;
    if !changing {
        if json {
            print_json(
                ui,
                &json!({ "name": name, "allow": cfg.allow, "deny": cfg.deny }),
            );
        } else {
            let or = |patterns: &[String], none: &str| {
                if patterns.is_empty() {
                    none.to_string()
                } else {
                    patterns.join(", ")
                }
            };
            ui.line(&name);
            ui.line(&format!(
                "  allow: {}",
                or(&cfg.allow, "(every tool not denied)")
            ));
            ui.line(&format!("  deny:  {}", or(&cfg.deny, "(none)")));
        }
        return Ok(0);
    }
    if clear_allow {
        cfg.allow.clear();
    }
    if clear_deny {
        cfg.deny.clear();
    }
    if !allow.is_empty() {
        cfg.allow = validate_patterns(allow, "--allow")?;
    }
    if !deny.is_empty() {
        cfg.deny = validate_patterns(deny, "--deny")?;
    }
    let summary = format!("{} {}", cfg.kind(), cfg.location());
    let saved = json!({
        "name": name, "kind": cfg.kind(), "location": cfg.location(),
        "allow": cfg.allow, "deny": cfg.deny,
    });
    let lines = tool_lists_lines(&tool_lists(&cfg));
    store.add_server(&name, cfg)?;
    if json {
        print_value(ui, &json!({ "saved": saved }), true);
    } else {
        ui.err_line(&format!("saved {name} ({summary})"));
        for line in lines {
            ui.err_line(&format!("  {line}"));
        }
    }
    Ok(0)
}

pub(crate) fn import(cx: &mut Ctx<'_>, flags: ImportFlags) -> Result<u8, Failure> {
    let ImportFlags { file, from, force } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    let files: Vec<std::path::PathBuf> = match file {
        Some(f) => vec![f],
        None => mcpdial::import_config::candidates(from)
            .into_iter()
            .filter(|p| p.exists())
            .collect(),
    };
    if files.is_empty() {
        return Err(
            Error::usage("no config files found; pass a path to a host's config file").into(),
        );
    }
    let existing = store.servers()?;
    let mut imported: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // The running commentary is for a human; a program gets one object at the end.
    let say = |line: String| {
        if !json {
            ui.err_line(&line);
        }
    };
    for path in &files {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        let found = mcpdial::import_config::read(path, &text)
            .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        if found.is_empty() {
            say(format!("{}: no servers", path.display()));
            continue;
        }
        for f in found {
            if existing.contains_key(&f.name) && !force {
                say(format!(
                    "  skip {:<16} already saved (use --force to overwrite)",
                    f.name
                ));
                skipped.push(f.name);
                continue;
            }
            let summary = format!("{} {}", f.config.kind(), f.config.location());
            match store.add_server(&f.name, f.config) {
                Ok(()) => {
                    say(format!(
                        "  add  {:<16} {summary}  [{} in {}]",
                        f.name,
                        f.scope,
                        path.display()
                    ));
                    for n in &f.notes {
                        say(format!("       note: {n}"));
                    }
                    if !f.notes.is_empty() {
                        notes.insert(f.name.clone(), f.notes);
                    }
                    imported.push(f.name);
                }
                Err(e) => {
                    say(format!("  skip {:<16} {e}", f.name));
                    skipped.push(f.name);
                }
            }
        }
    }
    if json {
        let mut receipt = json!({ "imported": imported, "skipped": skipped });
        if !notes.is_empty() {
            receipt["notes"] = json!(notes);
        }
        print_value(ui, &receipt, true);
    } else {
        ui.err_line(&format!("imported {} server(s)", imported.len()));
    }
    Ok(0)
}

pub(crate) fn export(cx: &mut Ctx<'_>, flags: ExportFlags) -> Result<u8, Failure> {
    let ExportFlags {
        names,
        format,
        merge,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let _out = &cx.out;
    let json = cx.json;
    let out = mcpdial::export_config::export(store, &names, format, merge.as_deref())?;
    ui.out(&out.document);
    for note in out.notes {
        if json {
            ui.err_line(&json!({ "note": note }).to_string());
        } else {
            ui.err_line(&note);
        }
    }
    Ok(0)
}

pub(crate) fn rm(cx: &mut Ctx<'_>, name: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let json = cx.json;
    if store.remove_server(&name)? {
        if json {
            print_value(ui, &json!({ "removed": name }), true);
        } else {
            ui.err_line(&format!("removed {name}"));
        }
        Ok(0)
    } else {
        Err(Error::usage(format!("no server named {name:?}")).into())
    }
}

pub(crate) fn ls(cx: &mut Ctx<'_>, flags: LsFlags) -> Result<u8, Failure> {
    let LsFlags { no_probe, refresh } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let _out = &cx.out;
    let json = cx.json;
    if no_probe {
        let servers = store.servers()?;
        let creds = store.credentials()?;
        if json {
            let rows: Vec<Value> = servers
                .iter()
                .map(|(n, c)| {
                    saved_row(
                        n,
                        c,
                        creds.get(n).is_some_and(Credential::has_token),
                        daemon::is_running(store, n),
                    )
                })
                .collect();
            print_json(ui, &rows);
        } else {
            // A column earns its place only once a server has something for it.
            let with_source = servers.values().any(|c| c.source.is_some());
            let with_timeout = servers.values().any(|c| c.timeout.is_some());
            let running: Vec<&String> = servers
                .keys()
                .filter(|n| daemon::is_running(store, n))
                .collect();
            let mut headers = vec!["NAME", "TYPE", "AUTH"];
            if with_source {
                headers.push("SOURCE");
            }
            if with_timeout {
                headers.push("TIMEOUT");
            }
            if !running.is_empty() {
                headers.push("DAEMON");
            }
            headers.push("LOCATION");
            let rows: Vec<Vec<String>> = servers
                .iter()
                .map(|(n, c)| {
                    let auth = if let Some(var) = &c.token_env {
                        format!("${var}")
                    } else if creds.get(n).is_some_and(Credential::has_token) {
                        "saved".into()
                    } else {
                        "-".into()
                    };
                    let mut row = vec![n.clone(), c.kind().into(), auth];
                    if with_source {
                        row.push(c.source.as_ref().map_or("-", Source::label).into());
                    }
                    if with_timeout {
                        row.push(c.timeout.map_or("-".into(), |t| format!("{t}s")));
                    }
                    if !running.is_empty() {
                        row.push(daemon_label(running.contains(&n)));
                    }
                    row.push(c.location().into());
                    row
                })
                .collect();
            ui.table(&headers, &rows);
        }
        return Ok(0);
    }
    let freshness = if refresh {
        client::Freshness::Live
    } else {
        client::Freshness::Remembered
    };
    let listing = client::listing(store, opts, freshness)?;
    if json {
        let servers = store.servers()?;
        let creds = store.credentials()?;
        let rows: Vec<Value> = listing
            .iter()
            .map(|row| {
                let mut out = match servers.get(&row.name) {
                    Some(cfg) => saved_row(
                        &row.name,
                        cfg,
                        creds.get(&row.name).is_some_and(Credential::has_token),
                        row.running,
                    ),
                    None => json!({}),
                };
                let probed = serde_json::to_value(row).expect("a listing is serializable");
                if let (Some(fields), Value::Object(status)) = (out.as_object_mut(), probed) {
                    fields.extend(status);
                }
                out
            })
            .collect();
        print_json(ui, &rows);
    } else if listing.is_empty() {
        ui.err_line(NO_SERVERS);
    } else {
        let rows: Vec<Vec<String>> = listing.iter().map(listing_row).collect();
        ui.table(&LISTING_HEADERS, &rows);
    }
    Ok(0)
}

/// The config a registry entry describes, with every value it needs in hand.
/// Nothing is run: the command line is built, not tried.
pub(crate) fn from_registry(
    opts: &Options,
    entry: &str,
    pick: &Pick,
    args: &[String],
) -> Result<Resolved, Failure> {
    const NAME_HINT: &str =
        "a registry name is the entry's own `name`, like io.github.owner/server";
    if !entry.contains('/') {
        return Err(Failure::hinted(
            Error::usage(format!("{entry:?} is not a registry name")),
            NAME_HINT,
        ));
    }
    let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
    let server = registry.latest(entry)?.ok_or_else(|| {
        Failure::hinted(
            Error::usage(format!("the registry has no server named {entry}")),
            NAME_HINT,
        )
    })?;
    let resolved = mcpdial::registry::convert(&server, pick, args)?;
    if !resolved.missing.is_empty() {
        return Err(Failure::hinted(
            Error::usage(format!(
                "{entry} needs {} value(s) this command was not given",
                resolved.missing.len()
            )),
            format!(
                "pass each with --arg VALUE, in this order:\n  {}",
                resolved.missing.join("\n  ")
            ),
        ));
    }
    Ok(resolved)
}

/// The config a catalog entry describes, from the freshest catalog at hand. A
/// registry entry goes through the registry exactly as `--registry` would.
pub(crate) fn from_catalog(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    id: &str,
) -> Result<Resolved, Failure> {
    let loaded = catalog::load(
        store,
        &catalog::Source::from_env(),
        false,
        opts.timeout_or_default(),
        &opts.user_agent,
    )?;
    if opts.verbose {
        ui.err_line(&format!("catalog: {}", loaded.origin));
    }
    let Some(entry) = catalog::find(&loaded.entries, id) else {
        let ids = loaded.entries.iter().map(|e| e.id.as_str());
        return Err(Failure::hinted(
            Error::usage(format!("no catalog entry named {id:?}")),
            match closest(id, ids) {
                Some(near) => format!("did you mean {near}? `mcpdial catalog` lists them all."),
                None => "`mcpdial catalog` lists every entry with its id.".to_string(),
            },
        ));
    };
    let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
    let resolved = catalog::resolve(entry, &registry)?;
    if !resolved.missing.is_empty() {
        return Err(Failure::hinted(
            Error::config(format!(
                "{id} needs {} value(s) the catalog does not carry",
                resolved.missing.len()
            )),
            format!(
                "add it from the registry instead, passing each with --arg VALUE in this order:\n  mcpdial add NAME --registry {} --arg ...\n  {}",
                entry.registry.as_deref().unwrap_or("?"),
                resolved.missing.join("\n  ")
            ),
        ));
    }
    Ok(resolved)
}
