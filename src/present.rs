//! Where every line for a person or a program goes out.
//!
//! [`Plain`] is the agent surface: the bytes a pipe has always received, and
//! the bytes `tests/contract.rs` holds it to. [`Rich`] is what a person sees at
//! a terminal, and the only place that may differ from `Plain`. [`choose`] is
//! the one decision between them; nothing else in the binary looks at the
//! terminal to decide how to print.
//!
//! [`choose`]: dyn Presenter::choose

use crate::cli::Cli;
use crate::Failure;
use mcpdial::catalog;
use mcpdial::session::{Media, ResourceBody};
use mcpdial::Error;
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use style::ColorMode;

#[cfg(feature = "rich")]
mod image;
#[cfg(feature = "rich")]
mod json;
#[cfg(feature = "rich")]
mod markdown;
pub mod style;

/// Set (to anything but `0`) to get what a pipe would get, even at a terminal.
pub const ENV_PLAIN: &str = "MCPDIAL_PLAIN";

/// How a shell session is, as the dot in its prompt shows it.
///
/// The dot is a reminder rather than an announcement: each of these states is
/// also said once, in one dim line, at the moment it becomes true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Health {
    /// Connected, with nothing said against it.
    Fine,
    /// The saved token runs out within [`TOKEN_RUNNING_OUT`].
    Expiring,
    /// The transport failed; the next command dials again.
    Lost,
}

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

    /// A media block on its way to stdout, with the file its bytes landed in
    /// when one did. A terminal that can show the image keeps it here, to draw
    /// under the placeholder line when the text goes out; everywhere else,
    /// including every pipe, this is nothing at all.
    fn draw(&self, _media: &Media, _saved_to: Option<&Path>) {}

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
        for line in table_lines(headers, rows, &|_, cell| cell.to_string()) {
            self.line(&line);
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

    /// Whether there is a person on stdin to answer a question. A program is
    /// never asked one, whatever is on its stdin: a prompt into a pipe waits
    /// for an answer that is not coming, so a missing argument there stays the
    /// error it has always been.
    fn asks(&self) -> bool {
        false
    }

    /// Whether a person is watching this run go by, on both streams.
    ///
    /// The other half of [`asks`]. A question wants somebody to answer it,
    /// which is stdin and the stdout a prompt is drawn on; prose beside the
    /// answer - the updating progress line, a word about the session, the
    /// sentence an elicitation puts above its prompts - wants somebody to read
    /// it, which is that stdout and a stderr that is a screen rather than a
    /// log. `--color always` hands a pipe `Rich`, and nobody is watching a
    /// pipe, so being `Rich` does not settle this on its own.
    ///
    /// [`asks`]: Presenter::asks
    fn watched(&self) -> bool {
        false
    }

    /// Whether the shell's reader can be a line editor here: see
    /// [`a_line_can_be_edited`], which is the whole of the answer. Neither
    /// presenter gives a different one - `--plain` freezes the bytes, not the
    /// keyboard, and `MCPDIAL_PLAIN=1` at a terminal still gets Tab - but the
    /// question is about the terminal, so it is answered here with the rest.
    fn edits_lines(&self) -> bool {
        a_line_can_be_edited()
    }

    /// A line beside the output rather than part of it: what a prompt filled
    /// in, written as the command that would have said it outright. Dim at a
    /// terminal, since it is not the answer, only how to ask again.
    fn aside(&self, line: &str) {
        self.err_line(line);
    }

    /// What a shell session asks for its next line with. A redirected stdout
    /// has been given the server's name and an angle bracket since there was a
    /// shell, and a program cannot be shown a state, so this says nothing about
    /// one.
    fn shell_prompt(&self, label: &str, _health: Health) -> String {
        format!("{label}> ")
    }

    /// The number the shell filed a result under, before the result itself, so
    /// that `show`, `save` and `retry` have something to name. A program is
    /// told nothing: its bytes are the contract, and a number among them would
    /// change it.
    fn numbered(&self, _n: usize) {}

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
    /// The one decision: `Rich` for a person at a terminal who asked for
    /// nothing else, and for the pipe that asked for colour. `--json`,
    /// `--plain`, `MCPDIAL_PLAIN`, a dumb terminal or a pipe that said nothing
    /// each mean `Plain`, and so does a build without the `rich` feature.
    pub fn choose(cli: &Cli) -> Box<dyn Presenter> {
        // `--color always` is the one way a pipe gets `Rich`: colour is what
        // it asked for, and a pipe into `less -R` is where it wants it. It
        // does not overrule the four ways of asking for the piped bytes
        // outright, and it changes nothing for anyone else asking
        // `wants_plain` whether stdout is a person's.
        let plain = match cli.color {
            ColorMode::Always => asked_for_plain(cli),
            ColorMode::Never | ColorMode::Auto => wants_plain(cli),
        };
        if plain {
            Box::new(Plain)
        } else {
            // Being `Rich` settles stdout; stderr is the other stream colour
            // is decided for, and the only thing left to look at.
            use std::io::IsTerminal;
            rich(cli, std::io::stderr().is_terminal())
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

pub(crate) fn wants_plain(cli: &Cli) -> bool {
    use std::io::IsTerminal;
    asked_for_plain(cli) || !std::io::stdout().is_terminal()
}

/// Whether a line editor has both its ends. It draws its prompt on stdout and
/// takes keys from stdin, so a person has to be at each of them.
///
/// This is the fact [`Presenter::asks`] is built on, and on its own it is the
/// whole of what the shell's reader needs: what the bytes look like is the
/// other decision, and the two are not the same one. Reached from outside
/// through [`Presenter::edits_lines`].
fn a_line_can_be_edited() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// The four ways of asking for the piped bytes outright, whatever the stream.
fn asked_for_plain(cli: &Cli) -> bool {
    let by_env = std::env::var(ENV_PLAIN).is_ok_and(|v| !v.is_empty() && v != "0");
    let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
    cli.json || cli.plain || by_env || dumb
}

/// Colour is decided per stream: `2>log` at a terminal keeps escapes out of
/// the log. Colour on stdout takes no second look, since `Rich` at all means
/// either a terminal or the `--color always` that asked for the pipe - and
/// that pipe is exactly why `asks` and `watched` do look, because neither a
/// question nor a line of prose has anybody at the far end of one.
#[cfg(feature = "rich")]
fn rich(cli: &Cli, stderr_is_terminal: bool) -> Box<dyn Presenter> {
    use std::io::IsTerminal;
    Box::new(Rich {
        no_pager: cli.no_pager,
        color_out: style::color_enabled(cli.color, true),
        color_err: style::color_enabled(cli.color, stderr_is_terminal),
        image: image::protocol(|name| std::env::var(name).ok()),
        markdown: !cli.raw,
        // A question needs both ends of a line editor and bytes that are a
        // person's; `--color always` gets a pipe `Rich`, and a pipe can be
        // asked nothing.
        asks: a_line_can_be_edited(),
        // Prose beside the answer wants a screen at both ends instead: a
        // redirected stderr is a log file, and nobody reads a pipe's stdout.
        watched: std::io::stdout().is_terminal() && stderr_is_terminal,
        ..Rich::default()
    })
}

/// Without the feature there is nothing but `Plain` to choose.
#[cfg(not(feature = "rich"))]
fn rich(_cli: &Cli, _stderr_is_terminal: bool) -> Box<dyn Presenter> {
    Box::new(Plain)
}

/// A table's lines: every column but the last padded to its widest cell, and
/// `paint` given each body cell with its column once the width is measured, so
/// what it adds does not count.
fn table_lines(
    headers: &[&str],
    rows: &[Vec<String>],
    paint: &dyn Fn(usize, &str) -> String,
) -> Vec<String> {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>, paint: &dyn Fn(usize, &str) -> String| {
        let mut out = String::new();
        for (i, c) in cells.iter().enumerate() {
            out.push_str(&paint(i, c));
            if i + 1 < cols {
                out.push_str(&" ".repeat(widths[i] - c.chars().count() + 2));
            }
        }
        out.trim_end().to_string()
    };
    let mut lines = vec![line(headers.to_vec(), &|_, h| h.to_string())];
    lines.extend(
        rows.iter()
            .map(|row| line(row.iter().map(String::as_str).collect(), paint)),
    );
    lines
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

/// Erase from the cursor to the end of the line, so that a redraw does not
/// leave the tail of a longer one standing behind it.
#[cfg(feature = "rich")]
const ERASE: &str = "\x1b[K";

/// What a person sees at a terminal. The same as [`Plain`] so far, except
/// where a terminal has always been treated differently: control characters in
/// a server's text are shown as escapes, binary resource bodies are refused,
/// and a long fetch counts on stderr as it goes. With colour on, the status of
/// a listed server and the `error:` prefix carry their meaning in colour.
#[cfg(feature = "rich")]
#[derive(Default)]
pub struct Rich {
    progressing: std::cell::Cell<bool>,
    /// `--no-pager`.
    no_pager: bool,
    /// Whether SGR sequences go to stdout, and whether they go to stderr.
    color_out: bool,
    color_err: bool,
    /// Which escape sequence draws an image here, for the terminals that have
    /// one. `Presenter::choose` decides that stdout is a person's terminal;
    /// `image::protocol` decides only which terminal it is.
    image: Option<image::Protocol>,
    /// Each drawing waiting to go out, under the placeholder line it belongs
    /// to, in the order the result's blocks were rendered.
    drawings: std::cell::RefCell<Vec<(String, String)>>,
    /// Whether the section since `page_start` has an image in it.
    drew: std::cell::Cell<bool>,
    /// Whether a server's markdown is rendered rather than shown as written;
    /// `--raw` is what turns it off.
    markdown: bool,
    /// Whether a question can be put and answered: both ends a terminal. See
    /// [`rich`], which is the only place that decides it.
    asks: bool,
    /// Whether prose beside the answer has anybody to read it: stdout and
    /// stderr both a terminal. Decided in [`rich`] alongside `asks`.
    watched: bool,
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

    fn asks(&self) -> bool {
        self.asks
    }

    fn watched(&self) -> bool {
        self.watched
    }

    fn aside(&self, line: &str) {
        let dim = style::Style::new().dim().when(self.color_err);
        self.err_line(&dim.paint(line));
    }

    fn numbered(&self, n: usize) {
        self.aside(&format!("[{n}]"));
    }

    /// The status dot between the server's name and the bracket. Rustyline
    /// draws the prompt on stdout, so it is stdout's colour switch that decides
    /// whether the dot is painted; the glyph carries the same three states on
    /// its own, for the terminal where colour is off.
    fn shell_prompt(&self, label: &str, health: Health) -> String {
        let (dot, style) = health_dot(health);
        format!("{label} {} > ", style.when(self.color_out).paint(dot))
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
        // A drawing the section never printed - output that went to a file, or
        // a resource whose bodies leave raw - belongs to nothing after it.
        self.drawings.borrow_mut().clear();
        // A pager reads the page through a pipe and writes it back out its own
        // way, which no image escape survives, so a page with one in it goes
        // straight to the screen however tall it is.
        if !self.drew.replace(false) {
            let screen = screen();
            let rows = rows_on(&String::from_utf8_lossy(&page), screen.map(|s| s.cols));
            let pager = pager_for(rows, screen.map(|s| s.rows), self.no_pager, |name| {
                std::env::var(name).ok()
            });
            if pager.is_some_and(|pager| run_pager(&pager, &page)) {
                return;
            }
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
    /// JSON document, as `structuredContent` is rendered, is coloured like one;
    /// text that was written as markdown is rendered as markdown, which is what
    /// most tool results are.
    fn text(&self, text: &str) {
        if let Some(painted) = self.highlighted(text) {
            return self.line(&painted);
        }
        // A result carrying an image goes out as it is: `shown` splices the
        // drawing's own escape sequence in, and rendering markdown over that
        // would mangle it. Otherwise the escaping comes first, so that markdown
        // never carries an escape sequence of the server's own to the terminal.
        let drawing_pending = !self.drawings.borrow().is_empty();
        let shown = self.shown(text);
        if drawing_pending {
            return self.line(&shown);
        }
        match self.markdown(&shown) {
            Some(rendered) => self.out(&rendered),
            None => self.line(&shown),
        }
    }

    /// The image is kept rather than drawn now: the placeholder line it goes
    /// under has not been printed yet. Bytes that are not a PNG, and a terminal
    /// with no way to show one, keep the placeholder alone.
    fn draw(&self, media: &Media, saved_to: Option<&Path>) {
        let Some(protocol) = self.image else {
            return;
        };
        let screen = screen();
        let Some(drawing) = image::drawing(
            protocol,
            &media.bytes,
            screen.map(|s| s.cols),
            screen.and_then(|s| s.cell),
        ) else {
            return;
        };
        self.drawings
            .borrow_mut()
            .push((mcpdial::session::describe(media, saved_to), drawing));
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

    /// The line is redrawn from its start, so `ERASE` takes the rest of it: a
    /// server whose status message gets shorter, or stops sending one at all,
    /// would otherwise be read as its new count with the old words still after
    /// it, and `progress_end` would leave that on the screen.
    fn progress(&self, text: &str) {
        self.err(&format!("\r{text}{ERASE}"));
        self.progressing.set(true);
    }

    fn progress_end(&self) {
        if self.progressing.replace(false) {
            self.err_line("");
        }
    }

    fn table(&self, headers: &[&str], rows: &[Vec<String>]) {
        for line in self.table_lines(headers, rows) {
            self.line(&line);
        }
    }

    fn error(&self, message: &str, hint: Option<&str>) {
        self.err_line(&format!("{} {message}", self.error_prefix()));
        if let Some(hint) = hint {
            self.err_line(hint);
        }
    }
}

#[cfg(feature = "rich")]
impl Rich {
    /// `text` in colour when colour goes to stdout and the text is a
    /// pretty-printed JSON document; `None` says to print it as it is.
    fn highlighted(&self, text: &str) -> Option<String> {
        self.color_out.then(|| json::highlighted(text)).flatten()
    }

    /// `text` rendered when it was written as markdown and `--raw` did not ask
    /// for it as it came; `None` says to print it as it is. The rendering ends
    /// in its own newline, so it goes out through `out` rather than `line`.
    fn markdown(&self, text: &str) -> Option<String> {
        (self.markdown && markdown::looks_like(text)).then(|| {
            markdown::rendered(
                text,
                &markdown::skin(self.color_out),
                screen().map(|s| s.cols),
            )
        })
    }

    /// A table whose STATUS column, when it has one, is coloured by what each
    /// status means.
    fn table_lines(&self, headers: &[&str], rows: &[Vec<String>]) -> Vec<String> {
        let status = headers.iter().position(|h| *h == "STATUS");
        table_lines(headers, rows, &|col, cell| {
            if Some(col) == status {
                status_style(cell).when(self.color_out).paint(cell)
            } else {
                cell.to_string()
            }
        })
    }

    /// A server's text with the cursor-moving characters escaped, and each
    /// drawing on the line under the placeholder it belongs to. The placeholder
    /// is the whole of the line for a tool result and the end of it for a
    /// prompt's message, which carries the role in front. Both sides come from
    /// the same `describe`, and each drawing is used once, so a server that
    /// writes a placeholder out as text of its own costs at most the line an
    /// image was going to sit on anyway.
    fn shown(&self, text: &str) -> String {
        let mut drawings = self.drawings.borrow_mut();
        let mut out = String::with_capacity(text.len());
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&visible(line));
            if let Some(at) = drawings.iter().position(|(under, _)| line.ends_with(under)) {
                out.push('\n');
                out.push_str(&drawings.remove(at).1);
                self.drew.set(true);
            }
        }
        out
    }

    fn error_prefix(&self) -> String {
        style::Style::new()
            .bold()
            .color(style::Color::Red)
            .when(self.color_err)
            .paint("error:")
    }
}

/// The dot a shell prompt wears, and what colour it is worth.
///
/// Full, half and hollow, in that order, so the three states are still three
/// states where `NO_COLOR` or a colourless terminal leaves the paint off.
#[cfg(feature = "rich")]
fn health_dot(health: Health) -> (&'static str, style::Style) {
    use style::{Color, Style};
    match health {
        Health::Fine => ("\u{25cf}", Style::new().color(Color::Green)),
        Health::Expiring => ("\u{25d0}", Style::new().color(Color::Yellow)),
        Health::Lost => ("\u{25cb}", Style::new().color(Color::Red)),
    }
}

/// Green for a server that answered, yellow for one a credential or an older
/// transport stands between, red for one that is not answering at all.
#[cfg(feature = "rich")]
fn status_style(label: &str) -> style::Style {
    use style::{Color, Style};
    match label {
        "connected" => Style::new().color(Color::Green),
        "auth required" | "token rejected" | "legacy sse" => Style::new().color(Color::Yellow),
        "unreachable" | "blocked (403)" | "error" => Style::new().color(Color::Red),
        l if l.starts_with("http ") => Style::new().color(Color::Red),
        _ => Style::new(),
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

/// What the terminal on stdout says about itself.
#[cfg(feature = "rich")]
#[derive(Clone, Copy)]
struct Screen {
    cols: usize,
    rows: usize,
    /// The pixels one cell takes, from the terminals that report their own size
    /// in pixels as well as in cells. An image is measured against it.
    cell: Option<(usize, usize)>,
}

/// The terminal on stdout, when it can be asked. The `ioctl` is declared here
/// rather than through a crate: std links libc already, and this is the one
/// call needed.
#[cfg(all(feature = "rich", unix))]
fn screen() -> Option<Screen> {
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
    if rc != 0 || size.rows == 0 {
        return None;
    }
    let (cols, rows) = (size.cols as usize, size.rows as usize);
    let cell = (cols > 0 && size.x_pixels > 0 && size.y_pixels > 0)
        .then(|| (size.x_pixels as usize / cols, size.y_pixels as usize / rows));
    Some(Screen { cols, rows, cell })
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

/// A console's size is not asked for, so nothing is paged on Windows and no
/// image is measured against the screen there.
#[cfg(all(feature = "rich", not(unix)))]
fn screen() -> Option<Screen> {
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
    fn a_program_is_prompted_by_name_alone_whatever_the_session_is_doing() {
        for health in [Health::Fine, Health::Expiring, Health::Lost] {
            assert_eq!(Plain.shell_prompt("web", health), "web> ", "{health:?}");
            assert_eq!(Kept::default().shell_prompt("web", health), "web> ");
        }
    }

    #[cfg(feature = "rich")]
    #[test]
    fn the_prompt_dot_says_which_of_the_three_states_the_session_is_in() {
        let painted = Rich {
            color_out: true,
            ..Rich::default()
        };
        assert_eq!(
            painted.shell_prompt("chrome", Health::Fine),
            "chrome \x1b[32m\u{25cf}\x1b[0m > "
        );
        assert_eq!(
            painted.shell_prompt("chrome", Health::Expiring),
            "chrome \x1b[33m\u{25d0}\x1b[0m > "
        );
        assert_eq!(
            painted.shell_prompt("chrome", Health::Lost),
            "chrome \x1b[31m\u{25cb}\x1b[0m > "
        );

        // NO_COLOR, or a terminal with nothing to paint with: full, half and
        // hollow still tell the three apart.
        let bare = Rich::default();
        assert_eq!(
            bare.shell_prompt("chrome", Health::Fine),
            "chrome \u{25cf} > "
        );
        assert_eq!(
            bare.shell_prompt("chrome", Health::Expiring),
            "chrome \u{25d0} > "
        );
        assert_eq!(
            bare.shell_prompt("chrome", Health::Lost),
            "chrome \u{25cb} > "
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
    fn coloured() -> Rich {
        Rich {
            color_out: true,
            color_err: true,
            ..Default::default()
        }
    }

    #[cfg(feature = "rich")]
    #[test]
    fn every_status_word_has_its_colour_and_the_padding_ignores_it() {
        let rows: Vec<Vec<String>> = [
            ("a", "connected"),
            ("b", "auth required"),
            ("c", "token rejected"),
            ("d", "legacy sse"),
            ("e", "unreachable"),
            ("f", "blocked (403)"),
            ("g", "error"),
            ("h", "http 503"),
            ("i", "something new"),
        ]
        .iter()
        .map(|(n, s)| vec![n.to_string(), s.to_string(), "x".into()])
        .collect();
        assert_eq!(
            coloured().table_lines(&["NAME", "STATUS", "SERVER"], &rows),
            [
                "NAME  STATUS          SERVER",
                "a     \x1b[32mconnected\x1b[0m       x",
                "b     \x1b[33mauth required\x1b[0m   x",
                "c     \x1b[33mtoken rejected\x1b[0m  x",
                "d     \x1b[33mlegacy sse\x1b[0m      x",
                "e     \x1b[31munreachable\x1b[0m     x",
                "f     \x1b[31mblocked (403)\x1b[0m   x",
                "g     \x1b[31merror\x1b[0m           x",
                "h     \x1b[31mhttp 503\x1b[0m        x",
                "i     something new   x",
            ]
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn colour_stays_in_the_status_column_and_off_when_disabled() {
        let rows = vec![vec!["error".into(), "connected".into()]];
        assert_eq!(
            coloured().table_lines(&["NAME", "STATUS"], &rows),
            ["NAME   STATUS", "error  \x1b[32mconnected\x1b[0m"]
        );
        assert_eq!(
            coloured().table_lines(&["NAME", "TYPE"], &rows),
            ["NAME   TYPE", "error  connected"]
        );
        assert_eq!(
            Rich::default().table_lines(&["NAME", "STATUS"], &rows),
            ["NAME   STATUS", "error  connected"]
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn the_error_prefix_is_bold_red_only_with_colour_on() {
        assert_eq!(coloured().error_prefix(), "\x1b[1;31merror:\x1b[0m");
        assert_eq!(Rich::default().error_prefix(), "error:");
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_pretty_printed_document_is_highlighted_only_with_colour_on() {
        let pretty = serde_json::to_string_pretty(&serde_json::json!({"a": 1})).unwrap();
        assert_eq!(
            coloured().highlighted(&pretty).as_deref(),
            Some("{\n  \x1b[1;34m\"a\"\x1b[0m: \x1b[36m1\x1b[0m\n}")
        );
        assert_eq!(Rich::default().highlighted(&pretty), None);
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

    /// What `text` put on stdout, taken from the page rather than the terminal.
    /// Every line here is short enough that no screen width wraps it.
    #[cfg(feature = "rich")]
    fn shown(rich: &Rich, text: &str) -> String {
        rich.page_start();
        rich.text(text);
        String::from_utf8(rich.page.borrow_mut().take().unwrap_or_default())
            .expect("the presenter writes UTF-8")
    }

    #[cfg(feature = "rich")]
    #[test]
    fn markdown_is_rendered_and_anything_else_reaches_the_terminal_as_it_came() {
        let rich = Rich {
            markdown: true,
            ..Default::default()
        };
        assert_eq!(shown(&rich, "# T\n- one"), "T\n• one\n");
        assert_eq!(shown(&rich, "one\ntwo\r"), "one\ntwo\\r\n");
        let pretty = serde_json::to_string_pretty(&serde_json::json!({"a": 1})).unwrap();
        assert_eq!(shown(&rich, &pretty), "{\n  \"a\": 1\n}\n");
    }

    #[cfg(feature = "rich")]
    #[test]
    fn raw_prints_the_markdown_a_server_sent() {
        assert_eq!(shown(&Rich::default(), "# T\n- one"), "# T\n- one\n");
    }

    #[cfg(feature = "rich")]
    fn shot() -> Media {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(4096, 0);
        Media {
            kind: "image".into(),
            mime_type: "image/png".into(),
            bytes,
        }
    }

    #[cfg(feature = "rich")]
    fn drawing_terminal() -> Rich {
        Rich {
            image: Some(image::Protocol::Iterm2),
            ..Default::default()
        }
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_drawing_goes_under_the_placeholder_line_it_belongs_to() {
        let rich = drawing_terminal();
        rich.draw(&shot(), None);
        let shown = rich.shown("done\n[image image/png, 4 KB]\nand after");
        let (before, rest) = shown.split_once('\n').unwrap();
        let (placeholder, rest) = rest.split_once('\n').unwrap();
        let (drawn, after) = rest.split_once('\n').unwrap();
        assert_eq!(
            (before, placeholder, after),
            ("done", "[image image/png, 4 KB]", "and after")
        );
        assert!(
            drawn.starts_with("\x1b]1337;File=inline=1;size=4096"),
            "{drawn:?}"
        );
        assert!(drawn.ends_with('\x07'), "{drawn:?}");
        assert!(rich.drew.get(), "a page with an image in it is not paged");
        // Used once: a second result gets nothing left over from the first.
        assert_eq!(
            rich.shown("[image image/png, 4 KB]"),
            "[image image/png, 4 KB]"
        );
        // A prompt's message carries its role in front of the placeholder.
        rich.draw(&shot(), None);
        assert!(rich
            .shown("user: [image image/png, 4 KB]")
            .starts_with("user: [image image/png, 4 KB]\n\x1b]1337;File="));
    }

    #[cfg(feature = "rich")]
    #[test]
    fn a_terminal_with_no_way_to_draw_leaves_the_placeholder_alone() {
        let rich = Rich::default();
        rich.draw(&shot(), None);
        assert_eq!(
            rich.shown("[image image/png, 4 KB]"),
            "[image image/png, 4 KB]"
        );
        assert!(!rich.drew.get());
    }

    #[cfg(feature = "rich")]
    #[test]
    fn anything_but_a_png_keeps_the_placeholder_on_a_terminal_that_could_draw() {
        let rich = drawing_terminal();
        let jpeg = Media {
            kind: "image".into(),
            mime_type: "image/jpeg".into(),
            bytes: b"\xff\xd8\xff\xe0".to_vec(),
        };
        rich.draw(&jpeg, None);
        assert_eq!(
            rich.shown("[image image/jpeg, 4 B]"),
            "[image image/jpeg, 4 B]"
        );
    }

    #[cfg(feature = "rich")]
    #[test]
    fn escaping_is_untouched_by_the_drawings_beside_it() {
        let rich = drawing_terminal();
        let text = "Valid keys are: Enter,\r,\n,\0,\x1b[0m\tend";
        assert_eq!(rich.shown(text), visible(text));
        rich.draw(&shot(), Some(Path::new("shots/shot-1.png")));
        assert_eq!(
            rich.shown("[image saved to shots/shot-1.png, 4 KB]\r"),
            "[image saved to shots/shot-1.png, 4 KB]\\r",
            "a placeholder the server's own text ran into is not one of ours"
        );
        // The same line, and now it is: `--save-dir` names the file and draws it.
        assert!(rich
            .shown("[image saved to shots/shot-1.png, 4 KB]")
            .contains("\x1b]1337;File="));
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
