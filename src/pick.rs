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
//! `mcpdial` under a pipe gets clap's usage error and exit 2, exactly as it
//! always has, and [`bare`] is where that is settled.
//!
//! With nothing saved there is nothing to pick, and a first run gets
//! [`welcome`] instead: a few lines saying what mcpdial is and how to get a
//! server into it, and the prompt back. The catalog is one of the ways it
//! names, offered as a command rather than sprung as a full-screen checklist
//! that has to be fetched before it can be drawn.
//!
//! [`fzf`] and [`numbered`] are the two halves of picking one of a list, and
//! they are shared: `fzf` when it is on `PATH`, else the list numbered and
//! answered by its number or by the line itself. `prompt.rs` picks the values
//! an `enum` allows with the same two.

use crate::diagnose::shell_word;
use crate::media::MediaFiles;
use crate::notices::Notices;
use crate::output::Output;
use crate::present::Presenter;
use crate::{args, prompt, Failure, EXIT_ERROR};
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

/// What a bare `mcpdial` says. Nothing here dials anything: that is the whole
/// point of it, and [`client::saved`] is chosen over [`client::listing`] for
/// exactly that reason.
pub fn welcome(ui: &dyn Presenter, store: &Store) -> Result<(), Failure> {
    ui.line(ABOUT);
    let saved = client::saved(store)?;
    let Some(first) = saved.first() else {
        ui.line(NOTHING_SAVED);
        return Ok(());
    };
    let counted = match saved.len() {
        1 => "1 server saved, as the last dial left it".to_string(),
        n => format!("{n} servers saved, as the last dial left them"),
    };
    ui.line(&format!("\n{counted}:\n"));
    let rows: Vec<Vec<String>> = saved.iter().map(row).collect();
    ui.table(&SAVED_HEADERS, &rows);
    ui.line(&format!(
        "\nwhat next:\n  \
         mcpdial pick          # pick a server and a tool, and make the call\n  \
         mcpdial ls            # dial them all for a status that is current\n  \
         mcpdial tools {:<8}# what one server offers\n\n\
         `mcpdial --help` lists every command.",
        shell_word(&first.name)
    ));
    Ok(())
}

/// What mcpdial is, in the two lines someone who has just installed it needs.
const ABOUT: &str = "\
mcpdial dials MCP servers from the shell: what a server offers, a call to one of
its tools, and a name to keep it under.";

/// The rest of a first run: the three ways to get a server, and what to do once
/// there is one.
const NOTHING_SAVED: &str = "
No servers are saved yet.

getting started:
  mcpdial import        # the servers Claude, Cursor and VS Code already have
  mcpdial browse        # tick what you want from a reviewed catalog
  mcpdial add wiki --http https://mcp.deepwiki.com/mcp

Then `mcpdial ls` says what is saved and whether it answers, and `mcpdial pick`
picks a server and a tool and makes the call. `mcpdial --help` lists every
command.";

const SAVED_HEADERS: [&str; 4] = ["NAME", "TYPE", "STATUS", "CHECKED"];

fn row(s: &client::Saved) -> Vec<String> {
    let (status, checked) = match &s.last {
        Some((status, age)) => (status.label(), checked_label(*age)),
        None => ("-".into(), "never".into()),
    };
    vec![s.name.clone(), s.kind.into(), status, checked]
}

/// How long ago a probe ran. `ls` counts in minutes because what it shows was
/// taken seconds ago; this column may be showing a status from months back, and
/// the point of it is how much salt to take that with.
fn checked_label(seconds: u64) -> String {
    match seconds {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
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
    let (columns, lines) = server_lines(&servers);
    let Some(at) = chooser.one(
        ui,
        &Offer {
            what: "server",
            about: "Pick a server, then a tool, then its arguments, and the call is made.",
            filters: "name",
            columns,
            search: Search::Name,
        },
        &lines,
    )?
    else {
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
    let (columns, lines) = tool_lines(&tools);
    let Some(at) = chooser.one(
        ui,
        &Offer {
            what: "tool",
            about: "Pick a tool. Its arguments come next, from the schema the server sent.",
            filters: "name and description",
            columns,
            search: Search::Everything,
        },
        &lines,
    )?
    else {
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

/// Leaving the picker is where someone who did not mean to open it ends up,
/// so it is the one place that says what the rest of the program is.
fn nothing_picked(ui: &dyn Presenter) -> u8 {
    ui.err_line("nothing picked");
    ui.err_line("`mcpdial --help` lists the commands; `mcpdial ls` names the saved servers.");
    0
}

/// One line per saved server: its name, the status the last probe left, how it
/// is dialed, and how many tools it answered with, under the same column names
/// `ls` prints them under.
fn server_lines(servers: &[Listing]) -> (String, Vec<String>) {
    columns(
        &["NAME", "STATUS", "TYPE", "TOOLS"],
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
fn tool_lines(tools: &[Value]) -> (String, Vec<String>) {
    columns(
        &["NAME", "DESCRIPTION"],
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

/// The header and the rows, every column but the last padded to its widest
/// cell. The header is measured with the rows rather than after them, so a
/// column name sits over the column it names however wide the cells under it
/// turn out to be.
fn columns(headers: &[&str], rows: Vec<Vec<String>>) -> (String, Vec<String>) {
    let mut all = vec![headers.iter().map(|h| (*h).to_string()).collect()];
    all.extend(rows);
    let mut widths: Vec<usize> = Vec::new();
    for row in &all {
        for (i, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(i) {
                Some(widest) => *widest = (*widest).max(width),
                None => widths.push(width),
            }
        }
    }
    let mut padded: Vec<String> = all
        .iter()
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
        .collect();
    (padded.remove(0), padded)
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
        offer: &Offer<'_>,
        lines: &[String],
    ) -> Result<Option<usize>, Failure> {
        match fzf(offer, lines) {
            Fzf::Picked(answer) => return Ok(lines.iter().position(|line| *line == answer)),
            Fzf::Quit => return Ok(None),
            Fzf::Absent => {}
        }
        // Answered by number rather than by filtering, so it is told that
        // instead of what `fzf`'s header says.
        ui.err_line(offer.about);
        ui.err_line("Answer with a number, or ^D to leave.");
        // Every row is offered behind its number, right-aligned so that the
        // tenth row starts where the first one does and the column names sit
        // over the column they name.
        let digits = lines.len().to_string().chars().count();
        ui.err_line(&format!("{}{}", " ".repeat(digits + 4), offer.columns));
        for (i, line) in lines.iter().enumerate() {
            ui.err_line(&format!("  {:>digits$}) {line}", i + 1));
        }
        let question = format!("{} (1-{}): ", offer.what, lines.len());
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
                None => ui.err_line(&format!(
                    "  {} is one of the {} above",
                    offer.what,
                    lines.len()
                )),
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

/// One question the picker asks: what it is picking, what a person needs told
/// before they can answer, the column names the lines sit under, and how much
/// of a line a query is matched against.
pub struct Offer<'a> {
    /// The word the prompt and the numbered question are built from.
    pub what: &'a str,
    /// What is being picked and what picking it does. A bare `mcpdial` opens
    /// straight onto this list with nothing said yet, so it is said here.
    pub about: &'a str,
    /// What a typed query is matched against, named for `fzf`'s header. Only
    /// `fzf` filters; the numbered list is answered by number, and telling it
    /// to type a filter would be telling it the wrong thing.
    pub filters: &'a str,
    /// The column names, padded to sit over the columns they name.
    pub columns: String,
    pub search: Search,
}

impl Offer<'_> {
    /// What sits above `fzf`'s list: what is being picked, what filters it,
    /// and the column names where there are any to print.
    fn header(&self) -> String {
        let mut lines = vec![self.about.to_string()];
        if !self.filters.is_empty() {
            lines.push(format!(
                "Type to filter by {}. Enter picks, Esc leaves.",
                self.filters
            ));
        }
        if !self.columns.is_empty() {
            lines.push(self.columns.clone());
        }
        lines.join("\n")
    }
}

/// How much of a line a typed query is matched against.
#[derive(Clone, Copy)]
pub enum Search {
    /// The name in the first column and nothing else. A server line carries a
    /// status, a transport and a tool count beside the name, and those are
    /// words every row shares: without this, `ect` finds three servers inside
    /// the `connected` they all say.
    Name,
    /// The whole line. A tool's line is its name and the first thing its
    /// description says, and what a tool is for is half of what there is to
    /// search it by.
    Everything,
}

/// `fzf` over the lines, when it is on the PATH. It is the fuzzy picker the
/// people who want one already have, which is why there is no crate here.
pub fn fzf(offer: &Offer<'_>, choices: &[String]) -> Fzf {
    let Some(program) = on_path("fzf") else {
        return Fzf::Absent;
    };
    let mut command = Command::new(program);
    command
        .arg(format!("--prompt={}> ", offer.what))
        .arg("--height=40%")
        .arg("--reverse")
        .arg(format!("--header={}", offer.header()));
    if let Search::Name = offer.search {
        // fzf reads `--nth` against whitespace-separated fields, and a saved
        // name is one word by the rule that saved it.
        command.arg("--nth=1");
    }
    let started = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn();
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
        let (columns, rows) = server_lines(&[
            listing("echo", Status::Connected, Some(5)),
            listing("work", Status::AuthRequired, None),
        ]);
        assert_eq!(rows[0], "echo  connected      stdio  5 tools");
        assert_eq!(rows[1], "work  auth required  stdio  -");
        // The column names are measured with the rows, so each one starts
        // where the cells it names start.
        assert_eq!(columns, "NAME  STATUS         TYPE   TOOLS");
        for row in [&columns, &rows[0], &rows[1]] {
            assert_eq!(row.find("stdio").or(row.find("TYPE")), Some(21));
        }
    }

    #[test]
    fn a_tool_line_is_the_name_and_the_first_line_of_what_it_is_for() {
        let (columns, rows) = tool_lines(&[
            json!({"name": "echo", "description": "Echo a message back.\nThe rest is detail."}),
            json!({"name": "count"}),
        ]);
        assert_eq!(rows[0], "echo   Echo a message back.");
        assert_eq!(rows[1], "count");
        assert_eq!(columns, "NAME   DESCRIPTION");
    }

    /// The bug this picker had: a server line ends in words every other line
    /// also ends in, so `ect` matched all three servers inside the `connected`
    /// they share. `Search::Name` narrows fzf to `--nth=1`, which is the first
    /// whitespace-separated field, so what that field holds is the whole fix.
    #[test]
    fn the_field_fzf_is_narrowed_to_holds_the_name_and_nothing_else() {
        let (columns, rows) = server_lines(&[
            listing("chrome", Status::Connected, Some(29)),
            listing("playwright", Status::AuthRequired, None),
        ]);
        for (row, name) in rows.iter().zip(["chrome", "playwright"]) {
            // Every row does carry the words that used to catch it, and none
            // of them is in the field a query is now matched against.
            assert!(row.contains("stdio"), "{row}");
            assert_eq!(row.split_whitespace().next(), Some(name), "{row}");
        }
        assert_eq!(columns.split_whitespace().next(), Some("NAME"));
    }

    /// What a bare `mcpdial` puts above the list, which is the whole of what it
    /// says before someone has to answer it.
    #[test]
    fn the_header_says_what_is_picked_what_filters_it_and_what_the_columns_are() {
        let offer = Offer {
            what: "server",
            about: "Pick a server.",
            filters: "name",
            columns: "NAME  STATUS".to_string(),
            search: Search::Name,
        };
        assert_eq!(
            offer.header(),
            "Pick a server.\nType to filter by name. Enter picks, Esc leaves.\nNAME  STATUS"
        );
        // A list with no columns to name does not leave a blank line where
        // they would have gone.
        let bare = Offer {
            columns: String::new(),
            filters: "",
            ..offer
        };
        assert_eq!(bare.header(), "Pick a server.");
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
