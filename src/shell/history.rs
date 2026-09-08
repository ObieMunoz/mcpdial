//! What a shell session remembers of the results it has printed.
//!
//! Every `call`, `read`, `prompt` and `raw` result is filed under a number as it
//! goes out, so that a later line can name one rather than run it again: `show 3`
//! prints it a second time, `save 3 FILE` writes it, `retry` sends the last call
//! once more with an argument changed. `_` names the last result and `$3` the
//! third, which is what an eye reading the numbers off the screen reaches for.
//!
//! None of it reaches a program that did not ask: the numbers are drawn by the
//! terminal presenter alone, so a piped session's bytes are the ones they always
//! were, and the commands that name a number are commands nobody's script has.
//!
//! A result is the largest thing this program handles - `--max-chars` exists
//! because one of them should not land in a context window - so what a session
//! holds is bounded twice over: [`RESULTS`] of them at most, and [`BYTES`] of
//! them altogether, whichever runs out first. The newest is never dropped, even
//! where it is larger than the whole budget on its own, because `_` has to name
//! something. Nothing is written to disk, and nothing outlives the process.

use crate::media::{file_stem, resource_stem};
use mcpdial::session::{extension_for, media, media_blocks, resource_bodies, Media, ResourceBody};
use mcpdial::Error;
use serde_json::{Map, Value};
use std::collections::VecDeque;

/// How many results one session keeps.
pub const RESULTS: usize = 50;

/// How much of them, measured as the JSON they would print as. A screenshot is
/// a megabyte and a crawl is several, so fifty of the largest results a real
/// server sends would be a gigabyte held for nothing; this is the number that
/// says no.
pub const BYTES: usize = 8 * 1024 * 1024;

/// The arguments of the last [`RESULTS`] calls, for `retry` and `edit` to send
/// again. They came off a line somebody typed, so they are small next to what
/// came back.
const CALLS: usize = RESULTS;

/// What produced a result: what `retry` and `edit` send again, and what names
/// the file `save` writes when nobody names one.
#[derive(Clone, Debug)]
pub enum Origin {
    Call { tool: String, arguments: Value },
    Prompt { name: String },
    Read { uri: String },
    Raw { method: String },
}

impl Origin {
    /// The command that produced the result, for an error that has to say why a
    /// line naming it cannot do what it asked.
    pub fn command(&self) -> &'static str {
        match self {
            Origin::Call { .. } => "call",
            Origin::Prompt { .. } => "prompt",
            Origin::Read { .. } => "read",
            Origin::Raw { .. } => "raw",
        }
    }

    /// The file name a result is saved under when nobody names one: what it came
    /// from, less anything a filesystem would argue with.
    fn stem(&self) -> String {
        match self {
            Origin::Call { tool, .. } => file_stem(tool),
            Origin::Prompt { name } => file_stem(name),
            Origin::Read { uri } => resource_stem(uri),
            Origin::Raw { method } => file_stem(method),
        }
    }
}

/// One result, as it was printed and as it can be printed again.
#[derive(Debug)]
pub struct Recorded {
    /// What this session called the result on screen.
    pub number: usize,
    pub origin: Origin,
    /// The result object exactly as the server sent it: what `--json` printed,
    /// and what a path filter is later applied to.
    pub result: Value,
    /// The same result as this session rendered it for a person; empty for the
    /// results that print as JSON whichever mode they print in.
    pub text: String,
    weight: usize,
}

/// What one result puts in a file, and how that file is named when nobody names
/// it. Nothing here is a copy of the result: the bytes of a binary block are
/// decoded once, on the way out.
pub enum Keep {
    /// The result object, as `--json` prints it.
    Document,
    /// The first binary block the result carries, byte for byte.
    Bytes { bytes: Vec<u8>, extension: String },
    /// A resource's bodies as they came: text while every one of them is text.
    Bodies {
        bodies: Vec<ResourceBody>,
        extension: String,
    },
    /// The result as this session rendered it.
    Text,
}

impl Keep {
    /// The extension of a file named after the result rather than by hand.
    pub fn extension(&self) -> &str {
        match self {
            Keep::Document => "json",
            Keep::Bytes { extension, .. } | Keep::Bodies { extension, .. } => extension,
            Keep::Text => "txt",
        }
    }
}

impl Recorded {
    /// Whether the tool that answered reported an error, which the summary of a
    /// saved file carries so that a reader of that line alone still sees it.
    pub fn failed(&self) -> bool {
        self.result["isError"] == true
    }

    /// What `save` writes: the whole object under `--json` and for a `raw` result,
    /// which has no other form; a binary block as its bytes; text as text.
    pub fn keep(&self, json: bool) -> Result<Keep, Error> {
        if json || matches!(self.origin, Origin::Raw { .. }) {
            return Ok(Keep::Document);
        }
        if let Origin::Read { .. } = self.origin {
            return Ok(Keep::Bodies {
                bodies: resource_bodies(&self.result)?,
                extension: body_extension(&self.result),
            });
        }
        if let Some(found) = first_media(&self.result)? {
            return Ok(Keep::Bytes {
                extension: extension_for(&found.mime_type).to_string(),
                bytes: found.bytes,
            });
        }
        // A result with nothing to show as text is saved as the object it was,
        // rather than as an empty file.
        Ok(match self.text.is_empty() {
            true => Keep::Document,
            false => Keep::Text,
        })
    }

    /// The file `save` writes to when nobody named one.
    pub fn filename(&self, keep: &Keep) -> String {
        format!("{}.{}", self.origin.stem(), keep.extension())
    }
}

/// The results one shell session has printed, oldest first.
#[derive(Default)]
pub struct History {
    kept: VecDeque<Recorded>,
    /// The weight of `kept`, so that dropping one costs no re-measuring.
    weight: usize,
    /// How many results this session has printed. The numbering does not restart
    /// when an old result is dropped: `[51]` follows `[50]`.
    printed: usize,
    /// Every `call` this session sent, and what it sent, whether or not the
    /// server answered - a call that failed is the one most worth running again.
    sent: VecDeque<(String, Value)>,
}

impl History {
    /// The number the next result will be filed under, so that it can be shown
    /// before the result it names.
    pub fn next_number(&self) -> usize {
        self.printed + 1
    }

    /// File a result, dropping the oldest until the session is back inside both
    /// bounds.
    pub fn record(&mut self, origin: Origin, result: Value, text: String) {
        self.printed += 1;
        let weight = json_bytes(&result) + text.len();
        self.weight += weight;
        self.kept.push_back(Recorded {
            number: self.printed,
            origin,
            result,
            text,
            weight,
        });
        while self.kept.len() > 1 && (self.kept.len() > RESULTS || self.weight > BYTES) {
            if let Some(dropped) = self.kept.pop_front() {
                self.weight -= dropped.weight;
            }
        }
    }

    /// A `call` on its way out, remembered before the server has had its say.
    pub fn sending(&mut self, tool: &str, arguments: &Value) {
        self.sent.push_back((tool.to_string(), arguments.clone()));
        while self.sent.len() > CALLS {
            self.sent.pop_front();
        }
    }

    /// The call `retry` sends again: the last one of `tool` where a tool is
    /// named, and the last one of any tool otherwise.
    pub fn last_call(&self, tool: Option<&str>) -> Option<(&str, &Value)> {
        self.sent
            .iter()
            .rev()
            .find(|(name, _)| tool.is_none_or(|wanted| wanted == name))
            .map(|(name, arguments)| (name.as_str(), arguments))
    }

    /// The result a line named: `_` for the last, `$3` or `3` for the third.
    pub fn find(&self, reference: &str) -> Result<&Recorded, Error> {
        let named = reference.trim();
        if named == "_" {
            return self.kept.back().ok_or_else(nothing_yet);
        }
        let number = named
            .strip_prefix('$')
            .unwrap_or(named)
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                Error::usage(format!(
                    "{named:?} is not a result number; results are numbered from 1, \
                     and _ is the last one"
                ))
            })?;
        self.kept
            .iter()
            .find(|r| r.number == number)
            .ok_or_else(|| self.out_of_reach(number))
    }

    fn out_of_reach(&self, number: usize) -> Error {
        match self.printed {
            0 => nothing_yet(),
            printed if number > printed => Error::usage(format!(
                "there is no result {number}; this session has printed {printed}"
            )),
            _ => Error::usage(format!(
                "result {number} is no longer kept; a session holds the last {RESULTS} results, \
                 or {} MiB of them, whichever runs out first",
                BYTES / 1024 / 1024
            )),
        }
    }
}

fn nothing_yet() -> Error {
    Error::usage("no results in this session yet")
}

/// The extension a saved `resources/read` takes: what the first body says it is,
/// and text where it says nothing.
fn body_extension(result: &Value) -> String {
    result["contents"][0]["mimeType"]
        .as_str()
        .map_or_else(|| "txt".to_string(), |mime| extension_for(mime).to_string())
}

/// The first block of a result carrying bytes, decoded.
fn first_media(result: &Value) -> Result<Option<Media>, Error> {
    for block in media_blocks(result) {
        if let Some(found) = media(block)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The bytes a value would occupy as JSON, counted without building the string:
/// the whole point of the measurement is not to hold a second copy of something
/// already too large.
fn json_bytes(value: &Value) -> usize {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counted = Counter::default();
    serde_json::to_writer(&mut counted, value).ok();
    counted.0
}

/// `base` with `changes` written over it, which is what `retry TOOL key=value`
/// sends: every argument of the last call, and the one that is different.
pub fn merged(base: &Value, changes: &Value) -> Value {
    let (Some(base), Some(extra)) = (base.as_object(), changes.as_object()) else {
        return changes.clone();
    };
    let mut fields = base.clone();
    for (key, value) in extra {
        fields.insert(key.clone(), value.clone());
    }
    Value::Object(fields)
}

/// The empty arguments a `retry` of a tool this session has not called yet
/// starts from.
pub fn no_arguments() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_result(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    fn call(tool: &str) -> Origin {
        Origin::Call {
            tool: tool.to_string(),
            arguments: json!({}),
        }
    }

    #[test]
    fn results_are_numbered_from_one_and_named_three_ways() {
        let mut history = History::default();
        assert_eq!(history.next_number(), 1);
        for n in 1..=3 {
            history.record(
                call("count"),
                text_result(&format!("count={n}")),
                format!("count={n}"),
            );
        }
        assert_eq!(history.find("2").unwrap().text, "count=2");
        assert_eq!(history.find("$2").unwrap().text, "count=2");
        assert_eq!(history.find("_").unwrap().text, "count=3");
        assert_eq!(history.find("_").unwrap().number, 3);
        assert_eq!(history.next_number(), 4);
    }

    #[test]
    fn a_number_nobody_printed_says_which_ones_there_are() {
        let history = History::default();
        assert!(history
            .find("1")
            .unwrap_err()
            .to_string()
            .contains("no results"));
        assert!(history
            .find("nope")
            .unwrap_err()
            .to_string()
            .contains("not a result number"));
        assert!(history
            .find("0")
            .unwrap_err()
            .to_string()
            .contains("not a result number"));

        let mut history = History::default();
        history.record(call("count"), text_result("count=1"), "count=1".into());
        let refused = history.find("7").unwrap_err().to_string();
        assert!(refused.contains("no result 7"), "{refused}");
        assert!(refused.contains("printed 1"), "{refused}");
    }

    #[test]
    fn a_session_keeps_the_last_fifty_and_numbers_past_them() {
        let mut history = History::default();
        for n in 1..=RESULTS + 5 {
            history.record(call("count"), text_result(&n.to_string()), n.to_string());
        }
        assert_eq!(history.kept.len(), RESULTS);
        assert_eq!(history.find("_").unwrap().number, RESULTS + 5);
        assert_eq!(
            history.find(&(RESULTS + 5).to_string()).unwrap().text,
            (RESULTS + 5).to_string()
        );
        let dropped = history.find("1").unwrap_err().to_string();
        assert!(dropped.contains("no longer kept"), "{dropped}");
        assert!(
            dropped.contains("8 MiB"),
            "the bound says what it is: {dropped}"
        );
    }

    #[test]
    fn the_bytes_bound_drops_older_results_before_the_count_does() {
        let mut history = History::default();
        let big = "x".repeat(BYTES / 4);
        for _ in 0..6 {
            history.record(call("crawl"), text_result(&big), big.clone());
        }
        assert!(history.weight <= BYTES, "{} bytes kept", history.weight);
        assert!(
            history.kept.len() < 6,
            "{} results kept",
            history.kept.len()
        );
        assert_eq!(history.find("_").unwrap().number, 6);
    }

    #[test]
    fn one_result_larger_than_the_whole_budget_is_still_the_last_one() {
        let mut history = History::default();
        history.record(call("small"), text_result("a"), "a".into());
        let huge = "x".repeat(BYTES * 2);
        history.record(call("crawl"), text_result(&huge), huge);
        assert_eq!(history.kept.len(), 1, "_ has to name something");
        assert_eq!(history.find("_").unwrap().number, 2);
    }

    #[test]
    fn retry_finds_the_last_call_of_a_tool_or_of_any_tool() {
        let mut history = History::default();
        history.sending("navigate", &json!({"url": "a"}));
        history.sending("screenshot", &json!({}));
        history.sending("navigate", &json!({"url": "b"}));
        assert_eq!(history.last_call(None).unwrap().0, "navigate");
        assert_eq!(history.last_call(Some("navigate")).unwrap().1["url"], "b");
        assert_eq!(history.last_call(Some("screenshot")).unwrap().1, &json!({}));
        assert!(history.last_call(Some("nope")).is_none());
    }

    #[test]
    fn a_retry_changes_one_argument_and_keeps_the_rest() {
        let base = json!({"url": "a", "timeout": 30});
        let out = merged(&base, &json!({"url": "b"}));
        assert_eq!(out, json!({"url": "b", "timeout": 30}));
        assert_eq!(merged(&base, &no_arguments()), base);
    }

    #[test]
    fn a_file_is_named_after_what_made_the_result() {
        let mut history = History::default();
        history.record(call("take_screenshot"), text_result("x"), "x".into());
        history.record(
            Origin::Read {
                uri: "file:///notes/today.md".into(),
            },
            json!({"contents": [{"uri": "file:///notes/today.md", "mimeType": "text/markdown",
                                 "text": "# today"}]}),
            String::new(),
        );
        history.record(
            Origin::Raw {
                method: "tools/list".into(),
            },
            json!({"tools": []}),
            String::new(),
        );

        let shot = history.find("1").unwrap();
        let keep = shot.keep(false).unwrap();
        assert!(matches!(keep, Keep::Text));
        assert_eq!(shot.filename(&keep), "take_screenshot.txt");
        assert_eq!(
            shot.filename(&shot.keep(true).unwrap()),
            "take_screenshot.json"
        );

        let note = history.find("2").unwrap();
        let keep = note.keep(false).unwrap();
        assert_eq!(note.filename(&keep), "today.markdown");

        let listing = history.find("3").unwrap();
        let keep = listing.keep(false).unwrap();
        assert!(matches!(keep, Keep::Document), "raw has no other form");
        assert_eq!(listing.filename(&keep), "tools_list.json");
    }

    #[test]
    fn a_binary_block_is_saved_as_its_bytes_under_the_extension_its_type_asks_for() {
        let mut history = History::default();
        history.record(
            call("shot"),
            json!({"content": [{"type": "image", "mimeType": "image/png",
                                "data": "iVBORw0KGgo="}]}),
            "[image image/png, 8 B]".into(),
        );
        let shot = history.find("_").unwrap();
        let keep = shot.keep(false).unwrap();
        match &keep {
            Keep::Bytes { bytes, .. } => assert_eq!(&bytes[..4], b"\x89PNG"),
            _ => panic!("a block with bytes is saved as its bytes"),
        }
        assert_eq!(shot.filename(&keep), "shot.png");
        // Under --json the whole object goes out, blocks and all.
        assert!(matches!(shot.keep(true).unwrap(), Keep::Document));
    }
}
