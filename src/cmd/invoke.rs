//! Running something on a server: a tool, a prompt, or a raw method.
//!
//! These are the commands with a result worth printing, which is why they are
//! the ones `--save-dir`, `--max-chars` and `--output` apply to.

use crate::cli::TARGET_HELP;
use crate::cmd::{dial, elicitation, info_hint, prompts_hint, Ctx};
use crate::diagnose::{
    call_hint, closest, find_tool, is_argument_error, json_arg_hint, missing_item,
    reads_as_argument_error, server_refused, shell_word, NO_SERVERS,
};
use crate::failure::{refuse_denied, Failure, EXIT_DRIFT, EXIT_ERROR};
use crate::media::{emit, file_stem, MediaFiles};
use crate::output::{As, Payload};
use crate::render::{
    listed, print_hint, print_json, print_tools, printed_result, truncate, Servers,
};
use crate::validate::read_json_arg;
use crate::{args, brief, output, prompt, snapshot, tasks};
use mcpdial::session::render_messages;
use mcpdial::{client, Error};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ToolsFlags {
    /// A saved name, an http(s):// URL, or stdio:<command>; every saved server when omitted
    pub(crate) target: Option<String>,
    /// Show full descriptions and parameters
    #[arg(short, long)]
    pub(crate) long: bool,
    /// Include the tools the server's allow and deny lists hide, marked (denied)
    #[arg(long, requires = "target")]
    pub(crate) all: bool,
    /// Write the tools in full to FILE, which must not exist, instead of listing them
    #[arg(long, value_name = "FILE", requires = "target", conflicts_with_all = ["long", "all"])]
    pub(crate) snapshot: Option<PathBuf>,
    /// Report how the tools differ from a snapshot; exit 3 when a caller would break
    #[arg(long, value_name = "FILE", requires = "target",
          conflicts_with_all = ["snapshot", "long", "all"])]
    pub(crate) check: Option<PathBuf>,
    /// With --check: hold every snapshotted tool to the object the snapshot holds
    #[arg(long, requires = "check")]
    pub(crate) strict: bool,
}

#[derive(clap::Args)]
pub(crate) struct CallFlags {
    #[arg(help = TARGET_HELP)]
    pub(crate) target: String,
    /// A tool name from `mcpdial tools TARGET`
    pub(crate) tool: String,
    /// One JSON object (inline, @file, or - for stdin), or key=value pairs
    pub(crate) arguments: Vec<String>,
    /// Answers for anything the server elicits mid-call: a JSON object or @file
    #[arg(long, value_name = "JSON")]
    pub(crate) elicit: Option<String>,
    /// Print a url-mode elicitation's address instead of opening a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
    /// Refuse to call when TOOL has drifted from this snapshot; exit 3 with the differences
    #[arg(long, value_name = "FILE")]
    pub(crate) check: Option<PathBuf>,
    /// With --check: hold TOOL to the object the snapshot holds
    #[arg(long, requires = "check")]
    pub(crate) strict: bool,
    /// Have the server run TOOL in the background, and poll until it finishes
    #[arg(long, conflicts_with = "detach")]
    pub(crate) task: bool,
    /// Start TOOL in the background, print the task id, and exit
    #[arg(long)]
    pub(crate) detach: bool,
    /// Seconds the server is asked to keep a --task or --detach task for (default 3600)
    #[arg(long, value_name = "SECS")]
    pub(crate) ttl: Option<u64>,
}

#[derive(clap::Args)]
pub(crate) struct PromptFlags {
    #[arg(help = TARGET_HELP)]
    pub(crate) target: String,
    /// A prompt name from `mcpdial prompts TARGET`
    pub(crate) name: String,
    /// One JSON object (inline, @file, or - for stdin), or key=value pairs
    pub(crate) arguments: Vec<String>,
    /// Answers for anything the server elicits mid-call: a JSON object or @file
    #[arg(long, value_name = "JSON")]
    pub(crate) elicit: Option<String>,
    /// Print a url-mode elicitation's address instead of opening a browser
    #[arg(long)]
    pub(crate) no_browser: bool,
}

pub(crate) fn schema(cx: &mut Ctx<'_>, target: String, tool: String) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let r = client::resolve(store, &target)?;
    refuse_denied(&r.config, &r.name, &tool)?;
    let mut conn = client::connect(store, &r, opts)?;
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
            hint: closest(&tool, names.iter().copied()).map(|near| format!("did you mean {near}?")),
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

pub(crate) fn prompt(cx: &mut Ctx<'_>, flags: PromptFlags) -> Result<u8, Failure> {
    let PromptFlags {
        target,
        name,
        arguments,
        elicit,
        no_browser,
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &mut cx.opts;
    let out = &cx.out;
    let notices = &mut cx.notices;
    let save_dir = cx.save_dir.as_deref();
    let json = cx.json;
    opts.elicit = elicitation(ui, elicit.as_deref(), no_browser, json, false)?;
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
    let mut conn = dial(store, opts, &target)?;
    let outcome = conn.session.get_prompt_watching(&name, arguments, notices);
    notices.finish();
    let mut result = outcome
        .map_err(|e| missing_item(e, "prompts", &prompts_hint(&target), &info_hint(&target)))?;
    if !json {
        if let Some(d) = result["description"].as_str() {
            ui.err_line(d);
        }
    }
    let files = MediaFiles {
        dir: save_dir,
        stem: file_stem(&name),
    };
    ui.paged(|| emit(ui, out, &mut result, json, false, &files, render_messages))?;
    Ok(0)
}

pub(crate) fn call(cx: &mut Ctx<'_>, flags: CallFlags) -> Result<u8, Failure> {
    let CallFlags {
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
    } = flags;
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &mut cx.opts;
    let out = &cx.out;
    let notices = &mut cx.notices;
    let save_dir = cx.save_dir.as_deref();
    let json = cx.json;
    let promised = check.as_deref().map(snapshot::read).transpose()?;
    opts.elicit = elicitation(ui, elicit.as_deref(), no_browser, json, false)?;
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
    let r = client::resolve(store, &target)?;
    refuse_denied(&r.config, &r.name, &tool)?;
    let mut conn = client::connect(store, &r, opts)?;
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
            comparison.report(ui, json, snapshot::Report::Stderr);
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
        store,
        &mut conn,
        tasks::Wanted {
            tool: &tool,
            arguments,
            known: &tools,
        },
        tasks::planned(task, detach, ttl),
        notices,
    );
    let mut result = match outcome {
        Ok(tasks::Outcome::Result(result)) => result,
        // Nothing ran to completion here: the id is the whole answer,
        // and `mcpdial tasks TARGET` is what turns it back into one.
        Ok(tasks::Outcome::Detached(started)) => {
            tasks::print_detached(ui, &started, json);
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
    let (is_error, text) = printed_result(ui, out, &mut result, json, save_dir, &tool)?;
    // A failed result that is really a schema complaint, or a server's way
    // of saying it has no such tool, gets the same answer as the JSON-RPC
    // error other servers would have sent.
    if is_error {
        if let Some(hint) = hint(&mut conn, reads_as_argument_error(&text)) {
            print_hint(ui, &hint, json);
        }
    }
    Ok(if is_error { EXIT_ERROR } else { 0 })
}

pub(crate) fn raw(
    cx: &mut Ctx<'_>,
    target: String,
    method: String,
    params: String,
) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let out = &cx.out;
    let json = cx.json;
    let params = read_json_arg(&params, "params")?;
    let mut conn = dial(store, opts, &target)?;
    let result = conn.session.request_once(&method, Some(params))?;
    let sent = out.deliver(
        Payload::Json {
            value: &result,
            one_line: false,
        },
        false,
    )?;
    ui.paged(|| output::show(ui, sent, As::Json, json, || print_json(ui, &result)));
    Ok(0)
}

/// `tools`, of one server or of every saved one.
pub(crate) fn tools(cx: &mut Ctx<'_>, flags: ToolsFlags) -> Result<u8, Failure> {
    let ui = cx.ui;
    let store = &cx.store;
    let opts = &cx.opts;
    let json = cx.json;
    match flags {
        ToolsFlags {
            target: None, long, ..
        } => {
            let mut probes = client::probe_all(store, opts, true)?;
            if json {
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

        ToolsFlags {
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
            let mut conn = dial(store, opts, &target)?;
            let tools = if all {
                conn.list_all_tools()?
            } else {
                conn.list_tools()?
            };
            if let Some(path) = to_file {
                snapshot::write(&path, &snapshot::document(&conn.server_info, &tools))?;
                snapshot::wrote(ui, json, &path, tools.len());
                return Ok(0);
            }
            if let Some(promised) = promised {
                let comparison = snapshot::compare(&promised, &tools, strict);
                comparison.report(ui, json, snapshot::Report::Stdout);
                return Ok(if comparison.ok() { 0 } else { EXIT_DRIFT });
            }
            if json {
                print_json(ui, &json!({ "tools": brief::tools(&tools, long) }));
            } else {
                listed(ui, long, || {
                    ui.line(&format!("{} tool(s):\n", tools.len()));
                    print_tools(ui, &tools, long);
                });
            }
            Ok(0)
        }
    }
}
