//! Where every line for a person or a program goes out.
//!
//! [`Plain`] is the agent surface: the bytes a pipe has always received, and
//! the bytes `tests/contract.rs` holds it to. [`Rich`] is what a person sees at
//! a terminal, and the only place that may differ from `Plain`. [`choose`] is
//! the one decision between them; nothing else in the binary looks at the
//! terminal to decide how to print.
//!
//! [`choose`]: dyn Presenter::choose

use crate::{Cli, Failure};
use mcpdial::catalog;
use mcpdial::session::ResourceBody;
use mcpdial::Error;
use serde_json::Value;
use std::io::Write;

#[cfg(feature = "rich")]
mod json;
#[cfg(feature = "rich")]
mod style;

/// Set (to anything but `0`) to get what a pipe would get, even at a terminal.
pub const ENV_PLAIN: &str = "MCPDIAL_PLAIN";

pub trait Presenter {
    /// Text on stdout, as given.
    fn out(&self, text: &str);
    /// One line on stdout.
    fn line(&self, line: &str);
    /// Text on stderr, as given, flushed: a prompt waiting for an answer.
    fn err(&self, text: &str);
    /// One line on stderr.
    fn err_line(&self, line: &str);
    /// Bytes on stdout, flushed.
    fn bytes(&self, bytes: &[u8]) -> Result<(), Error>;

    /// A JSON document, already serialized: one line, or pretty-printed.
    fn json(&self, text: &str) {
        self.line(text);
    }

    /// Text a server sent: a tool result, a prompt's messages.
    fn text(&self, text: &str) {
        self.line(text);
    }

    /// A resource's bodies on stdout, byte for byte. `redirect` is the command
    /// that produced them, for a refusal to name.
    fn resource(&self, bodies: &[ResourceBody], _redirect: &str) -> Result<(), Failure> {
        for body in bodies {
            let bytes = match body {
                ResourceBody::Text(t) => t.as_bytes(),
                ResourceBody::Bytes(b) => b.as_slice(),
            };
            self.bytes(bytes)?;
        }
        Ok(())
    }

    /// A count of something still being fetched, replacing the last one. A
    /// program gets nothing: the number is company for a person waiting.
    fn progress(&self, _text: &str) {}

    /// The fetch is over; whatever `progress` left on the line is finished.
    fn progress_end(&self) {}

    fn table(&self, headers: &[&str], rows: &[Vec<String>]) {
        let cols = headers.len();
        let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
        for row in rows {
            for (i, cell) in row.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let line = |cells: Vec<&str>| {
            let mut out = String::new();
            for (i, c) in cells.iter().enumerate() {
                if i + 1 == cols {
                    out.push_str(c);
                } else {
                    out.push_str(&format!("{:<w$}  ", c, w = widths[i]));
                }
            }
            out.trim_end().to_string()
        };
        self.line(&line(headers.to_vec()));
        for row in rows {
            self.line(&line(row.iter().map(String::as_str).collect()));
        }
    }

    /// Tools and prompts list identically; only the word for what they take
    /// differs, `describe` knows how to read it, and `hints` says what the item's
    /// own metadata adds after its name, which for a prompt is nothing.
    fn named(
        &self,
        items: &[Value],
        long: bool,
        takes: &str,
        describe: &dyn Fn(&Value) -> Vec<String>,
        hints: &dyn Fn(&Value) -> String,
    ) {
        for item in items {
            // `tools --all` marks what the allow and deny lists hide.
            let marker = if item["denied"] == true {
                " (denied)"
            } else {
                ""
            };
            let name = format!(
                "{}{}{marker}",
                item["name"].as_str().unwrap_or("?"),
                hints(item)
            );
            let desc = item["description"].as_str().unwrap_or("").trim();
            if long {
                self.line(&name);
                for line in desc.lines() {
                    self.line(&format!("    {}", line.trim_end()));
                }
                let params = describe(item);
                if !params.is_empty() {
                    self.line(&format!("  {takes}:"));
                    for p in params {
                        self.line(&format!("    {p}"));
                    }
                }
                self.line("");
            } else {
                let first = desc.lines().next().unwrap_or("");
                self.line(&format!("  {name:<28} {}", truncate_at(first, 90)));
            }
        }
    }

    /// Resources and templates print the same way; only the key holding the URI differs.
    fn resources(&self, resources: &[Value], long: bool) {
        for r in resources {
            let uri = r["uri"]
                .as_str()
                .or_else(|| r["uriTemplate"].as_str())
                .unwrap_or("?");
            let desc = r["description"].as_str().unwrap_or("").trim();
            if long {
                self.line(uri);
                for line in desc.lines() {
                    self.line(&format!("    {}", line.trim_end()));
                }
                if let Some(mime) = r["mimeType"].as_str() {
                    self.line(&format!("    type: {mime}"));
                }
                self.line("");
            } else {
                let summary = match desc.is_empty() {
                    true => r["name"].as_str().unwrap_or(""),
                    false => desc.lines().next().unwrap_or(""),
                };
                self.line(&format!("  {uri:<44} {}", truncate_at(summary, 74)));
            }
        }
    }

    /// The catalog as a person reads it: one block per category, one line per entry.
    fn catalog(&self, entries: &[catalog::Entry]) {
        let width = |pick: fn(&catalog::Entry) -> &str| {
            entries
                .iter()
                .map(|e| pick(e).chars().count())
                .max()
                .unwrap_or(0)
        };
        let (id_w, name_w) = (width(|e| &e.id), width(|e| &e.name));
        for (i, (category, group)) in catalog::grouped(entries).iter().enumerate() {
            if i > 0 {
                self.line("");
            }
            self.line(category);
            for e in group {
                self.line(&format!(
                    "  {:<id_w$}  {:<name_w$}  {:<5}  {:<7}  {}",
                    e.id,
                    e.name,
                    e.transport.as_str(),
                    e.auth.as_str(),
                    e.summary
                ));
            }
        }
    }

    /// A note on stderr: `note:` before the first line, the rest indented under it.
    fn note(&self, note: &str) {
        let mut lines = note.lines();
        if let Some(first) = lines.next() {
            self.err_line(&format!("note: {first}"));
        }
        for line in lines {
            self.err_line(&format!("      {line}"));
        }
    }

    /// A command that failed, and the hint that spells out what was expected.
    fn error(&self, message: &str, hint: Option<&str>) {
        self.err_line(&format!("error: {message}"));
        if let Some(hint) = hint {
            self.err_line(hint);
        }
    }

    /// Stdout from here to `page_end` is one piece of output that may run past
    /// the screen: a tool's result, a schema. A program gets every line as it
    /// comes; a person gets a pager when it is taller than the terminal.
    fn page_start(&self) {}

    fn page_end(&self) {}
}

impl dyn Presenter {
    /// The one decision: `Rich` only for a person at a terminal who asked for
    /// nothing else. `--json`, `--plain`, `MCPDIAL_PLAIN`, a dumb terminal or a
    /// pipe each mean `Plain`, and so does a build without the `rich` feature.
    pub fn choose(cli: &Cli) -> Box<dyn Presenter> {
        if wants_plain(cli) {
            Box::new(Plain)
        } else {
            rich(cli)
        }
    }
}

impl dyn Presenter + '_ {
    /// Runs `body` as one piece of output that may run past the screen; see
    /// [`Presenter::page_start`]. The section ends however `body` returns.
    pub fn paged<T>(&self, body: impl FnOnce() -> T) -> T {
        self.page_start();
        let result = body();
        self.page_end();
        result
    }
}

fn wants_plain(cli: &Cli) -> bool {
    use std::io::IsTerminal;
    let by_env = std::env::var(ENV_PLAIN).is_ok_and(|v| !v.is_empty() && v != "0");
    let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
    cli.json || cli.plain || by_env || dumb || !std::io::stdout().is_terminal()
}

#[cfg(feature = "rich")]
fn rich(cli: &Cli) -> Box<dyn Presenter> {
    Box::new(Rich {
        no_pager: cli.no_pager,
        ..Rich::default()
    })
}

/// Without the feature there is nothing but `Plain` to choose.
#[cfg(not(feature = "rich"))]
fn rich(_cli: &Cli) -> Box<dyn Presenter> {
    Box::new(Plain)
}

/// Today's bytes, exactly: what a pipe, a program and an agent receive.
pub struct Plain;

impl Presenter for Plain {
    fn out(&self, text: &str) {
        print!("{text}");
    }

    fn line(&self, line: &str) {
        println!("{line}");
    }

    fn err(&self, text: &str) {
        eprint!("{text}");
        std::io::stderr().flush().ok();
    }

    fn err_line(&self, line: &str) {
        eprintln!("{line}");
    }

    fn bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)
            .map_err(|e| Error::transport(format!("writing to stdout: {e}")))?;
        out.flush().ok();
        Ok(())
    }
}

/// What a person sees at a terminal. The same as [`Plain`] so far, except
/// where a terminal has always been treated differently: control characters in
/// a server's text are shown as escapes, binary resource bodies are refused,
/// and a long fetch counts on stderr as it goes.
#[cfg(feature = "rich")]
#[derive(Default)]
pub struct Rich {
    progressing: std::cell::Cell<bool>,
    /// `--no-pager`.
    no_pager: bool,
    /// What the section since `page_start` has printed, until `page_end`
    /// decides where it goes.
    page: std::cell::RefCell<Option<Vec<u8>>>,
}

#[cfg(feature = "rich")]
impl Presenter for Rich {
    fn out(&self, text: &str) {
        if !self.held(text.as_bytes()) {
            Plain.out(text);
        }
    }

    fn line(&self, line: &str) {
        if self.held(line.as_bytes()) {
            self.held(b"\n");
        } else {
            Plain.line(line);
        }
    }

    fn err(&self, text: &str) {
        Plain.err(text);
    }

    fn err_line(&self, line: &str) {
        Plain.err_line(line);
    }

    fn bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.held(bytes) {
            Ok(())
        } else {
            Plain.bytes(bytes)
        }
    }

    fn page_start(&self) {
        self.page.borrow_mut().get_or_insert_with(Vec::new);
    }

    /// The section's output goes to the pager when there is one for it, and
    /// straight out when there is not, or when the pager cannot be started.
    fn page_end(&self) {
        let Some(page) = self.page.borrow_mut().take() else {
            return;
        };
        let screen = terminal_size();
        let rows = rows_on(&String::from_utf8_lossy(&page), screen.map(|s| s.0));
        let pager = pager_for(rows, screen.map(|s| s.1), self.no_pager, |name| {
            std::env::var(name).ok()
        });
        if pager.is_some_and(|pager| run_pager(&pager, &page)) {
            return;
        }
        Plain.bytes(&page).ok();
    }

    /// A pretty-printed document gets its keys, strings, numbers, booleans and
    /// null in colour; the layout is untouched.
    fn json(&self, text: &str) {
        match self.highlighted(text) {
            Some(painted) => self.line(&painted),
            None => self.line(text),
        }
    }

    /// The control characters that move the cursor or open an escape sequence
    /// are shown as escapes: a server that lists `\r` among its valid keys
    /// otherwise overwrites the start of its own error message. Newlines and
    /// tabs are the text's own layout and stay. Text that is a pretty-printed
    /// JSON document, as `structuredContent` is rendered, is coloured like one.
    fn text(&self, text: &str) {
        match self.highlighted(text) {
            Some(painted) => self.line(&painted),
            None => self.line(&visible(text)),
        }
    }

    /// Base64 is no use to anyone reading it and raw bytes corrupt a terminal,
    /// so binary asks for a redirect rather than picking one of those two ways
    /// to be useless.
    fn resource(&self, bodies: &[ResourceBody], redirect: &str) -> Result<(), Failure> {
        if bodies.iter().any(|b| matches!(b, ResourceBody::Bytes(_))) {
            return Err(Failure::hinted(
                Error::usage("this resource is binary and stdout is a terminal"),
                format!(
                    "send it somewhere it can land: {redirect} > file, or {redirect} --save-dir DIR"
                ),
            ));
        }
        Plain.resource(bodies, redirect)
    }

    fn progress(&self, text: &str) {
        self.err(&format!("\r{text}"));
        self.progressing.set(true);
    }

    fn progress_end(&self) {
        if self.progressing.replace(false) {
            self.err_line("");
        }
    }
}

#[cfg(feature = "rich")]
impl Rich {
    /// `text` in colour when colour is wanted and the text is a pretty-printed
    /// JSON document; `None` says to print it as it is.
    fn highlighted(&self, text: &str) -> Option<String> {
        style::wanted().then(|| json::highlighted(text)).flatten()
    }
}

#[cfg(feature = "rich")]
fn visible(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => write!(out, "\\x{:02x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out
}

#[cfg(feature = "rich")]
impl Rich {
    /// Keeps `bytes` for the page being collected; false when none is, and
    /// they are to go straight out.
    fn held(&self, bytes: &[u8]) -> bool {
        match self.page.borrow_mut().as_mut() {
            Some(page) => {
                page.extend_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

/// The pager for output `rows` tall, in git's order: none under `--no-pager`,
/// none where the screen's height is unknown (a pipe, or a console that will
/// not say), none when the output fits above the prompt that follows it; else
/// `MCPDIAL_PAGER`, else `PAGER`, else `less -RFX`. Either variable set but
/// empty, or set to `cat`, means none too.
#[cfg(feature = "rich")]
fn pager_for(
    rows: usize,
    screen_rows: Option<usize>,
    no_pager: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    const DEFAULT_PAGER: &str = "less -RFX";
    if no_pager || screen_rows.is_none_or(|screen| rows < screen) {
        return None;
    }
    let pager = env("MCPDIAL_PAGER")
        .or_else(|| env("PAGER"))
        .unwrap_or_else(|| DEFAULT_PAGER.to_string());
    let pager = pager.trim().to_string();
    (!pager.is_empty() && pager != "cat").then_some(pager)
}

/// The rows `text` takes on a screen `cols` wide, counting the wrapping of
/// long lines and not the SGR sequences that colour them. Without a width,
/// one row per line.
#[cfg(feature = "rich")]
fn rows_on(text: &str, cols: Option<usize>) -> usize {
    text.lines()
        .map(|line| match cols {
            Some(cols) if cols > 0 => visible_width(line).max(1).div_ceil(cols),
            _ => 1,
        })
        .sum()
}

/// Characters a line puts on the screen: every one but those inside an
/// `ESC [ ... m` sequence.
#[cfg(feature = "rich")]
fn visible_width(line: &str) -> usize {
    let mut width = 0;
    let mut in_sgr = false;
    for c in line.chars() {
        match (in_sgr, c) {
            (false, '\x1b') => in_sgr = true,
            (false, _) => width += 1,
            (true, 'm') => in_sgr = false,
            (true, _) => {}
        }
    }
    width
}

/// Runs `pager` on `page` and waits for it to exit, so that a prompt after it
/// comes after it. False when it could not be started, in which case nothing
/// has been printed. A command with no shell syntax in it is started directly,
/// so that one that is not installed is found out here and not by a shell
/// that would say so on stderr; anything else goes through the shell, as git
/// does.
#[cfg(feature = "rich")]
fn run_pager(pager: &str, page: &[u8]) -> bool {
    use std::process::{Command, Stdio};
    let mut command = match shell_command(pager) {
        Some(command) => command,
        None => {
            let mut words = pager.split_whitespace();
            let Some(program) = words.next() else {
                return false;
            };
            let mut command = Command::new(program);
            command.args(words);
            command
        }
    };
    command.stdin(Stdio::piped());
    // git sets the same, so that a bare `PAGER=less` keeps colour and stays
    // on screen after it quits.
    if std::env::var_os("LESS").is_none() {
        command.env("LESS", "FRX");
    }
    std::io::stdout().flush().ok();
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A pager quit before the end closes its side; that is the reader
        // done, not a failure.
        stdin.write_all(page).ok();
    }
    child.wait().ok();
    true
}

/// `pager` under the shell, when it has syntax that needs one: git's own test.
#[cfg(feature = "rich")]
fn shell_command(pager: &str) -> Option<std::process::Command> {
    const SHELL_SYNTAX: &[char] = &[
        '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', '#', '~', '=',
        '%',
    ];
    if !pager.contains(SHELL_SYNTAX) {
        return None;
    }
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut command = std::process::Command::new(shell);
    command.arg(flag).arg(pager);
    Some(command)
}

/// The terminal on stdout as (columns, rows), when it can be asked. The
/// `ioctl` is declared here rather than through a crate: std links libc
/// already, and this is the one call needed.
#[cfg(all(feature = "rich", unix))]
fn terminal_size() -> Option<(usize, usize)> {
    use std::os::raw::{c_int, c_ulong};
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        x_pixels: u16,
        y_pixels: u16,
    }
    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    let mut size = WinSize {
        rows: 0,
        cols: 0,
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: TIOCGWINSZ fills one `struct winsize`, which `WinSize` lays out
    // as C does, and touches nothing else.
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut size as *mut WinSize) };
    (rc == 0 && size.rows > 0).then_some((size.cols as usize, size.rows as usize))
}

/// Linux numbers its ioctls; the BSDs, and Darwin with them, encode the size
/// of the struct in the request.
#[cfg(all(
    feature = "rich",
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
#[cfg(all(feature = "rich", any(target_os = "solaris", target_os = "illumos")))]
const TIOCGWINSZ: std::os::raw::c_ulong = 0x5468;
#[cfg(all(
    feature = "rich",
    unix,
    not(any(target_os = "solaris", target_os = "illumos")),
    any(
        not(any(target_os = "linux", target_os = "android")),
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )
))]
const TIOCGWINSZ: std::os::raw::c_ulong = 0x4008_7468;

/// A console's height is not asked for, so nothing is paged on Windows.
#[cfg(all(feature = "rich", not(unix)))]
fn terminal_size() -> Option<(usize, usize)> {
    None
}

pub fn truncate_at(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n - 3).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A presenter that keeps what it was given, so the renderings can be
    /// checked without a process.
    #[derive(Default)]
    struct Kept {
        out: std::cell::RefCell<String>,
        err: std::cell::RefCell<String>,
    }

    impl Presenter for Kept {
        fn out(&self, text: &str) {
            self.out.borrow_mut().push_str(text);
        }
        fn line(&self, line: &str) {
            self.out(line);
            self.out("\n");
        }
        fn err(&self, text: &str) {
            self.err.borrow_mut().push_str(text);
        }
        fn err_line(&self, line: &str) {
            self.err(line);
            self.err("\n");
        }
        fn bytes(&self, bytes: &[u8]) -> Result<(), Error> {
            self.out(&String::from_utf8_lossy(bytes));
            Ok(())
        }
    }

    #[test]
    fn a_table_pads_every_column_but_the_last() {
        let p = Kept::default();
        p.table(
            &["NAME", "TYPE", "LOCATION"],
            &[
                vec!["a".into(), "http".into(), "https://x".into()],
                vec!["longer".into(), "stdio".into(), "".into()],
            ],
        );
        assert_eq!(
            p.out.borrow().as_str(),
            "NAME    TYPE   LOCATION\na       http   https://x\nlonger  stdio\n"
        );
    }

    #[test]
    fn notes_and_errors_keep_their_shape_on_stderr() {
        let p = Kept::default();
        p.note("first\nsecond");
        p.error("no", Some("try this"));
        p.error("alone", None);
        assert_eq!(
            p.err.borrow().as_str(),
            "note: first\n      second\nerror: no\ntry this\nerror: alone\n"
        );
    }

    #[test]
    fn a_pipe_gets_a_resource_whatever_it_holds_and_no_progress() {
        let p = Kept::default();
        let bodies = [
            ResourceBody::Text("text ".into()),
            ResourceBody::Bytes(b"bytes".to_vec()),
        ];
        assert!(p.resource(&bodies, "mcpdial read x y").is_ok());
        assert_eq!(p.out.borrow().as_str(), "text bytes");
        p.progress("counting");
        p.progress_end();
        assert_eq!(p.err.borrow().as_str(), "");
    }

    #[cfg(feature = "rich")]
    #[test]
    fn visible_escapes_what_would_move_the_cursor_and_keeps_layout() {
        assert_eq!(
            visible(
                "Error: k is invalid. Valid keys are: Enter,\r,\n,ShiftLeft,\0,\x1b[0m,\u{9b}\tend"
            ),
            "Error: k is invalid. Valid keys are: Enter,\\r,\n,ShiftLeft,\\0,\\x1b[0m,\\x9b\tend"
        );
        assert_eq!(
            visible("plain text\nsecond line"),
            "plain text\nsecond line"
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_terminal_refuses_bytes_and_names_the_redirect() {
        let bodies = [ResourceBody::Bytes(b"\x89PNG".to_vec())];
        let f = Rich::default()
            .resource(&bodies, "mcpdial read x y")
            .unwrap_err();
        assert!(matches!(f.error, Error::Usage(_)));
        assert_eq!(
            f.hint.as_deref(),
            Some("send it somewhere it can land: mcpdial read x y > file, or mcpdial read x y --save-dir DIR")
        );
        assert!(Rich::default()
            .resource(&[ResourceBody::Text("ok".into())], "mcpdial read x y")
            .is_ok());
    }

    #[cfg(feature = "rich")]
    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[cfg(feature = "rich")]
    #[test]
    fn output_is_paged_only_when_it_and_the_prompt_after_it_overflow_the_screen() {
        let none = env_of(&[]);
        assert_eq!(pager_for(10, Some(40), false, &none), None);
        assert_eq!(pager_for(39, Some(40), false, &none), None);
        assert_eq!(
            pager_for(40, Some(40), false, &none).as_deref(),
            Some("less -RFX")
        );
        assert_eq!(
            pager_for(500, Some(40), false, &none).as_deref(),
            Some("less -RFX")
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_pipe_and_no_pager_never_page() {
        let none = env_of(&[]);
        assert_eq!(pager_for(500, None, false, &none), None);
        assert_eq!(pager_for(500, Some(40), true, &none), None);
    }

    #[cfg(feature = "rich")]
    #[test]
    fn the_pager_variables_are_read_in_gits_order_and_an_empty_one_disables() {
        assert_eq!(
            pager_for(500, Some(40), false, env_of(&[("PAGER", "more")])).as_deref(),
            Some("more")
        );
        assert_eq!(
            pager_for(
                500,
                Some(40),
                false,
                env_of(&[("MCPDIAL_PAGER", "bat -p"), ("PAGER", "more")])
            )
            .as_deref(),
            Some("bat -p")
        );
        assert_eq!(
            pager_for(
                500,
                Some(40),
                false,
                env_of(&[("MCPDIAL_PAGER", ""), ("PAGER", "more")])
            ),
            None
        );
        assert_eq!(
            pager_for(500, Some(40), false, env_of(&[("PAGER", "")])),
            None
        );
        assert_eq!(
            pager_for(500, Some(40), false, env_of(&[("PAGER", "cat")])),
            None
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn rows_count_wrapped_lines_and_not_colour() {
        assert_eq!(rows_on("", Some(80)), 0);
        assert_eq!(rows_on("one\ntwo\n", Some(80)), 2);
        assert_eq!(rows_on("one\ntwo", Some(80)), 2);
        assert_eq!(rows_on("one\n\nthree\n", Some(80)), 3);
        assert_eq!(rows_on(&"x".repeat(81), Some(80)), 2);
        assert_eq!(rows_on(&"x".repeat(160), Some(80)), 2);
        assert_eq!(rows_on(&"x".repeat(161), Some(80)), 3);
        assert_eq!(rows_on(&"x".repeat(1000), None), 1);
        assert_eq!(
            rows_on(&format!("\x1b[1m{}\x1b[0m", "x".repeat(80)), Some(80)),
            1
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_pager_with_shell_syntax_gets_a_shell_and_a_plain_one_does_not() {
        assert!(shell_command("less -RFX").is_none());
        assert!(shell_command("bat -p").is_none());
        assert!(shell_command("sed s/a/b/ | less").is_some());
        assert!(shell_command("less --pattern='x'").is_some());
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_page_holds_stdout_until_it_ends_and_nothing_outside_it() {
        let rich = Rich::default();
        assert!(!rich.held(b"outside"));
        rich.page_start();
        rich.line("one");
        rich.out("two ");
        rich.text("three\r");
        rich.bytes(b"four").unwrap();
        // Taken here rather than through `page_end`, which would print it.
        assert_eq!(
            rich.page.borrow_mut().take().as_deref(),
            Some(b"one\ntwo three\\r\nfour".as_slice())
        );
        assert!(!rich.held(b"outside"));
    }
}
