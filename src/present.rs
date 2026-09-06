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
    /// differs, and `describe` knows how to read it.
    fn named(
        &self,
        items: &[Value],
        long: bool,
        takes: &str,
        describe: &dyn Fn(&Value) -> Vec<String>,
    ) {
        for item in items {
            // `tools --all` marks what the allow and deny lists hide.
            let marker = if item["denied"] == true {
                " (denied)"
            } else {
                ""
            };
            let name = format!("{}{marker}", item["name"].as_str().unwrap_or("?"));
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
}

impl dyn Presenter {
    /// The one decision: `Rich` only for a person at a terminal who asked for
    /// nothing else. `--json`, `--plain`, `MCPDIAL_PLAIN`, a dumb terminal or a
    /// pipe each mean `Plain`, and so does a build without the `rich` feature.
    pub fn choose(cli: &Cli) -> Box<dyn Presenter> {
        if wants_plain(cli) {
            Box::new(Plain)
        } else {
            rich()
        }
    }
}

fn wants_plain(cli: &Cli) -> bool {
    use std::io::IsTerminal;
    let by_env = std::env::var(ENV_PLAIN).is_ok_and(|v| !v.is_empty() && v != "0");
    let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
    cli.json || cli.plain || by_env || dumb || !std::io::stdout().is_terminal()
}

#[cfg(feature = "rich")]
fn rich() -> Box<dyn Presenter> {
    Box::new(Rich::default())
}

/// Without the feature there is nothing but `Plain` to choose.
#[cfg(not(feature = "rich"))]
fn rich() -> Box<dyn Presenter> {
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
}

#[cfg(feature = "rich")]
impl Presenter for Rich {
    fn out(&self, text: &str) {
        Plain.out(text);
    }

    fn line(&self, line: &str) {
        Plain.line(line);
    }

    fn err(&self, text: &str) {
        Plain.err(text);
    }

    fn err_line(&self, line: &str) {
        Plain.err_line(line);
    }

    fn bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        Plain.bytes(bytes)
    }

    /// The control characters that move the cursor or open an escape sequence
    /// are shown as escapes: a server that lists `\r` among its valid keys
    /// otherwise overwrites the start of its own error message. Newlines and
    /// tabs are the text's own layout and stay.
    fn text(&self, text: &str) {
        self.line(&visible(text));
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
}
