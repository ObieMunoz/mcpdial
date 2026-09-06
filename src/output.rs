//! Where a result goes when printing all of it would be a mistake.
//!
//! `--max-chars` bounds what lands on stdout; `--output` sends the whole of it
//! to a file and leaves one line behind. Both exist for a caller with a context
//! window to protect, so both say what they did in a way a program can read: a
//! cut result under `--json` is an object under a `truncated` key no earlier
//! release emitted, and a written file is named by the summary that replaces
//! it. Neither flag given, every byte is the one that was always there.

use crate::present::Presenter;
use crate::{Cli, Cmd};
use mcpdial::session::ResourceBody;
use mcpdial::Error;
use serde_json::{json, Map, Value};
use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// A default for `--max-chars`, so an agent harness sets the bound once.
///
/// Read here rather than in [`crate::env_defaults`], which fills a flag in for
/// every command: this one is a flag only some commands take, and a bound
/// filled in everywhere would make `ls` refuse itself.
pub const ENV_MAX_CHARS: &str = "MCPDIAL_MAX_CHARS";

/// The commands with a server's result to send anywhere. Every other command
/// prints something mcpdial composed itself, which is already small.
const APPLIES_TO: &str = "--max-chars and --output apply to call, prompt, read, raw and shell";

/// The flag whose path this module reserves and writes, for the errors that
/// name it. A shell `save` writes through the same sink under its own name.
const FLAG: &str = "--output";

/// A payload on its way to stdout, in the unit its bound is counted in.
pub enum Payload<'a> {
    /// Text a server sent, already rendered.
    Text(&'a str),
    /// A result object. Cutting a serialized document leaves something no
    /// reader could parse, so a cut one is replaced rather than shortened.
    Json { value: &'a Value, one_line: bool },
    /// A resource's bodies as they are. Bytes are not characters and are never
    /// cut; a file is the only thing to do with them.
    Bytes(&'a [u8]),
}

/// What to print in the payload's place.
#[derive(Debug)]
pub enum Delivery {
    /// Neither flag had anything to say: print the payload as it always was.
    Whole,
    /// Print this where the payload would have gone, and say `note` on stderr.
    Cut {
        payload: String,
        note: Option<String>,
    },
    /// The payload is in a file; this one line goes out instead.
    Wrote(String),
}

/// The way a payload reaches stdout, so a cut one takes the same way the whole
/// one would have.
#[derive(Clone, Copy)]
pub enum As {
    /// A JSON document.
    Json,
    /// Text a server sent, which a terminal shows the escapes of.
    Text,
    /// Bytes as they are, with no line of their own.
    Raw,
}

/// A delivery on its way out: the summary of a file, the head of a payload
/// with whatever it has to say on stderr, or `whole`, the way the payload
/// always left when neither flag had anything to say about it.
pub fn show(ui: &dyn Presenter, sent: Delivery, shape: As, json: bool, whole: impl FnOnce()) {
    match sent {
        Delivery::Whole => whole(),
        Delivery::Cut { payload, note } => {
            match shape {
                As::Json => ui.json(&payload),
                As::Text => ui.text(&payload),
                As::Raw => ui.out(&payload),
            }
            if let Some(note) = note {
                ui.err_line(&note);
            }
        }
        Delivery::Wrote(line) if json => ui.json(&line),
        Delivery::Wrote(line) => ui.line(&line),
    }
}

/// The two flags as one sink.
pub struct Output {
    max_chars: Option<usize>,
    file: Option<PathBuf>,
    json: bool,
    /// What the errors and the summary call whoever asked for the file, since
    /// `--output` is not the only thing that writes one.
    flag: &'static str,
    /// Whether this run made `file`. Later commands of one `shell` session
    /// replace what earlier ones wrote there; a file mcpdial did not make is
    /// never written over.
    ours: Cell<bool>,
}

impl Output {
    /// The sink the flags ask for, checked before anything is dialed: a command
    /// that would ignore them says so, and a `--output` path that could not be
    /// written is refused while nothing has been sent.
    pub fn choose(cli: &Cli) -> Result<Self, Error> {
        let applies = matches!(
            cli.cmd,
            Cmd::Call { .. }
                | Cmd::Prompt { .. }
                | Cmd::Read { .. }
                | Cmd::Raw { .. }
                | Cmd::Shell { .. }
        );
        if !applies {
            if cli.max_chars.is_some() || cli.output.is_some() {
                return Err(Error::usage(APPLIES_TO));
            }
            return Ok(Self::inert(cli.json));
        }
        let max_chars = match cli.max_chars {
            Some(n) => Some(n),
            None => from_env()?,
        };
        if let Some(path) = &cli.output {
            reserve(FLAG, path)?;
        }
        Ok(Self {
            max_chars,
            file: cli.output.clone(),
            json: cli.json,
            flag: FLAG,
            ours: Cell::new(false),
        })
    }

    /// One file, named by the command that asked for it rather than by a flag:
    /// what the shell's `save` writes through, so that an unwritable path, a
    /// file already there and a path that is a directory are all answered in
    /// the words `--output` answers them in.
    pub fn to_path(flag: &'static str, path: PathBuf, json: bool) -> Self {
        Self {
            max_chars: None,
            file: Some(path),
            json,
            flag,
            ours: Cell::new(false),
        }
    }

    fn inert(json: bool) -> Self {
        Self {
            max_chars: None,
            file: None,
            json,
            flag: FLAG,
            ours: Cell::new(false),
        }
    }

    /// The payload's fate: a file, a cut, or the path it always took.
    /// `is_error` is a tool's `isError`, which the summary carries so a reader
    /// of that line alone still sees what the result said.
    pub fn deliver(&self, payload: Payload<'_>, is_error: bool) -> Result<Delivery, Error> {
        if let Some(path) = &self.file {
            let (bytes, unit, count) = match payload {
                Payload::Text(text) => (text.as_bytes().to_vec(), CHARS, text.chars().count()),
                Payload::Json { value, one_line } => {
                    let doc = document(value, one_line);
                    let count = doc.chars().count();
                    (doc.into_bytes(), CHARS, count)
                }
                Payload::Bytes(bytes) => (bytes.to_vec(), BYTES, bytes.len()),
            };
            self.write(path, &bytes)?;
            return Ok(Delivery::Wrote(self.summary(path, unit, count, is_error)));
        }
        let Some(limit) = self.max_chars else {
            return Ok(Delivery::Whole);
        };
        match payload {
            Payload::Bytes(_) => Ok(Delivery::Whole),
            Payload::Text(text) => Ok(self.cut(text, limit, |head| head.to_string())),
            Payload::Json { value, one_line } => {
                let doc = document(value, one_line);
                Ok(self.cut(&doc, limit, |head| {
                    envelope(head, limit, doc.chars().count(), is_error, one_line)
                }))
            }
        }
    }

    /// A resource's bodies: characters while every body is text, and the bytes
    /// themselves once any of them is not, since a blob has no characters to
    /// count and a reader of one wants it back unchanged.
    pub fn deliver_resource(&self, bodies: &[ResourceBody]) -> Result<Delivery, Error> {
        if bodies.iter().all(|b| matches!(b, ResourceBody::Text(_))) {
            let text: String = bodies
                .iter()
                .map(|b| match b {
                    ResourceBody::Text(t) => t.as_str(),
                    ResourceBody::Bytes(_) => "",
                })
                .collect();
            return self.deliver(Payload::Text(&text), false);
        }
        let bytes: Vec<u8> = bodies
            .iter()
            .flat_map(|b| match b {
                ResourceBody::Text(t) => t.as_bytes().to_vec(),
                ResourceBody::Bytes(b) => b.clone(),
            })
            .collect();
        self.deliver(Payload::Bytes(&bytes), false)
    }

    /// The first `limit` characters, counted in characters so no UTF-8
    /// sequence is ever halved. The cut happens before any presenter escapes a
    /// control character, so an escape it writes cannot be split either.
    fn cut(&self, text: &str, limit: usize, shape: impl FnOnce(&str) -> String) -> Delivery {
        let total = text.chars().count();
        if total <= limit {
            return Delivery::Whole;
        }
        let head: String = text.chars().take(limit).collect();
        Delivery::Cut {
            payload: shape(&head),
            note: (!self.json).then(|| {
                format!(
                    "output truncated: {} of {} chars shown; use --output FILE or --json for all of it",
                    grouped(limit),
                    grouped(total)
                )
            }),
        }
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        let mut open = OpenOptions::new();
        open.write(true);
        if self.ours.get() {
            open.create(true).truncate(true);
        } else {
            open.create_new(true);
        }
        let mut file = open.open(path).map_err(|e| refused(self.flag, path, &e))?;
        file.write_all(bytes)
            .map_err(|e| Error::transport(format!("{} {}: {e}", self.flag, path.display())))?;
        self.ours.set(true);
        Ok(())
    }

    fn summary(&self, path: &Path, unit: &'static str, count: usize, is_error: bool) -> String {
        if self.json {
            let mut o = Map::new();
            o.insert("output".into(), json!(path.display().to_string()));
            o.insert(unit.into(), json!(count));
            o.insert("isError".into(), json!(is_error));
            Value::Object(o).to_string()
        } else {
            format!("wrote {} {unit} to {}", grouped(count), path.display())
        }
    }
}

const CHARS: &str = "chars";
const BYTES: &str = "bytes";

/// The object that goes out in a cut result's place: the head of it, and one
/// key saying it is a head. Nothing the server sent is rewritten, so a reader
/// that finds `truncated` knows every other key is missing rather than edited.
fn envelope(head: &str, shown: usize, total: usize, is_error: bool, one_line: bool) -> String {
    let value = json!({
        "truncated": {
            "chars": shown,
            "totalChars": total,
            "hint": "re-run with --output FILE to keep the whole result",
        },
        "isError": is_error,
        "head": head,
    });
    document(&value, one_line)
}

/// A JSON document as it goes to stdout: one line where a line is all a reader
/// can take, pretty-printed everywhere else.
pub fn document(value: &Value, one_line: bool) -> String {
    if one_line {
        value.to_string()
    } else {
        serde_json::to_string_pretty(value).expect("a JSON value is serializable")
    }
}

fn from_env() -> Result<Option<usize>, Error> {
    match std::env::var(ENV_MAX_CHARS) {
        Err(_) => Ok(None),
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => v.trim().parse().map(Some).map_err(|_| {
            Error::usage(format!(
                "{ENV_MAX_CHARS} must be a number of characters, got {v:?}"
            ))
        }),
    }
}

/// What a flag that writes a file can be told about its path before a single
/// byte is sent: a file already there is not written over, and neither is a
/// directory. `flag` names the flag being answered, since more than one of
/// them writes a file and each has to say which one refused.
pub fn reserve(flag: &str, path: &Path) -> Result<(), Error> {
    match path.metadata() {
        Ok(m) if m.is_dir() => Err(Error::usage(format!(
            "{flag} {} is a directory; name the file to write",
            path.display()
        ))),
        Ok(_) => Err(already_exists(flag, path)),
        Err(e) if e.kind() == ErrorKind::NotFound => match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() && !dir.is_dir() => {
                Err(Error::usage(format!(
                    "{flag} {}: {} is not a directory",
                    path.display(),
                    dir.display()
                )))
            }
            _ => Ok(()),
        },
        Err(e) => Err(Error::usage(format!("{flag} {}: {e}", path.display()))),
    }
}

/// A file that was not there a moment ago, holding `bytes`. The path was
/// [`reserve`]d before anything was dialed, but a directory the process cannot
/// write to only says so here, so the same words answer either way.
pub fn create(flag: &str, path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| refused(flag, path, &e))?;
    file.write_all(bytes)
        .map_err(|e| Error::transport(format!("{flag} {}: {e}", path.display())))
}

fn already_exists(flag: &str, path: &Path) -> Error {
    Error::usage(format!(
        "{flag} {} already exists; name a path that does not, or remove it",
        path.display()
    ))
}

fn refused(flag: &str, path: &Path, e: &std::io::Error) -> Error {
    match e.kind() {
        ErrorKind::AlreadyExists => already_exists(flag, path),
        _ => Error::usage(format!("{flag} {}: {e}", path.display())),
    }
}

/// A count as a person reads it, since these are the numbers that decide
/// whether the rest is worth fetching.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounded(limit: usize, json: bool) -> Output {
        Output {
            max_chars: Some(limit),
            file: None,
            json,
            flag: FLAG,
            ours: Cell::new(false),
        }
    }

    fn to_file(path: &Path, json: bool) -> Output {
        Output::to_path(FLAG, path.to_path_buf(), json)
    }

    fn cut(d: Delivery) -> (String, Option<String>) {
        match d {
            Delivery::Cut { payload, note } => (payload, note),
            Delivery::Whole => panic!("expected a cut, got the whole payload"),
            Delivery::Wrote(line) => panic!("expected a cut, got {line}"),
        }
    }

    fn wrote(d: Delivery) -> String {
        match d {
            Delivery::Wrote(line) => line,
            _ => panic!("expected a file"),
        }
    }

    #[test]
    fn a_cut_lands_between_characters_never_inside_one() {
        let text = "héllo 🌍 wörld";
        let out = bounded(8, false);
        let (payload, note) = cut(out.deliver(Payload::Text(text), false).unwrap());
        assert_eq!(payload, "héllo 🌍 ");
        assert_eq!(payload.chars().count(), 8, "the bound counts characters");
        assert!(payload.len() > 8, "and those characters are not bytes");
        assert_eq!(
            note.unwrap(),
            "output truncated: 8 of 13 chars shown; use --output FILE or --json for all of it"
        );
    }

    #[test]
    fn every_prefix_of_multi_byte_text_is_still_text() {
        let text = "aé🌍\u{0301}z";
        for limit in 0..=text.chars().count() + 1 {
            match bounded(limit, false)
                .deliver(Payload::Text(text), false)
                .unwrap()
            {
                Delivery::Whole => assert!(limit >= text.chars().count()),
                Delivery::Cut { payload, .. } => {
                    assert_eq!(payload.chars().count(), limit);
                    assert!(text.starts_with(&payload));
                }
                Delivery::Wrote(_) => unreachable!(),
            }
        }
    }

    #[test]
    fn a_payload_within_the_bound_is_left_alone() {
        let out = bounded(64, false);
        assert!(matches!(
            out.deliver(Payload::Text("short"), false).unwrap(),
            Delivery::Whole
        ));
        let value = json!({"content": [], "isError": false});
        assert!(matches!(
            out.deliver(
                Payload::Json {
                    value: &value,
                    one_line: true
                },
                false
            )
            .unwrap(),
            Delivery::Whole
        ));
    }

    #[test]
    fn a_cut_result_is_an_object_with_a_key_no_result_has() {
        let value =
            json!({"content": [{"type": "text", "text": "x".repeat(500)}], "isError": true});
        let (payload, note) = cut(bounded(40, true)
            .deliver(
                Payload::Json {
                    value: &value,
                    one_line: true,
                },
                true,
            )
            .unwrap());
        assert!(note.is_none(), "the object says it itself under --json");
        let got: Value = serde_json::from_str(&payload).expect("still a JSON document");
        assert_eq!(got["truncated"]["chars"], 40);
        assert_eq!(
            got["truncated"]["totalChars"],
            document(&value, true).chars().count()
        );
        assert_eq!(got["isError"], true);
        assert_eq!(got["head"].as_str().unwrap().chars().count(), 40);
        assert!(got.get("content").is_none(), "nothing is half a result");
    }

    #[test]
    fn bytes_are_never_cut() {
        assert!(matches!(
            bounded(2, false)
                .deliver(Payload::Bytes(b"\x89PNG\r\n\x1a\n"), false)
                .unwrap(),
            Delivery::Whole
        ));
    }

    #[test]
    fn a_file_takes_the_payload_whole_and_leaves_one_line() {
        let dir = std::env::temp_dir().join(format!("mcpdial-output-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.txt");
        let text = "é".repeat(1500);

        let line = wrote(
            to_file(&path, false)
                .deliver(Payload::Text(&text), false)
                .unwrap(),
        );
        assert_eq!(line, format!("wrote 1,500 chars to {}", path.display()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // A path already holding something is not written over.
        let err = to_file(&path, false)
            .deliver(Payload::Text("other"), false)
            .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // The same sink, twice: the second command replaces the first's file.
        let sink = to_file(&dir.join("twice.json"), true);
        let first = json!({"a": 1});
        wrote(
            sink.deliver(
                Payload::Json {
                    value: &first,
                    one_line: true,
                },
                false,
            )
            .unwrap(),
        );
        let line = wrote(sink.deliver(Payload::Bytes(b"\x89PNG"), true).unwrap());
        assert_eq!(
            line,
            json!({"output": dir.join("twice.json").display().to_string(),
                   "bytes": 4, "isError": true})
            .to_string()
        );
        assert_eq!(std::fs::read(dir.join("twice.json")).unwrap(), b"\x89PNG");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_and_a_missing_parent_are_refused_before_anything_is_sent() {
        let dir = std::env::temp_dir().join(format!("mcpdial-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(reserve(FLAG, &dir).is_err(), "a directory is not a file");
        assert!(reserve(FLAG, &dir.join("nowhere/deep/result.txt")).is_err());
        assert!(reserve(FLAG, &dir.join("result.txt")).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counts_are_grouped_where_a_reader_expects_them() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(480_221), "480,221");
    }
}
