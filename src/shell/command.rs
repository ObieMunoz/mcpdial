//! What a shell line does, once the word in front of it has been read.
//!
//! The word decides everything: which of these runs, whether a trailing
//! `| expression` is a filter or part of the argument, and what a failure is
//! answered with. Nothing here dials - the session is open already and is
//! handed in.

use super::help::{SAVE, SHOW_USAGE};
use super::history::{History, Keep, Origin, Recorded};
use super::input::shell_tools;
use super::shell_watched;
use crate::diagnose::{
    call_hint, find_tool, is_argument_error, reads_as_argument_error, server_refused, shell_word,
    suggest_tool,
};
use crate::media::{emit_rendered, file_stem, rendered, MediaFiles};
use crate::notices::Notices;
use crate::output::{As, Output, Payload};
use crate::path::{Filter, Filtered};
use crate::present::Presenter;
use crate::{output, print_hint, print_note, print_tool_result, print_value, prompt, Failure};
use mcpdial::session::{render_content, resource_bodies};
use mcpdial::subscribe::Subscriptions;
use mcpdial::{client, Error};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The answer to a tool name this server does not have.
pub(crate) fn no_such_tool(tools: &[Value], name: &str) -> Failure {
    Failure {
        error: Error::usage(format!("no tool named {name:?}")),
        hint: suggest_tool(tools, name, "`tools`"),
        tool: None,
    }
}

/// [`call_hint`] for a `call` line typed at the shell, where the arguments are
/// written bare and the tool list comes from the session already open.
pub(crate) fn shell_call_hint(
    cache: &mut Option<Vec<Value>>,
    conn: &mut client::Connection,
    tool: &str,
    argument_error: bool,
) -> Option<String> {
    call_hint(
        shell_tools(cache, conn),
        tool,
        argument_error,
        "call",
        "",
        "`tools`",
    )
}

/// What a shell `call` will send: the arguments that were typed, plus whatever
/// the tool requires and nobody typed, asked for where there is someone to ask.
/// The tool list is the one the hints read, so asking costs no extra request.
pub(crate) fn shell_fill(
    ui: &dyn Presenter,
    cache: &mut Option<Vec<Value>>,
    conn: &mut client::Connection,
    tool: &str,
    arguments: &mut Value,
) -> Result<(), Error> {
    if !ui.asks() {
        return Ok(());
    }
    let schema =
        find_tool(shell_tools(cache, conn), tool).map_or(Value::Null, |t| t["inputSchema"].clone());
    prompt::fill(ui, &schema, arguments, &format!("call {tool}"))
}

/// The first word of what follows a command, and the rest of the line, both
/// trimmed. Unlike [`name_and_args`] nothing stands in for a part that is not
/// there: these commands take a bare word, not a JSON object.
pub(crate) fn split_word(rest: &str) -> (&str, &str) {
    rest.split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, more)| (word, more.trim()))
}

/// A shell line, and the `| ...` it may end with. The bar is looked for outside
/// any JSON string, so a `|` inside an argument stays part of that argument;
/// only the first one is a bar, and everything after it is the filter, which is
/// how a `jq` filter keeps the pipes of its own.
pub(crate) fn split_filter(line: &str) -> (&str, Option<&str>) {
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if in_string {
            match c {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                '"' => in_string = true,
                '|' => return (line[..at].trim_end(), Some(line[at + 1..].trim())),
                _ => {}
            }
        }
    }
    (line, None)
}

/// A line that is nothing but the name of a result already printed: `_` for the
/// last, `$3` for the third. Neither can be a tool name, so neither is a command
/// anybody was going to type by accident.
pub(crate) fn names_a_result(word: &str) -> bool {
    word == "_" || word.starts_with('$')
}

/// Whether a `| ...` can follow this command: the ones that print a result, and
/// the ways of naming one already printed.
pub(crate) fn takes_a_filter(word: &str) -> bool {
    matches!(
        word,
        "call" | "read" | "prompt" | "raw" | "retry" | "edit" | "show"
    ) || names_a_result(word)
}

/// A line a `retry` or an `edit` is handing back to the loop, with the `| ...`
/// its own line ended with put back on the end of it: the loop reads that line
/// exactly as it reads a typed one, so the filter has to be part of it.
pub(crate) fn with_filter(line: String, expression: Option<&str>) -> String {
    match expression {
        Some(filter) => format!("{line} | {filter}"),
        None => line,
    }
}

/// A result through the `| ...` its line ended with, on stdout. The selected
/// lines leave the way a result's text does, so `--max-chars` and `--output`
/// keep their say over them, and a filter that selected nothing prints nothing
/// at all rather than a blank line.
pub(crate) fn shell_filter(
    ui: &dyn Presenter,
    out: &Output,
    filter: &Filter,
    result: &Value,
    json: bool,
) -> Result<(), Failure> {
    let Filtered { text, note } = filter.apply(result, json)?;
    if let Some(text) = text {
        let sent = out.deliver(Payload::Text(&text), result["isError"] == true)?;
        output::show(ui, sent, As::Text, json, || ui.text(&text));
    }
    if !note.is_empty() {
        print_note(ui, &note, json);
    }
    Ok(())
}

/// One numbered result on stdout: through the `| ...` the line ended with, or
/// exactly as it printed the first time.
pub(crate) fn shell_named(
    ui: &dyn Presenter,
    out: &Output,
    results: &History,
    named: &str,
    filter: Option<&Filter>,
    json: bool,
    target: &str,
) -> Result<(), Failure> {
    let rec = results
        .find(named)
        .map_err(|e| Failure::hinted(e, SHOW_USAGE))?;
    // The number it already had: naming a result again does not make a new one.
    ui.numbered(rec.number);
    match filter {
        Some(filter) => ui.paged(|| shell_filter(ui, out, filter, &rec.result, json)),
        None => ui.paged(|| shell_show(ui, out, rec, json, target)),
    }
}

/// A shell line that did not work, said the way that session says them.
pub(crate) fn shell_failed(ui: &dyn Presenter, failure: &Failure, json: bool) {
    if json {
        print_value(ui, &failure.to_json(), true);
    } else {
        failure.report(ui);
    }
}

/// The parts of a `shell` session one line changes: the connection, what the
/// session has learned about the server, and what it has printed - with the
/// `| ...` that line ended with, which decides how its result prints.
pub(crate) struct Live<'a, 'b> {
    pub(crate) conn: &'a mut client::Connection,
    pub(crate) notices: &'a mut Notices<'b>,
    /// tools/list, fetched at most once per session, so a mistake can be
    /// answered with the shape the server actually wants.
    pub(crate) tools: &'a mut Option<Vec<Value>>,
    pub(crate) results: &'a mut History,
    /// What the session is following, so that a `list_changed` or an update to
    /// a followed resource arriving during this call is put aside for the
    /// prompt rather than drawn into the middle of the result.
    pub(crate) subs: &'a mut Subscriptions,
    pub(crate) filter: Option<&'a Filter>,
}

/// One `call` at the shell, from the arguments it settled on to the result filed
/// under the number printed before it. The tool list travels with it because a
/// tool that refuses a call is answered with the shape it wanted instead.
pub(crate) fn shell_call(
    ui: &dyn Presenter,
    out: &Output,
    live: &mut Live<'_, '_>,
    tool: &str,
    arguments: Value,
    json: bool,
    save_dir: Option<&Path>,
) -> Result<(), Failure> {
    // What went out, kept before the answer comes back: a call that failed is
    // the one most worth running again.
    live.results.sending(tool, &arguments);
    let sent = arguments.clone();
    let mut result = match shell_watched(live.notices, live.subs, |w| {
        live.conn.session.call_tool_watching(tool, arguments, w)
    }) {
        Ok(result) => result,
        Err(e) => {
            let hint = server_refused(&e)
                .then(|| shell_call_hint(live.tools, live.conn, tool, is_argument_error(&e)))
                .flatten();
            return Err(Failure {
                error: e,
                hint,
                tool: None,
            });
        }
    };
    let files = MediaFiles {
        dir: save_dir,
        stem: file_stem(tool),
    };
    let text = rendered(ui, &mut result, json, &files, render_content)?;
    ui.numbered(live.results.next_number());
    // A line that asked for one field of the result is answered with that field
    // and nothing else, hint included: it named what it wanted.
    let outcome = match live.filter {
        Some(filter) => ui.paged(|| shell_filter(ui, out, filter, &result, json)),
        None => match ui.paged(|| print_tool_result(ui, out, &result, &text, json, true)) {
            Err(f) => Err(f),
            Ok(failed) => {
                if failed {
                    let argument_error = reads_as_argument_error(&text);
                    if let Some(hint) = shell_call_hint(live.tools, live.conn, tool, argument_error)
                    {
                        print_hint(ui, &hint, json);
                    }
                }
                Ok(())
            }
        },
    };
    live.results.record(
        Origin::Call {
            tool: tool.to_string(),
            arguments: sent,
        },
        result,
        text,
    );
    outcome
}

/// A recorded result printed again, the way the line that first printed it did:
/// the same text, the same object, the same bytes. Nothing is re-rendered, so a
/// `--save-dir` gains no second copy of an image already filed.
fn shell_show(
    ui: &dyn Presenter,
    out: &Output,
    rec: &Recorded,
    json: bool,
    target: &str,
) -> Result<(), Failure> {
    match &rec.origin {
        Origin::Call { .. } => {
            print_tool_result(ui, out, &rec.result, &rec.text, json, true).map(|_| ())
        }
        Origin::Prompt { .. } => emit_rendered(ui, out, &rec.result, &rec.text, json, true),
        Origin::Raw { .. } => {
            print_value(ui, &rec.result, json);
            Ok(())
        }
        Origin::Read { .. } if json => emit_rendered(ui, out, &rec.result, &rec.text, json, true),
        Origin::Read { uri } => {
            let bodies = resource_bodies(&rec.result)?;
            let sent = out.deliver_resource(&bodies)?;
            let redirect = format!("mcpdial read {} {}", shell_word(target), uri);
            let mut refused = Ok(());
            output::show(ui, sent, As::Raw, json, || {
                refused = ui.resource(&bodies, &redirect);
            });
            refused
        }
    }
}

/// One recorded result in a file, written through the sink `--output` writes
/// through: a file already there, a path that is a directory and a directory
/// that cannot be written are refused in the same words, and the line left
/// behind counts the same units. An empty `file` names one after the tool and
/// the media type instead.
pub(crate) fn shell_save(
    ui: &dyn Presenter,
    rec: &Recorded,
    file: &str,
    json: bool,
) -> Result<(), Failure> {
    let keep = rec.keep(json)?;
    let path = match file.is_empty() {
        true => PathBuf::from(rec.filename(&keep)),
        false => PathBuf::from(file),
    };
    output::reserve(SAVE, &path)?;
    let sink = Output::to_path(SAVE, path, json);
    let sent = match &keep {
        Keep::Document => sink.deliver(
            Payload::Json {
                value: &rec.result,
                one_line: true,
            },
            rec.failed(),
        )?,
        Keep::Text => sink.deliver(Payload::Text(&rec.text), rec.failed())?,
        Keep::Bytes { bytes, .. } => sink.deliver(Payload::Bytes(bytes), rec.failed())?,
        Keep::Bodies { bodies, .. } => sink.deliver_resource(bodies)?,
    };
    output::show(ui, sent, As::Text, json, || {});
    Ok(())
}

/// The line a `retry` or an `edit` is about to run, said the way it would have
/// been typed, so that what went out is on the screen beside what comes back.
pub(crate) fn print_rerun(ui: &dyn Presenter, line: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "rerun": line }).to_string());
    } else {
        ui.aside(line);
    }
}
