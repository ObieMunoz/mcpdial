use crate::diagnose::{
    call_hint, closest, find_tool, is_argument_error, json_arg_hint, missing_capability,
    missing_item, reads_as_argument_error, server_refused, shell_word, NO_SERVERS,
};
use crate::media::{emit, emit_resource, file_stem, rendered, resource_stem, MediaFiles};
use crate::validate::{
    parse_headers, read_json_arg, read_secret, validate_location, validate_patterns,
    validate_timeout,
};
use clap::{CommandFactory, Parser};
use cli::{Cli, Cmd, Completing, ConfigCmd, TokenCmd};
use mcpdial::catalog;
use mcpdial::client::{self, describe_params, Listing, Options, Status};
use mcpdial::config::Source;
use mcpdial::registry::{Pick, Registry, Resolved};
use mcpdial::serve;
use mcpdial::session::{render_content, render_messages};
use mcpdial::transport::trace::Trace;
use mcpdial::{
    daemon, keychain, oauth, Backend, Credential, Elicit, Error, ServerConfig, Store, USER_AGENT,
};
use notices::Notices;
use output::{As, Output, Payload};
use present::{truncate_at, Presenter};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

mod args;
mod brief;
mod browse;
mod cli;
mod diagnose;
mod env_defaults;
mod grep;
mod media;
mod notices;
mod output;
mod path;
mod pick;
mod present;
mod prompt;
mod shell;
mod snapshot;
mod tasks;
mod validate;

const EXIT_ERROR: u8 = 1; // the server said no: JSON-RPC error, HTTP error, or tool isError
const EXIT_USAGE: u8 = 2; // bad arguments or config; nothing was sent
const EXIT_DRIFT: u8 = 3; // --check: the server no longer matches the snapshot

fn main() -> ExitCode {
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            return bare_mcpdial(e)
        }
        Err(e) => e.exit(),
    };
    let defaults = env_defaults::apply(&mut cli);
    let ui = <dyn Presenter>::choose(&cli);
    let json = cli.json;
    match defaults
        .map_err(Failure::from)
        .and_then(|()| run(ui.as_ref(), cli))
    {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            if json {
                ui.err_line(&f.to_json().to_string());
            } else {
                f.report(ui.as_ref());
            }
            ExitCode::from(f.exit_code())
        }
    }
}

/// A bare `mcpdial`: [`pick::welcome`] for a person, and clap's usage error for
/// every pipe, program, `--json` and `--plain`, exactly as it has always been.
/// Never the picker and never a dial, whatever is saved - `mcpdial pick` is
/// where choosing a server and calling one of its tools lives, and asking for
/// it is what starts it.
fn bare_mcpdial(e: clap::Error) -> ExitCode {
    // Somewhere to read the global flags from. The argv behind this error
    // carries none of them - a flag without a subcommand is a different clap
    // error, which never reaches here - so the environment is the whole of
    // what there is to find, and MCPDIAL_JSON in it is a program asking.
    let mut cli = Cli::parse_from(["mcpdial", "pick"]);
    let _ = env_defaults::apply(&mut cli);
    if !pick::at_a_terminal(&cli) {
        e.exit();
    }
    let ui = <dyn Presenter>::choose(&cli);
    match Store::from_env()
        .map_err(Failure::from)
        .and_then(|store| pick::welcome(ui.as_ref(), &store))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            f.report(ui.as_ref());
            ExitCode::from(f.exit_code())
        }
    }
}

/// A command that failed, plus an optional hint that spells out what was
/// expected instead. The hint is a second block of prose for a human and an
/// `error.hint` string under `--json`, so neither has to guess a tool's shape.
struct Failure {
    error: Error,
    hint: Option<String>,
    /// The tool the error is about, as `error.tool` under `--json`.
    tool: Option<String>,
}

impl Failure {
    fn hinted(error: Error, hint: impl Into<String>) -> Self {
        Self {
            error,
            hint: Some(hint.into()),
            tool: None,
        }
    }

    /// What the process exits with: a request that was never going to work is
    /// told apart from one that failed on its way out.
    fn exit_code(&self) -> u8 {
        match self.error {
            Error::Usage(_) | Error::Config(_) => EXIT_USAGE,
            _ => EXIT_ERROR,
        }
    }

    fn report(&self, ui: &dyn Presenter) {
        ui.error(&self.error.to_string(), self.hint.as_deref());
    }

    fn to_json(&self) -> Value {
        let mut v = error_json(&self.error);
        if let Some(hint) = &self.hint {
            v["error"]["hint"] = json!(hint);
        }
        if let Some(tool) = &self.tool {
            v["error"]["tool"] = json!(tool);
        }
        v
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            hint: None,
            tool: None,
        }
    }
}

/// The refusal a saved server's allow and deny lists make before anything is
/// sent for `tool`; `Ok` when they permit it.
fn refuse_denied(cfg: &ServerConfig, name: &str, tool: &str) -> Result<(), Failure> {
    cfg.refuse_denied(name, tool).map_err(|error| Failure {
        error,
        hint: None,
        tool: Some(tool.to_string()),
    })
}

fn print_json(ui: &dyn Presenter, v: &impl serde::Serialize) {
    ui.json(&serde_json::to_string_pretty(v).expect("a JSON value is serializable"));
}

fn print_value(ui: &dyn Presenter, v: &Value, compact: bool) {
    ui.json(&output::document(v, compact));
}

/// A note on stderr: prose for a human, `{"note": ...}` under `--json`, where
/// every line on either stream has to be an object.
fn print_note(ui: &dyn Presenter, note: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "note": note }).to_string());
    } else {
        ui.note(note);
    }
}

/// A hint on stderr: prose for a human, `{"hint": ...}` under `--json`, where
/// every line on either stream has to be an object.
fn print_hint(ui: &dyn Presenter, hint: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "hint": hint }).to_string());
    } else {
        ui.err_line(hint);
    }
}

/// A tool result on stdout, and a marker on stderr when the tool reported an
/// error: a failure whose text is a plain sentence otherwise reads as success
/// to anyone not checking `$?`. Under `--json` the object carries `isError`
/// itself. Returns whether the tool reported an error.
fn print_tool_result(
    ui: &dyn Presenter,
    out: &Output,
    result: &Value,
    text: &str,
    json: bool,
    one_line: bool,
) -> Result<bool, Failure> {
    let failed = result["isError"].as_bool().unwrap_or(false);
    let payload = if json {
        Payload::Json {
            value: result,
            one_line,
        }
    } else {
        Payload::Text(text)
    };
    let sent = out.deliver(payload, failed)?;
    output::show(ui, sent, shape(json), json, || {
        if json {
            print_value(ui, result, one_line);
        } else if !text.is_empty() {
            ui.text(text);
        }
    });
    if !json && failed {
        ui.err_line("(tool reported an error)");
    }
    Ok(failed)
}

/// A `tools/call` result on stdout, however it was fetched: the one the call
/// waited for, and the one a finished task was holding. Returns whether the
/// tool reported an error.
fn printed_result(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    save_dir: Option<&Path>,
    stem: &str,
) -> Result<(bool, String), Failure> {
    let files = MediaFiles {
        dir: save_dir,
        stem: file_stem(stem),
    };
    let text = rendered(ui, result, json, &files, render_content)?;
    let failed = ui.paged(|| print_tool_result(ui, out, result, &text, json, false))?;
    Ok((failed, text))
}

/// A result goes out as a document under `--json` and as the server's own text
/// otherwise, cut or whole.
fn shape(json: bool) -> As {
    if json {
        As::Json
    } else {
        As::Text
    }
}

/// What this invocation can answer when a server asks for one more fact.
///
/// Nothing here may wait on a person who is not there, and whether one is there
/// is a question already answered: [`Presenter::asks`] is what says whether this
/// run is a person's or a program's, and a missing argument and an elicitation
/// are the same question put twice. [`Presenter::watched`] is asked as well,
/// because an elicitation writes its question on stderr above the prompts, and
/// prose nobody will read is not worth stalling a call for; `--json` says so
/// twice over, since it also decides what the report on stderr looks like.
fn elicitation(
    ui: &dyn Presenter,
    answers: Option<&str>,
    no_browser: bool,
    json: bool,
    shell: bool,
) -> Result<Elicit, Error> {
    let answers = answers
        .map(|text| read_json_arg(text, "elicit answers"))
        .transpose()?
        .and_then(|v| v.as_object().cloned());
    Ok(Elicit {
        answers,
        ask: ui.asks() && ui.watched() && !json,
        later: shell,
        browser: !no_browser,
        json,
    })
}

fn dial(store: &Store, opts: &Options, target: &str) -> Result<client::Connection, Failure> {
    let r = client::resolve(store, target)?;
    Ok(client::connect(store, &r, opts)?)
}

fn info_hint(target: &str) -> String {
    format!("`mcpdial info {}`", shell_word(target))
}

/// Where to look for the resource or prompt the server did have.
fn resources_hint(target: &str) -> String {
    format!("`mcpdial resources {}`", shell_word(target))
}

fn prompts_hint(target: &str) -> String {
    format!("`mcpdial prompts {} --long`", shell_word(target))
}

/// The config a registry entry describes, with every value it needs in hand.
/// Nothing is run: the command line is built, not tried.
fn from_registry(
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
fn from_catalog(
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

/// The name a credential is filed under, and whether the target names anything
/// mcpdial could dial: a saved server, a URL, or a `stdio:` command line.
fn credential_key(store: &Store, target: String) -> (String, bool) {
    match client::resolve(store, &target) {
        Ok(r) => (r.name, true),
        Err(_) => (target, false),
    }
}

fn name_and_args(rest: &str) -> (&str, &str) {
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
    match args.trim() {
        "" => (name, "{}"),
        args => (name, args),
    }
}

/// One JSON object per error, so a program can branch on `kind` without parsing prose.
fn error_json(e: &Error) -> Value {
    let mut v = json!({ "message": e.to_string() });
    match e {
        Error::Rpc { code, data, .. } => {
            v["kind"] = json!("rpc");
            v["code"] = json!(code);
            if let Some(d) = data {
                v["data"] = d.clone();
            }
        }
        Error::Http {
            status,
            www_authenticate,
            ..
        } => {
            v["kind"] = json!("http");
            v["status"] = json!(status);
            if let Some(w) = www_authenticate {
                v["www_authenticate"] = json!(w);
            }
        }
        Error::Transport(_) => v["kind"] = json!("transport"),
        Error::Auth(_) => v["kind"] = json!("auth"),
        Error::Config(_) => v["kind"] = json!("config"),
        Error::Usage(_) => v["kind"] = json!("usage"),
    }
    json!({ "error": v })
}

/// The allow and deny lists a server has, under the keys they are saved as.
fn tool_lists(cfg: &ServerConfig) -> Vec<(&'static str, Vec<String>)> {
    [("allow", &cfg.allow), ("deny", &cfg.deny)]
        .into_iter()
        .filter(|(_, patterns)| !patterns.is_empty())
        .map(|(list, patterns)| (list, patterns.clone()))
        .collect()
}

/// One line per list that is set, for the receipt a human reads.
fn tool_lists_lines(lists: &[(&str, Vec<String>)]) -> Vec<String> {
    lists
        .iter()
        .map(|(list, patterns)| format!("{list}: {}", patterns.join(", ")))
        .collect()
}

fn run(ui: &dyn Presenter, cli: Cli) -> Result<u8, Failure> {
    let store = Store::from_env()?;
    let mut opts = Options {
        timeout: cli
            .timeout
            .map(|secs| Duration::from_secs_f64(secs.max(0.0))),
        user_agent: cli
            .user_agent
            .clone()
            .unwrap_or_else(|| USER_AGENT.to_string()),
        extra_headers: parse_headers(&cli.headers)?,
        token_env: cli.token_env.clone(),
        protocol_version: cli.protocol_version,
        verbose: cli.verbose,
        no_daemon: cli.no_daemon || daemon::disabled_by_env(),
        retry: !cli.no_retry,
        log_level: cli.log_level,
        trace: Trace::from_flag_or_env(cli.trace.as_deref())?,
        ..Options::default()
    };
    // Built before the command is matched, because matching moves it out of
    // `cli`; it borrows nothing from there.
    let mut notices = Notices::new(ui, &cli);
    if let Some(dir) = &cli.save_dir {
        let renders_media = matches!(
            cli.cmd,
            Cmd::Call { .. }
                | Cmd::Prompt { .. }
                | Cmd::Read { .. }
                | Cmd::Shell { .. }
                | Cmd::Tasks { .. }
        );
        if !renders_media {
            return Err(
                Error::usage("--save-dir applies to call, prompt, read, shell and tasks").into(),
            );
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::usage(format!("--save-dir {}: {e}", dir.display())))?;
    }
    let save_dir = cli.save_dir.as_deref();
    let out = Output::choose(&cli)?;
    let plain = present::wants_plain(&cli);

    match cli.cmd {
        Cmd::Add {
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
        } => {
            // A registry entry is saved without being run, as documented: its
            // command usually needs values the user has yet to supply.
            let dial = !no_probe && registry.is_none();
            let timeout = cli.timeout.map(validate_timeout).transpose()?;
            let mut notes = Vec::new();
            let mut cfg = match (http, stdio, registry) {
                (None, None, None) if catalog.is_some() => {
                    let id = catalog.as_deref().unwrap_or("");
                    let resolved = from_catalog(ui, &store, &opts, id)?;
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
                    let resolved = from_registry(&opts, &entry, &pick, &arg)?;
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
                return Err(
                    Error::usage("--header and --token-env only apply to --http servers").into(),
                );
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
                        return Err(Error::usage(format!(
                            "--env must look like KEY=VALUE, got {item:?}"
                        ))
                        .into())
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
                .then(|| client::listing_one(&store, &opts, &name))
                .transpose()?;
            if cli.json {
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

        Cmd::Set {
            name,
            allow,
            deny,
            clear_allow,
            clear_deny,
        } => {
            let Some(mut cfg) = store.server(&name)? else {
                return Err(Error::usage(format!("no server named {name:?}")).into());
            };
            let changing = !allow.is_empty() || !deny.is_empty() || clear_allow || clear_deny;
            if !changing {
                if cli.json {
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
            if cli.json {
                print_value(ui, &json!({ "saved": saved }), true);
            } else {
                ui.err_line(&format!("saved {name} ({summary})"));
                for line in lines {
                    ui.err_line(&format!("  {line}"));
                }
            }
            Ok(0)
        }

        Cmd::Search {
            query,
            limit,
            refresh,
            offline,
        } => {
            let query = query.join(" ");
            if query.trim().is_empty() {
                return Err(Failure::hinted(
                    Error::usage("search needs a query"),
                    "words that must all appear, like: mcpdial search browser automation",
                ));
            }
            let sync = match (refresh, offline) {
                (true, _) => mcpdial::registry::Sync::Refresh,
                (_, true) => mcpdial::registry::Sync::Offline,
                _ => mcpdial::registry::Sync::Auto,
            };
            let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
            let mut progress =
                |n: usize| ui.progress(&format!("fetching the registry: {n} servers"));
            let indexed = mcpdial::registry::index(&store, &registry, sync, &mut progress);
            ui.progress_end();
            let (index, note) = indexed?;
            if let Some(note) = note {
                ui.note(&note);
            }
            let loaded = catalog::load(
                &store,
                &catalog::Source::from_env(),
                offline,
                opts.timeout_or_default(),
                &opts.user_agent,
            )?;
            if opts.verbose {
                ui.err_line(&format!("catalog: {}", loaded.origin));
            }
            let catalog: Vec<String> = loaded
                .entries
                .iter()
                .filter_map(|e| e.registry.clone())
                .collect();
            let hits = mcpdial::search::rank(&query, &index.servers, &catalog);
            if hits.is_empty() {
                if cli.json {
                    print_json(ui, &hits);
                } else {
                    ui.err_line(&format!("no registry entry matches {query:?}"));
                }
                return Ok(EXIT_ERROR);
            }
            let shown: Vec<&Value> = hits.iter().copied().take(limit).collect();
            if cli.json {
                print_json(ui, &shown);
                return Ok(0);
            }
            let rows: Vec<Vec<String>> = shown
                .iter()
                .map(|e| {
                    let server = &e["server"];
                    let name = server["name"].as_str().unwrap_or("?");
                    let transports = match mcpdial::registry::transports(server) {
                        t if t.is_empty() => "-".to_string(),
                        t => t.join(", "),
                    };
                    let source = if catalog.iter().any(|c| c == name) {
                        "catalog"
                    } else {
                        "registry"
                    };
                    vec![
                        name.to_string(),
                        transports,
                        source.to_string(),
                        truncate(server["description"].as_str().unwrap_or("")),
                    ]
                })
                .collect();
            ui.table(&["NAME", "TRANSPORTS", "SOURCE", "DESCRIPTION"], &rows);
            if hits.len() > shown.len() {
                ui.err_line(&format!(
                    "{} of {} matches; --limit N shows more",
                    shown.len(),
                    hits.len()
                ));
            }
            Ok(0)
        }

        Cmd::Import { file, from, force } => {
            let files: Vec<std::path::PathBuf> = match file {
                Some(f) => vec![f],
                None => mcpdial::import_config::candidates(from)
                    .into_iter()
                    .filter(|p| p.exists())
                    .collect(),
            };
            if files.is_empty() {
                return Err(Error::usage(
                    "no config files found; pass a path to a host's config file",
                )
                .into());
            }
            let existing = store.servers()?;
            let mut imported: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // The running commentary is for a human; a program gets one object at the end.
            let say = |line: String| {
                if !cli.json {
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
            if cli.json {
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

        Cmd::Export {
            names,
            format,
            merge,
        } => {
            let out = mcpdial::export_config::export(&store, &names, format, merge.as_deref())?;
            ui.out(&out.document);
            for note in out.notes {
                if cli.json {
                    ui.err_line(&json!({ "note": note }).to_string());
                } else {
                    ui.err_line(&note);
                }
            }
            Ok(0)
        }

        Cmd::Shell { target, no_browser } => shell::run(
            ui,
            &store,
            &mut opts,
            &out,
            &mut notices,
            save_dir,
            cli.json,
            target,
            no_browser,
        ),

        Cmd::Start { name, idle } => {
            let idle = idle_duration(idle)?;
            let pid = daemon::start(&store, &name, idle, &opts)?;
            if cli.json {
                print_json(
                    ui,
                    &json!({
                        "name": name, "pid": pid,
                        "socket": daemon::socket_path(&store, &name),
                    }),
                );
            } else {
                ui.err_line(&format!("started {name} (pid {pid})"));
            }
            Ok(0)
        }

        Cmd::Stop { name } => {
            daemon::stop(&store, &name, &opts)?;
            ui.err_line(&format!("stopped {name}"));
            Ok(0)
        }

        // Only `start` runs this, with nothing but a pipe back to it on stdout:
        // an error before the socket is open is reported there, for `start` to
        // print as its own.
        Cmd::Daemon { name, idle } => {
            let idle = idle_duration(idle)?;
            match daemon::serve(&store, &name, &opts, idle) {
                Ok(()) => Ok(0),
                Err(e) => {
                    print_value(ui, &error_json(&e), true);
                    Ok(EXIT_ERROR)
                }
            }
        }

        Cmd::Schema { target, tool } => {
            let r = client::resolve(&store, &target)?;
            refuse_denied(&r.config, &r.name, &tool)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let tools = conn.list_tools()?;
            // The `tools/list` this name was looked up in succeeded, and nothing
            // was ever sent for the tool itself: the mistake is the caller's, and
            // saying so as the shell's own `schema` does costs no invented -32602.
            let Some(t) = find_tool(&tools, &tool) else {
                let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
                return Err(Failure {
                    error: Error::usage(format!(
                        "no tool named {tool:?}; available: {}",
                        names.join(", ")
                    )),
                    hint: closest(&tool, names.iter().copied())
                        .map(|near| format!("did you mean {near}?")),
                    tool: None,
                });
            };
            // Beside the note below, and on stderr for the same reason: stdout
            // is the tool object, whole, whichever mode asked for it.
            let hints = client::hint_tags(t);
            if !hints.is_empty() {
                ui.err_line(&format!("{tool}{hints}"));
            }
            ui.paged(|| print_json(ui, t));
            if t.get("outputSchema").is_some() {
                ui.err_line("this tool declares an outputSchema: results carry structuredContent");
            }
            Ok(0)
        }

        Cmd::Resources { target, long } => {
            let mut conn = dial(&store, &opts, &target)?;
            let resources = conn
                .session
                .list_resources()
                .map_err(|e| missing_capability(e, "resources", &info_hint(&target)))?;
            // Templates are optional even where resources are not, so a server with
            // none of them must not turn the whole listing into an error.
            let templates = conn.session.list_resource_templates().unwrap_or_default();
            if cli.json {
                print_json(
                    ui,
                    &json!({
                        "resources": brief::resources(&resources, long),
                        "resourceTemplates": brief::resources(&templates, long),
                    }),
                );
            } else {
                ui.line(&format!("{} resource(s):\n", resources.len()));
                ui.resources(&resources, long);
                if !templates.is_empty() {
                    ui.line(&format!("\n{} template(s):\n", templates.len()));
                    ui.resources(&templates, long);
                }
            }
            Ok(0)
        }

        Cmd::Read { target, uri } => {
            let mut conn = dial(&store, &opts, &target)?;
            let outcome = conn.session.read_resource_watching(&uri, &mut notices);
            notices.finish();
            let mut result = outcome.map_err(|e| {
                missing_item(
                    e,
                    "resources",
                    &resources_hint(&target),
                    &info_hint(&target),
                )
            })?;
            let redirect = format!("mcpdial read {} {}", shell_word(&target), shell_word(&uri));
            let files = MediaFiles {
                dir: save_dir,
                stem: resource_stem(&uri),
            };
            ui.paged(|| emit_resource(ui, &out, &mut result, cli.json, false, &files, &redirect))?;
            Ok(0)
        }

        Cmd::Prompts { target, long } => {
            let mut conn = dial(&store, &opts, &target)?;
            let prompts = conn
                .session
                .list_prompts()
                .map_err(|e| missing_capability(e, "prompts", &info_hint(&target)))?;
            if cli.json {
                print_json(ui, &json!({ "prompts": brief::prompts(&prompts, long) }));
            } else {
                ui.line(&format!("{} prompt(s):\n", prompts.len()));
                print_prompts(ui, &prompts, long);
            }
            Ok(0)
        }

        Cmd::Complete { target, of } => {
            let (reference, name, value, context) = match of {
                Completing::Prompt {
                    name,
                    argument,
                    value,
                    context,
                } => (
                    json!({ "type": "ref/prompt", "name": name }),
                    argument,
                    value,
                    context,
                ),
                Completing::Resource {
                    template,
                    variable,
                    value,
                    context,
                } => (
                    json!({ "type": "ref/resource", "uri": template }),
                    variable,
                    value,
                    context,
                ),
            };
            let context = context
                .map(|text| read_json_arg(&text, "context"))
                .transpose()?
                .map(|arguments| json!({ "arguments": arguments }));
            let mut conn = dial(&store, &opts, &target)?;
            let found = conn
                .session
                .complete(reference, json!({ "name": name, "value": value }), context)
                .map_err(|e| missing_capability(e, "completions", &info_hint(&target)))?;
            if cli.json {
                print_json(
                    ui,
                    &json!({ "values": found.values, "hasMore": found.has_more }),
                );
            } else {
                for value in &found.values {
                    ui.line(value);
                }
            }
            if found.has_more {
                ui.err_line(&format!(
                    "{} value(s); the server says it has more",
                    found.values.len()
                ));
            }
            Ok(0)
        }

        Cmd::Prompt {
            target,
            name,
            arguments,
            elicit,
            no_browser,
        } => {
            opts.elicit = elicitation(ui, elicit.as_deref(), no_browser, cli.json, false)?;
            let form = args::form(&arguments)?;
            let arguments = match form.json() {
                Some(text) => read_json_arg(text, "arguments").map_err(|e| {
                    Failure::hinted(
                        e,
                        json_arg_hint(
                            text,
                            &format!("mcpdial prompt {} {name}", shell_word(&target)),
                            format!(
                                "`mcpdial prompts {} --long` shows what {name} takes",
                                shell_word(&target)
                            ),
                        ),
                    )
                })?,
                // A prompt's arguments carry no schema: every one is a string.
                None => args::parse_pairs(form.pairs(), &Value::Null)?,
            };
            let mut conn = dial(&store, &opts, &target)?;
            let outcome = conn
                .session
                .get_prompt_watching(&name, arguments, &mut notices);
            notices.finish();
            let mut result = outcome.map_err(|e| {
                missing_item(e, "prompts", &prompts_hint(&target), &info_hint(&target))
            })?;
            if !cli.json {
                if let Some(d) = result["description"].as_str() {
                    ui.err_line(d);
                }
            }
            let files = MediaFiles {
                dir: save_dir,
                stem: file_stem(&name),
            };
            ui.paged(|| {
                emit(
                    ui,
                    &out,
                    &mut result,
                    cli.json,
                    false,
                    &files,
                    render_messages,
                )
            })?;
            Ok(0)
        }

        Cmd::Guide => {
            ui.out(include_str!("../docs/AGENTS.md"));
            Ok(0)
        }

        Cmd::Catalog { offline } => {
            let loaded = catalog::load(
                &store,
                &catalog::Source::from_env(),
                offline,
                opts.timeout_or_default(),
                &opts.user_agent,
            )?;
            if opts.verbose {
                ui.err_line(&format!("catalog: {}", loaded.origin));
            }
            if cli.json {
                print_json(ui, &loaded.entries);
            } else {
                ui.catalog(&loaded.entries);
                ui.err_line("\nadd one with: mcpdial add NAME --catalog ID");
            }
            Ok(0)
        }

        Cmd::Browse {
            all,
            offline,
            preview,
        } => browse::run(
            ui,
            &store,
            &opts,
            browse::Flags {
                all,
                offline,
                preview,
                interactive: !plain && std::io::stdin().is_terminal(),
            },
        ),

        Cmd::Pick => {
            opts.elicit = elicitation(ui, None, false, cli.json, false)?;
            pick::run(ui, &store, &opts, &out, &mut notices, save_dir)
        }

        Cmd::Completions { shell } => {
            // Building the whole command tree to walk it recurses deeper than
            // the 1 MB stack a Windows main thread is given, and every global
            // flag added takes it deeper still, so the walk gets a stack of its
            // own rather than being one flag away from overflowing.
            const SCRIPT_STACK: usize = 8 * 1024 * 1024;
            let failed =
                |what: &str| Error::transport(format!("writing the {shell} script: {what}"));
            std::thread::Builder::new()
                .stack_size(SCRIPT_STACK)
                .spawn(move || {
                    let mut command = Cli::command();
                    let name = command.get_name().to_string();
                    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
                })
                .map_err(|e| failed(&e.to_string()))?
                .join()
                .map_err(|_| failed("the writer stopped"))?;
            Ok(0)
        }

        Cmd::Rm { name } => {
            if store.remove_server(&name)? {
                if cli.json {
                    print_value(ui, &json!({ "removed": name }), true);
                } else {
                    ui.err_line(&format!("removed {name}"));
                }
                Ok(0)
            } else {
                Err(Error::usage(format!("no server named {name:?}")).into())
            }
        }

        Cmd::Ls { no_probe, refresh } => {
            if no_probe {
                let servers = store.servers()?;
                let creds = store.credentials()?;
                if cli.json {
                    let rows: Vec<Value> = servers
                        .iter()
                        .map(|(n, c)| {
                            saved_row(
                                n,
                                c,
                                creds.get(n).is_some_and(Credential::has_token),
                                daemon::is_running(&store, n),
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
                        .filter(|n| daemon::is_running(&store, n))
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
            let listing = client::listing(&store, &opts, freshness)?;
            if cli.json {
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
                        if let (Some(fields), Value::Object(status)) = (out.as_object_mut(), probed)
                        {
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

        Cmd::Tools {
            target: None, long, ..
        } => {
            let mut probes = client::probe_all(&store, &opts, true)?;
            if cli.json {
                for p in &mut probes {
                    p.tools = p.tools.take().map(|t| brief::tools(&t, long));
                }
                print_json(ui, &Servers { servers: &probes });
                return Ok(0);
            }
            if probes.is_empty() {
                ui.err_line(NO_SERVERS);
                return Ok(0);
            }
            listed(ui, long, || {
                for (i, p) in probes.iter().enumerate() {
                    if i > 0 {
                        ui.line("");
                    }
                    match &p.tools {
                        Some(tools) => {
                            ui.line(&format!(
                                "## {}  {}  ({} tools)",
                                p.name,
                                p.server.as_deref().unwrap_or(""),
                                tools.len()
                            ));
                            print_tools(ui, tools, long);
                        }
                        None => ui.line(&format!(
                            "## {}  {}{}",
                            p.name,
                            p.status.label(),
                            p.status
                                .detail()
                                .map(|d| format!(": {}", truncate(d)))
                                .unwrap_or_default()
                        )),
                    }
                }
            });
            Ok(0)
        }

        Cmd::Tools {
            target: Some(target),
            long,
            all,
            snapshot: to_file,
            check,
            strict,
        } => {
            // Both files are answered before anything is dialed: a path that
            // could never be written, and a snapshot that is not one, cost no
            // connection.
            if let Some(path) = &to_file {
                snapshot::reserve(path)?;
            }
            let promised = check.as_deref().map(snapshot::read).transpose()?;
            let mut conn = dial(&store, &opts, &target)?;
            let tools = if all {
                conn.list_all_tools()?
            } else {
                conn.list_tools()?
            };
            if let Some(path) = to_file {
                snapshot::write(&path, &snapshot::document(&conn.server_info, &tools))?;
                snapshot::wrote(ui, cli.json, &path, tools.len());
                return Ok(0);
            }
            if let Some(promised) = promised {
                let comparison = snapshot::compare(&promised, &tools, strict);
                comparison.report(ui, cli.json, snapshot::Report::Stdout);
                return Ok(if comparison.ok() { 0 } else { EXIT_DRIFT });
            }
            if cli.json {
                print_json(ui, &json!({ "tools": brief::tools(&tools, long) }));
            } else {
                listed(ui, long, || {
                    ui.line(&format!("{} tool(s):\n", tools.len()));
                    print_tools(ui, &tools, long);
                });
            }
            Ok(0)
        }

        Cmd::Grep(flags) => grep::run(ui, &store, &opts, cli.json, flags),

        Cmd::Info { target } => {
            let conn = dial(&store, &opts, &target)?;
            let init = &conn.server_info;
            if cli.json {
                print_json(ui, init);
            } else {
                ui.paged(|| {
                    let si = &init["serverInfo"];
                    ui.line(&format!(
                        "{} {}",
                        si["name"].as_str().unwrap_or("?"),
                        si["version"].as_str().unwrap_or("")
                    ));
                    ui.line(&format!(
                        "protocol {}",
                        init["protocolVersion"].as_str().unwrap_or("?")
                    ));
                    let caps: Vec<&str> = init["capabilities"]
                        .as_object()
                        .map(|o| o.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    ui.line(&format!(
                        "capabilities: {}",
                        if caps.is_empty() {
                            "(none)".into()
                        } else {
                            caps.join(", ")
                        }
                    ));
                    if let Some(instr) = init["instructions"].as_str() {
                        ui.line(&format!("\n{}", instr.trim()));
                    }
                });
            }
            Ok(0)
        }

        Cmd::Call {
            target,
            tool,
            arguments,
            elicit,
            no_browser,
            check,
            strict,
            task,
            detach,
            ttl,
        } => {
            let promised = check.as_deref().map(snapshot::read).transpose()?;
            opts.elicit = elicitation(ui, elicit.as_deref(), no_browser, cli.json, false)?;
            let form = args::form(&arguments)?;
            // A JSON object is settled before anything is dialed, as it always
            // was; pairs wait for the schema only an open session can supply.
            let object = form
                .json()
                .map(|text| {
                    read_json_arg(text, "arguments").map_err(|e| {
                        Failure::hinted(
                            e,
                            json_arg_hint(
                                text,
                                &format!("mcpdial call {} {tool}", shell_word(&target)),
                                format!(
                                    "`mcpdial schema {} {tool}` shows what {tool} takes",
                                    shell_word(&target)
                                ),
                            ),
                        )
                    })
                })
                .transpose()?;
            let r = client::resolve(&store, &target)?;
            refuse_denied(&r.config, &r.name, &tool)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            // Nothing is sent for a tool the snapshot says has moved. A
            // comparison that passes says nothing at all, so the result stays
            // the only thing this command prints.
            if let Some(promised) = promised {
                let live = conn.list_tools()?;
                let comparison = snapshot::compare(
                    &snapshot::named(&promised, &tool),
                    &snapshot::named(&live, &tool),
                    strict,
                );
                if !comparison.ok() {
                    comparison.report(ui, cli.json, snapshot::Report::Stderr);
                    return Ok(EXIT_DRIFT);
                }
            }
            let prefix = format!("mcpdial call {}", shell_word(&target));
            let tools_cmd = format!("`mcpdial tools {}`", shell_word(&target));
            let hint = |conn: &mut client::Connection, argument_error: bool| {
                let tools = conn.list_tools().unwrap_or_default();
                call_hint(&tools, &tool, argument_error, &prefix, "'", &tools_cmd)
            };
            // The schema is what a pair is read against and what a prompt asks
            // from, so one listing serves both, and neither costs a program a
            // request it was not making already.
            let tools = if ui.asks() || args::needs_schema(form.pairs()) {
                conn.list_tools().unwrap_or_default()
            } else {
                Vec::new()
            };
            let schema = find_tool(&tools, &tool).map_or(Value::Null, |t| t["inputSchema"].clone());
            let mut arguments = match object {
                Some(object) => object,
                None => args::parse_pairs(form.pairs(), &schema).map_err(|e| Failure {
                    error: e,
                    hint: call_hint(&tools, &tool, true, &prefix, "'", &tools_cmd),
                    tool: None,
                })?,
            };
            prompt::fill(ui, &schema, &mut arguments, &format!("{prefix} {tool}"))?;
            let outcome = tasks::call(
                ui,
                &store,
                &mut conn,
                tasks::Wanted {
                    tool: &tool,
                    arguments,
                    known: &tools,
                },
                tasks::planned(task, detach, ttl),
                &mut notices,
            );
            let mut result = match outcome {
                Ok(tasks::Outcome::Result(result)) => result,
                // Nothing ran to completion here: the id is the whole answer,
                // and `mcpdial tasks TARGET` is what turns it back into one.
                Ok(tasks::Outcome::Detached(started)) => {
                    tasks::print_detached(ui, &started, cli.json);
                    return Ok(0);
                }
                // The server said no; say what it wanted instead.
                Err(f) => {
                    let hint = f.hint.or_else(|| {
                        server_refused(&f.error)
                            .then(|| hint(&mut conn, is_argument_error(&f.error)))
                            .flatten()
                    });
                    return Err(Failure {
                        error: f.error,
                        hint,
                        tool: f.tool,
                    });
                }
            };
            let (is_error, text) =
                printed_result(ui, &out, &mut result, cli.json, save_dir, &tool)?;
            // A failed result that is really a schema complaint, or a server's way
            // of saying it has no such tool, gets the same answer as the JSON-RPC
            // error other servers would have sent.
            if is_error {
                if let Some(hint) = hint(&mut conn, reads_as_argument_error(&text)) {
                    print_hint(ui, &hint, cli.json);
                }
            }
            Ok(if is_error { EXIT_ERROR } else { 0 })
        }

        Cmd::Tasks(flags) => tasks::run(
            ui,
            &store,
            &opts,
            tasks::Printing {
                out: &out,
                save_dir,
                json: cli.json,
            },
            &mut notices,
            flags,
        ),

        Cmd::Raw {
            target,
            method,
            params,
        } => {
            let params = read_json_arg(&params, "params")?;
            let mut conn = dial(&store, &opts, &target)?;
            let result = conn.session.request_once(&method, Some(params))?;
            let sent = out.deliver(
                Payload::Json {
                    value: &result,
                    one_line: false,
                },
                false,
            )?;
            ui.paged(|| output::show(ui, sent, As::Json, cli.json, || print_json(ui, &result)));
            Ok(0)
        }

        Cmd::Serve {
            target,
            listen,
            stdio,
            listen_any,
            bearer_env,
            allow,
            deny,
        } => Ok(serve::run(
            store,
            opts,
            &target,
            serve::Settings {
                listen,
                stdio,
                listen_any,
                bearer_env,
                allow,
                deny,
                json: cli.json,
            },
        )?),

        Cmd::Login {
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
        } => {
            let r = client::resolve(&store, &target)?;
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
            let http = client::oauth_http(&opts, &r.name, opts.timeout_for(&r)?);
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
                "client-credentials" => {
                    oauth::login_client_credentials(&http, &url, &login_opts, notify)?
                }
                _ => oauth::login(&http, &url, existing.as_ref(), &login_opts, notify)?,
            };
            store.save_credential(&r.name, cred.clone())?;
            let refreshable = cred.can_refresh();
            if cli.json {
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

        Cmd::Logout { target } | Cmd::Token(TokenCmd::Rm { name: target }) => {
            let (name, dialable) = credential_key(&store, target);
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
            if cli.json {
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

        Cmd::Token(TokenCmd::Set { name, env }) => {
            let (key, _) = credential_key(&store, name);
            let token = read_secret(ui, env.as_deref(), "token")?;
            let mut cred = store.credential(&key)?.unwrap_or_default();
            cred.access_token = Some(token);
            cred.expires_at = None;
            cred.source = Some("manual".into());
            store.save_credential(&key, cred)?;
            if cli.json {
                print_value(ui, &json!({ "saved_credential": key }), true);
            } else {
                ui.err_line(&format!("saved token for {key}"));
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Show { name }) => {
            let (key, _) = credential_key(&store, name);
            let Some(cred) = store.credential(&key)? else {
                return Err(Error::config(format!("no credential saved for {key}")).into());
            };
            // Where it is kept earns its line only once that is not the default,
            // which is also the only time reading it off `token show` tells anyone
            // anything they could not assume.
            let backend = store.backend()?;
            let elsewhere = (backend != Backend::File).then(|| backend.label());
            if cli.json {
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

        Cmd::Config(ConfigCmd::Credentials { store: chosen }) => {
            let Some(chosen) = chosen else {
                let (backend, source) = match store.forced_backend() {
                    Some(forced) => (forced?, keychain::ENV_BACKEND),
                    None => match store.saved_backend()? {
                        Some(saved) => (saved, "config.json"),
                        None => (Backend::default(), "default"),
                    },
                };
                if cli.json {
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
            if cli.json {
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
    }
}

/// How a credential store is named in a sentence.
fn credential_store(backend: Backend) -> &'static str {
    match backend {
        Backend::File => "credentials.json",
        Backend::Keychain => "the OS keychain",
    }
}

const LISTING_HEADERS: [&str; 8] = [
    "NAME", "TYPE", "STATUS", "AGE", "AUTH", "DAEMON", "SERVER", "TOOLS",
];

/// `tools` with no target: the probes under a key, as `tools TARGET` puts its
/// own list under `tools`. A bare array could never grow a field beside them.
#[derive(serde::Serialize)]
struct Servers<'a> {
    servers: &'a [client::Probe],
}

/// What was saved about one server, as every `ls --json` row carries it. A probe
/// adds its status fields on top of these rather than in place of them, so a
/// program reading a row never has to know whether `--no-probe` was passed.
fn saved_row(name: &str, cfg: &ServerConfig, credential: bool, running: bool) -> Value {
    json!({
        "name": name, "kind": cfg.kind(), "location": cfg.location(),
        "headers": cfg.headers, "token_env": cfg.token_env,
        "credential": credential,
        "source": cfg.source, "timeout": cfg.timeout,
        "running": running,
        "allow": cfg.allow, "deny": cfg.deny,
    })
}

/// One server as `ls` shows it, which is also what `add` shows after dialing.
fn listing_row(l: &Listing) -> Vec<String> {
    vec![
        l.name.clone(),
        l.kind.into(),
        l.status.label(),
        age_label(l.age_seconds),
        auth_label(l),
        daemon_label(l.running),
        l.server
            .clone()
            .or_else(|| l.status.detail().map(truncate))
            .unwrap_or_else(|| "-".into()),
        l.tools.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
    ]
}

fn auth_label(l: &Listing) -> String {
    match (l.auth, &l.status) {
        (client::AuthUsed::Env, _) => l
            .token_env
            .as_ref()
            .map_or_else(|| "env".into(), |var| format!("${var}")),
        (client::AuthUsed::Saved, _) => "saved".into(),
        (client::AuthUsed::None, Status::AuthRequired) => "needed".into(),
        (client::AuthUsed::None, Status::TokenRejected) => "rejected".into(),
        (client::AuthUsed::None, _) => "-".into(),
    }
}

fn daemon_label(running: bool) -> String {
    if running { "running" } else { "-" }.into()
}

/// `--idle SECS` as a duration; zero or less would be a daemon that quits at once.
fn idle_duration(secs: Option<f64>) -> Result<Option<Duration>, Failure> {
    match secs {
        None => Ok(None),
        Some(s) if s > 0.0 => Ok(Some(Duration::from_secs_f64(s))),
        Some(_) => Err(Error::usage("--idle needs a positive number of seconds").into()),
    }
}

fn age_label(seconds: u64) -> String {
    match seconds {
        0 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s => format!("{}m", s / 60),
    }
}

fn expiry_label(cred: &Credential) -> String {
    match cred.expires_at {
        None => "no expiry recorded".into(),
        Some(t) => {
            let now = mcpdial::config::now();
            if t <= now {
                "expired".into()
            } else {
                let secs = t - now;
                if secs >= 86_400 {
                    format!("expires in {}d", secs / 86_400)
                } else if secs >= 3600 {
                    format!("expires in {}h", secs / 3600)
                } else {
                    format!("expires in {}m", (secs / 60).max(1))
                }
            }
        }
    }
}

fn truncate(s: &str) -> String {
    truncate_at(s.lines().next().unwrap_or(""), 60)
}

fn print_tools(ui: &dyn Presenter, tools: &[Value], long: bool) {
    // A `--long` listing has room to spell out what calling a tool does; the
    // short one carries only the hint a caller cannot afford to miss.
    let hints: &dyn Fn(&Value) -> String = if long {
        &client::hint_tags
    } else {
        &client::hint_mark
    };
    ui.named(tools, long, "parameters", &describe_params, hints);
}

/// A `--long` listing reads like a document and may run past the screen; the
/// short one is a summary that belongs on it.
fn listed(ui: &dyn Presenter, long: bool, print: impl FnOnce()) {
    if long {
        ui.paged(print);
    } else {
        print();
    }
}

fn print_prompts(ui: &dyn Presenter, prompts: &[Value], long: bool) {
    ui.named(prompts, long, "arguments", &describe_prompt_args, &|_| {
        String::new()
    });
}

/// A prompt's arguments carry no schema: every one of them is a string.
fn describe_prompt_args(prompt: &Value) -> Vec<String> {
    prompt["arguments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|a| {
            let mut line = a["name"].as_str().unwrap_or("?").to_string();
            if a["required"].as_bool().unwrap_or(false) {
                line.push_str(" (required)");
            }
            if let Some(d) = a["description"].as_str() {
                let first = d.trim().lines().next().unwrap_or("");
                if !first.is_empty() {
                    line.push_str(&format!(" - {first}"));
                }
            }
            line
        })
        .collect()
}
