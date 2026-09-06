//! `pick`: what a bare `mcpdial` does at a terminal, and the picking every
//! other question is asked with.
//!
//! Discovery is where a person gets stuck: which of the saved servers, which of
//! its thirty tools, and what that tool takes. So a bare `mcpdial` with a
//! person at both ends offers the servers with the status the last probe left,
//! then the tools with the first line of what each one is for, asks for the
//! arguments from the tool's own schema, and makes the call. The line that
//! would have made the same call outright is printed after the result, so that
//! what was found by hand can go into a script.
//!
//! Nothing here happens without a person. [`Presenter::asks`] is the same
//! question the schema prompts ask, false for every pipe, every `--json` and
//! every `--plain`, and this module refuses to run when it is false. A bare
//! `mcpdial` under a pipe never reaches the module at all: it gets clap's
//! usage error and exit 2, exactly as it always has.
//!
//! [`fzf`] and [`numbered`] are the two halves of picking one of a list, and
//! they are shared: `fzf` when it is on `PATH`, else the list numbered and
//! answered by its number or by the line itself. `prompt.rs` picks the values
//! an `enum` allows with the same two.

use crate::notices::Notices;
use crate::output::Output;
use crate::present::Presenter;
use crate::{args, prompt, shell_word, Failure, MediaFiles, EXIT_ERROR};
use mcpdial::client::{self, Freshness, Listing, Options};
use mcpdial::session::render_content;
use mcpdial::{Error, Store};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A person at a terminal on both streams, which is what a question needs and
/// what a pipe, a program, `--json` and `--plain` are not.
pub fn at_a_terminal(cli: &crate::Cli) -> bool {
    use std::io::IsTerminal;
    !crate::present::wants_plain(cli) && std::io::stdin().is_terminal()
}

/// Whether a bare `mcpdial` opens the picker rather than printing clap's usage
/// error: a person at a terminal, and a build with a `Rich` presenter to ask
/// them anything with. It is the same answer [`Presenter::asks`] gives, asked
/// before there is a command to make a presenter for.
pub fn wanted(cli: &crate::Cli) -> bool {
    cfg!(feature = "rich") && at_a_terminal(cli)
}

/// A server, a tool, its arguments, and the call.
pub fn run(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    out: &Output,
    notices: &mut Notices<'_>,
    save_dir: Option<&Path>,
) -> Result<u8, Failure> {
    // Both halves of what a question needs, worded as one refusal because the
    // agent surface is one set of bytes: a build without `rich` must answer a
    // pipe exactly as a build with it does.
    if !ui.asks() {
        return Err(Failure::hinted(
            Error::usage(
                "pick needs a terminal on stdin and stdout, and a build with the rich feature",
            ),
            "`mcpdial ls` names the saved servers, `mcpdial tools NAME` their tools, \
             and `mcpdial call NAME TOOL` makes the call",
        ));
    }
    let servers = client::listing(store, opts, Freshness::Remembered)?;
    if servers.is_empty() {
        return Err(Failure::hinted(
            Error::usage("no servers are saved"),
            "`mcpdial browse` ticks servers from the catalog; \
             `mcpdial add NAME --http URL` saves one outright",
        ));
    }
    let mut chooser = Chooser::new()?;
    let Some(at) = chooser.one(ui, "server", &server_lines(&servers))? else {
        return Ok(nothing_picked(ui));
    };
    let name = servers[at].name.clone();
    let r = client::resolve(store, &name)?;
    let mut conn = client::connect(store, &r, opts)?;
    let tools = conn.list_tools()?;
    if tools.is_empty() {
        return Err(Failure::hinted(
            Error::usage(format!("{name} offers no tools")),
            format!(
                "`mcpdial info {}` shows what it does offer",
                shell_word(&name)
            ),
        ));
    }
    let Some(at) = chooser.one(ui, "tool", &tool_lines(&tools))? else {
        return Ok(nothing_picked(ui));
    };
    let tool = tools[at]["name"].as_str().unwrap_or_default().to_string();
    let schema = tools[at]["inputSchema"].clone();
    // The chooser's editor has nothing left to read, and the answers below are
    // read on one of prompt.rs's own.
    drop(chooser);
    let mut arguments = json!({});
    prompt::ask_all(ui, &schema, &mut arguments)?;
    let line = reproduction(&name, &tool, &arguments, &schema);

    let outcome = conn
        .session
        .call_tool_watching(&tool, arguments, notices)
        .map_err(Failure::from);
    notices.finish();
    let finished = outcome.and_then(|mut result| {
        let files = MediaFiles {
            dir: save_dir,
            stem: crate::file_stem(&tool),
        };
        // A picked call is a person's, so it is never `--json`: `asks` is only
        // true where `Rich` was chosen, and `--json` chooses `Plain`.
        let text = crate::rendered(ui, &mut result, false, &files, render_content)?;
        let failed =
            ui.paged(|| crate::print_tool_result(ui, out, &result, &text, false, false))?;
        Ok(if failed { EXIT_ERROR } else { 0 })
    });
    // However the call went, the line that made it is the thing to keep: a
    // call that failed is the one most worth editing and running again.
    ui.aside(&line);
    finished
}

/// The command line that would have made the same call outright, quoted so it
/// survives a copy-paste into a shell.
fn reproduction(name: &str, tool: &str, arguments: &Value, schema: &Value) -> String {
    let mut line = format!("mcpdial call {} {tool}", shell_word(name));
    for pair in args::as_pairs(arguments, schema) {
        line.push(' ');
        line.push_str(&pair);
    }
    line
}

fn nothing_picked(ui: &dyn Presenter) -> u8 {
    ui.err_line("nothing picked");
    0
}

/// One line per saved server: its name, the status the last probe left, how it
/// is dialed, and how many tools it answered with.
fn server_lines(servers: &[Listing]) -> Vec<String> {
    columns(
        servers
            .iter()
            .map(|s| {
                vec![
                    s.name.clone(),
                    s.status.label(),
                    s.kind.to_string(),
                    s.tools
                        .map_or_else(|| "-".to_string(), |n| format!("{n} tools")),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

/// One line per tool: its name, and the first line of what it is for, which is
/// where a description says what it is before it says how.
fn tool_lines(tools: &[Value]) -> Vec<String> {
    columns(
        tools
            .iter()
            .map(|t| {
                let about = t["description"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim();
                vec![
                    t["name"].as_str().unwrap_or("?").to_string(),
                    crate::truncate_at(about, 80),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

/// The rows with every column but the last padded to its widest cell.
fn columns(rows: Vec<Vec<String>>) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(i) {
                Some(widest) => *widest = (*widest).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if i + 1 < row.len() {
                    line.push_str(&" ".repeat(widths[i] - cell.chars().count() + 2));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// Picking one of a list, over and over: the line editor is made once, so the
/// terminal is put into and out of raw mode once per question rather than once
/// per keystroke.
struct Chooser {
    editor: rustyline::DefaultEditor,
}

impl Chooser {
    fn new() -> Result<Self, Error> {
        rustyline::DefaultEditor::new()
            .map(|editor| Self { editor })
            .map_err(|e| Error::usage(format!("cannot offer a list to pick from: {e}")))
    }

    /// Which of `lines` was picked, or `None` where the picker was left. `fzf`
    /// where it is installed, else the lines numbered and typed at.
    fn one(
        &mut self,
        ui: &dyn Presenter,
        what: &str,
        lines: &[String],
    ) -> Result<Option<usize>, Failure> {
        match fzf(&format!("{what}> "), lines) {
            Fzf::Picked(answer) => return Ok(lines.iter().position(|line| *line == answer)),
            Fzf::Quit => return Ok(None),
            Fzf::Absent => {}
        }
        for (i, line) in lines.iter().enumerate() {
            ui.err_line(&format!("  {}) {line}", i + 1));
        }
        let question = format!("{what} (1-{}): ", lines.len());
        loop {
            use rustyline::error::ReadlineError;
            let answer = match self.editor.readline(&question) {
                Ok(answer) => answer,
                // Leaving the picker is not a failure; it is a person deciding
                // against the call before anything was sent.
                Err(ReadlineError::Interrupted | ReadlineError::Eof) => return Ok(None),
                Err(e) => return Err(Error::usage(format!("cannot read the answer: {e}")).into()),
            };
            match numbered(lines, answer.trim()) {
                Some(at) => return Ok(Some(at)),
                None => ui.err_line(&format!("  {what} is one of the {} above", lines.len())),
            }
        }
    }
}

/// Which of `shown` an answer names: its number in the list, or the line
/// itself. Where the two readings collide the numbering wins, since the number
/// is what the list just offered.
pub fn numbered(shown: &[String], answer: &str) -> Option<usize> {
    answer
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=shown.len()).contains(n))
        .map(|n| n - 1)
        .or_else(|| shown.iter().position(|line| line == answer))
}

/// What `fzf` did with the lines it was offered.
pub enum Fzf {
    Picked(String),
    /// Quit without picking, which is a person saying no to the whole thing.
    Quit,
    /// Not installed, or would not start: the numbered list stands in.
    Absent,
}

/// `fzf` over the lines, when it is on the PATH. It is the fuzzy picker the
/// people who want one already have, which is why there is no crate here.
pub fn fzf(prompt: &str, choices: &[String]) -> Fzf {
    let Some(program) = on_path("fzf") else {
        return Fzf::Absent;
    };
    let started = Command::new(program)
        .arg(format!("--prompt={prompt}"))
        .arg("--height=40%")
        .arg("--reverse")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    let Ok(mut child) = started else {
        return Fzf::Absent;
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(choices.join("\n").as_bytes()).ok();
    }
    let Ok(picked) = child.wait_with_output() else {
        return Fzf::Absent;
    };
    let answer = String::from_utf8_lossy(&picked.stdout).trim().to_string();
    match picked.status.success() && !answer.is_empty() {
        true => Fzf::Picked(answer),
        false => Fzf::Quit,
    }
}

/// `program` where a shell would find it.
pub fn on_path(program: &str) -> Option<PathBuf> {
    let names: Vec<String> = match cfg!(windows) {
        true => vec![
            format!("{program}.exe"),
            format!("{program}.cmd"),
            program.to_string(),
        ],
        false => vec![program.to_string()],
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpdial::client::{AuthUsed, Status};

    fn listing(name: &str, status: Status, tools: Option<usize>) -> Listing {
        Listing {
            name: name.to_string(),
            kind: "stdio",
            location: "echo-server".to_string(),
            status,
            auth: AuthUsed::None,
            token_env: None,
            server: None,
            tools,
            checked_at: 0,
            age_seconds: 0,
            running: false,
        }
    }

    #[test]
    fn a_server_line_carries_the_status_the_last_probe_left() {
        let rows = server_lines(&[
            listing("echo", Status::Connected, Some(5)),
            listing("work", Status::AuthRequired, None),
        ]);
        assert_eq!(rows[0], "echo  connected      stdio  5 tools");
        assert_eq!(rows[1], "work  auth required  stdio  -");
    }

    #[test]
    fn a_tool_line_is_the_name_and_the_first_line_of_what_it_is_for() {
        let rows = tool_lines(&[
            json!({"name": "echo", "description": "Echo a message back.\nThe rest is detail."}),
            json!({"name": "count"}),
        ]);
        assert_eq!(rows[0], "echo   Echo a message back.");
        assert_eq!(rows[1], "count");
    }

    #[test]
    fn a_line_is_picked_by_its_number_or_by_itself() {
        let shown = ["echo".to_string(), "count".to_string()];
        assert_eq!(numbered(&shown, "1"), Some(0));
        assert_eq!(numbered(&shown, "2"), Some(1));
        assert_eq!(numbered(&shown, "count"), Some(1));
        assert_eq!(numbered(&shown, "0"), None);
        assert_eq!(numbered(&shown, "3"), None);
        assert_eq!(numbered(&shown, "fail"), None);
        assert_eq!(numbered(&shown, ""), None);
        // Where a line reads as a number of its own, the list's numbering wins.
        let numbers = ["7".to_string(), "1".to_string()];
        assert_eq!(numbered(&numbers, "1"), Some(0));
        assert_eq!(numbered(&numbers, "7"), Some(0));
    }

    #[test]
    fn the_reproduction_line_is_the_call_written_out_and_quoted() {
        let schema = json!({
            "type": "object",
            "properties": {"message": {"type": "string"}, "limit": {"type": "integer"}},
            "required": ["message"],
        });
        assert_eq!(
            reproduction("echo", "echo", &json!({"message": "hello there"}), &schema),
            "mcpdial call echo echo 'message=hello there'"
        );
        assert_eq!(
            reproduction("echo", "count", &json!({}), &schema),
            "mcpdial call echo count"
        );
        // A name a shell would read as more than one word is quoted too.
        assert_eq!(
            reproduction("my server", "count", &json!({}), &schema),
            "mcpdial call 'my server' count"
        );
    }

    /// The guarantee the whole module rests on: with nobody to ask, it refuses
    /// rather than waits. `Plain` is what every pipe, every `--json` and every
    /// `--plain` gets, and it answers no.
    #[test]
    fn a_program_is_never_offered_a_list() {
        assert!(!crate::present::Plain.asks());
    }
}
