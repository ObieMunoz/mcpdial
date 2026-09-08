//! `edit`: the last call's arguments through `$EDITOR` and back.

use super::history::{History, Origin};
use crate::{file_stem, output, parse_object};
use mcpdial::Error;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// The call an `edit` line names: the one that made result N, or the last call
/// of the session where it names nothing.
pub(crate) fn edit_target(results: &History, named: &str) -> Result<(String, Value), Error> {
    if named.is_empty() {
        return results
            .last_call(None)
            .map(|(tool, arguments)| (tool.to_string(), arguments.clone()))
            .ok_or_else(|| Error::usage("no call in this session yet to edit"));
    }
    let rec = results.find(named)?;
    match &rec.origin {
        Origin::Call { tool, arguments } => Ok((tool.clone(), arguments.clone())),
        other => Err(Error::usage(format!(
            "result {} came from {}, and edit re-runs a call",
            rec.number,
            other.command()
        ))),
    }
}

/// The arguments of a call as `$VISUAL` or `$EDITOR` leaves them. The file is
/// this process's own, made where it cannot already exist and removed however
/// the editor goes; an editor that exits badly, or leaves nothing behind, sends
/// no call at all.
pub(crate) fn edit_arguments(tool: &str, arguments: &Value) -> Result<Value, Error> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .ok_or_else(|| {
            Error::usage("edit needs an editor: set $EDITOR (or $VISUAL) to the one you use")
        })?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let path = std::env::temp_dir().join(format!(
        "mcpdial-edit-{}-{}-{unique}.json",
        file_stem(tool),
        std::process::id()
    ));
    let document = output::document(arguments, false);
    output::create("edit", &path, document.as_bytes())?;
    let edited = run_editor(&editor, &path).and_then(|()| read_arguments(&path));
    std::fs::remove_file(&path).ok();
    edited
}

fn run_editor(editor: &str, path: &Path) -> Result<(), Error> {
    let mut words = mcpdial::transport::stdio::split_command(editor)?;
    if words.is_empty() {
        return Err(Error::usage(format!("edit: {editor:?} is not a command")));
    }
    let program = words.remove(0);
    let status = Command::new(&program)
        .args(&words)
        .arg(path)
        .status()
        .map_err(|e| Error::usage(format!("edit: cannot run {program}: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Err(Error::usage(format!(
        "edit: {program} exited without saving; nothing was sent"
    )))
}

fn read_arguments(path: &Path) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::usage(format!("edit {}: {e}", path.display())))?;
    if text.trim().is_empty() {
        return Err(Error::usage(
            "edit: the file was left empty; nothing was sent",
        ));
    }
    parse_object(&text, "arguments")
}
