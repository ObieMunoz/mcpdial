//! Finding out what is out there, and what a server offers.
//!
//! The registry and the catalog answer the first question, and a dialed
//! server's own listings answer the second.

use crate::brief;
use crate::cli::Completing;
use crate::cmd::{dial, info_hint, resources_hint, Ctx};
use crate::diagnose::{missing_capability, missing_item, shell_word};
use crate::failure::{Failure, EXIT_ERROR};
use crate::media::{emit_resource, resource_stem, MediaFiles};
use crate::render::{print_json, print_prompts, truncate};
use crate::validate::read_json_arg;
use mcpdial::registry::Registry;
use mcpdial::{catalog, Error};
use serde_json::{json, Value};

#[derive(clap::Args)]
pub(crate) struct SearchFlags {
    /// Words that must all appear in an entry's name, title or description
    pub(crate) query: Vec<String>,
    /// How many matches to show
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub(crate) limit: usize,
    /// Fetch the whole list again, even if the local copy is recent
    #[arg(long, conflicts_with = "offline")]
    pub(crate) refresh: bool,
    /// Search the local copy as it is, without touching the network
    #[arg(long)]
    pub(crate) offline: bool,
}

pub(crate) fn search(cx: &mut Ctx<'_>, flags: SearchFlags) -> Result<u8, Failure> {
    let SearchFlags {
        query,
        limit,
        refresh,
        offline,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
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
    let mut progress = |n: usize| ui.progress(&format!("fetching the registry: {n} servers"));
    let indexed = mcpdial::registry::index(store, &registry, sync, &mut progress);
    ui.progress_end();
    let (index, note) = indexed?;
    if let Some(note) = note {
        ui.note(&note);
    }
    let loaded = catalog::load(
        store,
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
        if json {
            print_json(ui, &hits);
        } else {
            ui.err_line(&format!("no registry entry matches {query:?}"));
        }
        return Ok(EXIT_ERROR);
    }
    let shown: Vec<&Value> = hits.iter().copied().take(limit).collect();
    if json {
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

pub(crate) fn resources(cx: &mut Ctx<'_>, target: String, long: bool) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let mut conn = dial(store, opts, &target)?;
    let resources = conn
        .session
        .list_resources()
        .map_err(|e| missing_capability(e, "resources", &info_hint(&target)))?;
    // Templates are optional even where resources are not, so a server with
    // none of them must not turn the whole listing into an error.
    let templates = conn.session.list_resource_templates().unwrap_or_default();
    if json {
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

pub(crate) fn read(cx: &mut Ctx<'_>, target: String, uri: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let out = &cx.out;
    let notices = &mut cx.notices;
    let save_dir = cx.save_dir.as_deref();
    let json = cx.json;
    let mut conn = dial(store, opts, &target)?;
    let outcome = conn.session.read_resource_watching(&uri, notices);
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
    ui.paged(|| emit_resource(ui, out, &mut result, json, false, &files, &redirect))?;
    Ok(0)
}

pub(crate) fn prompts(cx: &mut Ctx<'_>, target: String, long: bool) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let mut conn = dial(store, opts, &target)?;
    let prompts = conn
        .session
        .list_prompts()
        .map_err(|e| missing_capability(e, "prompts", &info_hint(&target)))?;
    if json {
        print_json(ui, &json!({ "prompts": brief::prompts(&prompts, long) }));
    } else {
        ui.line(&format!("{} prompt(s):\n", prompts.len()));
        print_prompts(ui, &prompts, long);
    }
    Ok(0)
}

pub(crate) fn complete(
    cx: &mut Ctx<'_>,
    target: String,
    of: crate::cli::Completing,
) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
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
    let mut conn = dial(store, opts, &target)?;
    let found = conn
        .session
        .complete(reference, json!({ "name": name, "value": value }), context)
        .map_err(|e| missing_capability(e, "completions", &info_hint(&target)))?;
    if json {
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

pub(crate) fn catalog(cx: &mut Ctx<'_>, offline: bool) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let loaded = catalog::load(
        store,
        &catalog::Source::from_env(),
        offline,
        opts.timeout_or_default(),
        &opts.user_agent,
    )?;
    if opts.verbose {
        ui.err_line(&format!("catalog: {}", loaded.origin));
    }
    if json {
        print_json(ui, &loaded.entries);
    } else {
        ui.catalog(&loaded.entries);
        ui.err_line("\nadd one with: mcpdial add NAME --catalog ID");
    }
    Ok(0)
}

pub(crate) fn info(cx: &mut Ctx<'_>, target: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    let conn = dial(store, opts, &target)?;
    let init = &conn.server_info;
    if json {
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

pub(crate) fn guide(cx: &mut Ctx<'_>) -> Result<u8, Failure> {
    let ui = cx.ui;
    let _out = &cx.out;
    ui.out(include_str!("../../docs/AGENTS.md"));
    Ok(0)
}
