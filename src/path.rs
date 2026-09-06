//! The `| ...` a shell line can end with: a small path expression, or `jq`.
//!
//! Most of what anyone asks of a result is one field of it, and a grammar of
//! four forms - `.key`, `.key.sub`, `.[0]`, `.[]` - covers that without a crate,
//! a query language or a second way to be wrong. Everything past those forms is
//! what `jq` is for, and the people who want it already have it installed, so
//! `| jq FILTER` hands the result to that program rather than growing this one.
//!
//! The filter is applied to what the result is actually about: a server's
//! `structuredContent` where it sent one, the text where that text is JSON, and
//! the result object itself otherwise. Nothing here fails on a shape it did not
//! expect - a key an object does not have, an index past the end of an array,
//! and any step at all asked of a scalar each yield no values, so a path that
//! matches nothing prints nothing and says so on stderr.
//!
//! `jq` is run as a program, never as a command line: the words after `jq` are
//! its argv and the result goes in on stdin, so no shell ever sees either, and
//! no byte a server sent is ever parsed as anything but data.

use crate::pick::on_path;
use mcpdial::transport::stdio::split_command;
use mcpdial::Error;
use serde_json::Value;
use std::borrow::Cow;
use std::io::Write;
use std::process::{Command, Stdio};

/// The whole grammar, said outright: it is shorter than a description of it.
pub const USAGE: &str = "usage: COMMAND | .a.b[0]   \
     (.key, .[0], .[] and combinations; | jq FILTER for anything more)";

/// The one program this ever runs, and only when a line names it.
const JQ: &str = "jq";

/// What `| jq` does with no filter after it: print the result, which is what
/// `jq` alone is usually reached for.
const IDENTITY: &str = ".";

/// One step of a path: a key, an index, or every element of what is there.
#[derive(Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
    Every,
}

/// A path expression, and the text it was written as, so that an expression
/// matching nothing can say which one did.
#[derive(Debug)]
pub struct Path {
    steps: Vec<Step>,
    source: String,
}

/// The `| ...` at the end of a shell line.
#[derive(Debug)]
pub enum Filter {
    Path(Path),
    /// The words after `jq`, each one argument to the program of that name.
    Jq(Vec<String>),
}

/// What a filter made of a result.
#[derive(Debug)]
pub struct Filtered {
    /// The lines to print, or nothing where the filter selected nothing at all.
    pub text: Option<String>,
    /// One line for stderr beside them: a path that named nothing, or what `jq`
    /// warned about while still succeeding. Empty when there is nothing to say.
    pub note: String,
}

impl Filter {
    /// The expression after the bar, read before the command in front of it
    /// runs, so that a filter nobody can read costs no request.
    pub fn parse(expression: &str) -> Result<Self, Error> {
        let text = expression.trim();
        let (head, tail) = text
            .split_once(char::is_whitespace)
            .unwrap_or((text, IDENTITY));
        if head == JQ {
            let args = split_command(tail)
                .map_err(|_| Error::usage(format!("unterminated quote in filter {text:?}")))?;
            return Ok(Filter::Jq(args));
        }
        parse(text).map(Filter::Path)
    }

    /// `result` through the filter. `json` decides only how a selected value is
    /// written: a line under `--json` has to be a document a reader can parse,
    /// so strings keep their quotes there and lose them everywhere else.
    pub fn apply(&self, result: &Value, json: bool) -> Result<Filtered, Error> {
        let subject = subject(result);
        match self {
            Filter::Path(path) => {
                let found = select(path, &subject);
                Ok(Filtered {
                    note: match found.is_empty() {
                        true => format!("{} matched nothing in this result", path.source),
                        false => String::new(),
                    },
                    text: (!found.is_empty()).then(|| lines(&found, json)),
                })
            }
            Filter::Jq(args) => {
                let said = jq(args, &subject)?;
                Ok(Filtered {
                    // What jq wrote, as its lines: the trailing newline is the
                    // presenter's to add, and a jq built for Windows ends every
                    // line with a carriage return that is its stdout's doing
                    // rather than anything in the result.
                    text: (!said.out.is_empty()).then(|| lines_of(&said.out)),
                    note: said.warning,
                })
            }
        }
    }
}

/// What a filter is applied to: the `structuredContent` a server sent, else its
/// text where that text is a JSON document, else the result object as it came.
/// A tool that answers in JSON is the one worth filtering, and it may say so
/// either way, so both are the thing itself rather than a wrapper around it.
fn subject(result: &Value) -> Cow<'_, Value> {
    if let Some(structured) = result.get("structuredContent") {
        return Cow::Borrowed(structured);
    }
    match text_json(result) {
        Some(parsed) => Cow::Owned(parsed),
        None => Cow::Borrowed(result),
    }
}

/// The text blocks of a result, parsed, while together they are one JSON
/// document. Two documents in a row are not one, and neither is prose, so both
/// leave the result object as the thing to filter.
fn text_json(result: &Value) -> Option<Value> {
    let blocks = result["content"].as_array()?;
    let text: Vec<&str> = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(text.join("\n").trim()).ok()
}

/// The expression as steps. It has to start at the top of the result, which is
/// what the leading `.` says, and `.` alone is the whole of it.
fn parse(expression: &str) -> Result<Path, Error> {
    let source = expression.trim();
    if !source.starts_with('.') {
        return Err(not_a_path(source));
    }
    let mut steps = Vec::new();
    let mut rest = source;
    while !rest.is_empty() {
        rest = match rest.as_bytes()[0] {
            b'.' => {
                let after = &rest[1..];
                let key: String = after.chars().take_while(|c| is_key_char(*c)).collect();
                if key.is_empty() {
                    // A bare `.`, and the `.` before a `[`, are the same nothing.
                    if !after.is_empty() && !after.starts_with('[') {
                        return Err(not_a_path(source));
                    }
                    after
                } else {
                    let after = &after[key.len()..];
                    steps.push(Step::Key(key));
                    after
                }
            }
            b'[' => {
                let (inside, after) = rest[1..]
                    .split_once(']')
                    .ok_or_else(|| not_a_path(source))?;
                steps.push(match inside.trim() {
                    "" => Step::Every,
                    n => Step::Index(n.parse().map_err(|_| not_a_path(source))?),
                });
                after
            }
            _ => return Err(not_a_path(source)),
        };
    }
    Ok(Path {
        steps,
        source: source.to_string(),
    })
}

/// What a key may be written as without quotes. Anything else is a key `jq`
/// can reach and this cannot, which the refusal says.
fn is_key_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

fn not_a_path(expression: &str) -> Error {
    Error::usage(format!(
        "{expression:?} is not a path; a filter is .key, .key.sub, .[0], .[] \
         or a combination of those, with indexes counting from 0"
    ))
}

/// Every value the path names. Each step maps the values it is given to the
/// values it finds in them, so a step that finds nothing in one of them drops
/// that one and a path that finds nothing anywhere yields an empty list rather
/// than an error: `.[]` over a list of objects missing a key is the ordinary
/// case, not a mistake.
fn select<'a>(path: &Path, value: &'a Value) -> Vec<&'a Value> {
    let mut found = vec![value];
    for step in &path.steps {
        let mut next = Vec::new();
        for value in found {
            match step {
                Step::Key(key) => next.extend(value.get(key)),
                Step::Index(at) => next.extend(value.get(at)),
                Step::Every => match value {
                    Value::Array(items) => next.extend(items),
                    Value::Object(fields) => next.extend(fields.values()),
                    _ => {}
                },
            }
        }
        found = next;
    }
    found
}

/// The selected values, one per line: a string as the text it holds, since a
/// filter is reached for to get a value out of the quotes, and anything else as
/// the one line of JSON it is.
fn lines(found: &[&Value], json: bool) -> String {
    found
        .iter()
        .map(|value| match value.as_str() {
            Some(text) if !json => text.to_string(),
            _ => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines of what a program wrote, joined the way selected values are, so
/// that both filters print one value per line whatever wrote them.
fn lines_of(text: &str) -> String {
    text.lines().collect::<Vec<_>>().join("\n")
}

/// What `jq` said.
struct Said {
    out: String,
    /// What it wrote to stderr while still succeeding, which is a warning and
    /// not something to swallow.
    warning: String,
}

/// The result through `jq`.
///
/// The filter is `args`, each word its own argv entry, and the result goes in
/// on stdin: no shell is started and nothing is ever concatenated into a
/// command line, so a result whose text reads `; rm -rf ~` is a string to `jq`
/// and nothing at all to anything else. The input is written from a thread of
/// its own, because a filter that prints more than a pipe holds would otherwise
/// wait on a reader that is itself waiting to finish writing.
fn jq(args: &[String], data: &Value) -> Result<Said, Error> {
    let Some(program) = on_path(JQ) else {
        return Err(Error::usage(format!(
            "{JQ} is not installed; the built-in filter reads .key, .[0] and .[]"
        )));
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::transport(format!("could not run {JQ}: {e}")))?;
    let document = serde_json::to_vec(data).map_err(|e| Error::usage(format!("{JQ}: {e}")))?;
    let mut stdin = child.stdin.take().expect("jq's stdin was piped");
    // A filter that stops reading - `jq -n`, `jq first(.[])` - closes the pipe,
    // and a write that lands on a closed pipe is that filter's answer, not an
    // error of its own.
    let feeding = std::thread::spawn(move || stdin.write_all(&document).is_ok());
    let done = child
        .wait_with_output()
        .map_err(|e| Error::transport(format!("{JQ}: {e}")))?;
    drop(feeding.join());
    let said = String::from_utf8_lossy(&done.stderr).trim().to_string();
    if !done.status.success() {
        let code = done
            .status
            .code()
            .map_or_else(|| "killed".to_string(), |c| format!("exit {c}"));
        return Err(Error::usage(match said.is_empty() {
            true => format!("{JQ} failed ({code})"),
            false => format!(
                "{JQ} failed ({code}): {}",
                said.lines().next().unwrap_or("")
            ),
        }));
    }
    Ok(Said {
        out: String::from_utf8_lossy(&done.stdout).into_owned(),
        warning: said,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn found(expression: &str, value: &Value) -> Vec<Value> {
        let path = parse(expression).expect("a path this test wrote");
        select(&path, value).into_iter().cloned().collect()
    }

    fn printed(expression: &str, value: &Value) -> String {
        let filter = Filter::parse(expression).expect("a filter this test wrote");
        filter
            .apply(value, false)
            .expect("a path filter runs nothing")
            .text
            .unwrap_or_default()
    }

    fn pages() -> Value {
        json!({"pages": [
            {"url": "https://example.com/", "id": 1},
            {"url": "https://example.com/about", "id": 2},
        ]})
    }

    #[test]
    fn each_form_of_the_grammar_selects_what_it_names() {
        let v = pages();
        assert_eq!(found(".", &v), vec![v.clone()], "`.` is the whole of it");
        assert_eq!(found(".pages[0].id", &v), vec![json!(1)]);
        assert_eq!(found(".pages[1]", &v), vec![v["pages"][1].clone()]);
        assert_eq!(
            found(".pages[].url", &v),
            vec![
                json!("https://example.com/"),
                json!("https://example.com/about")
            ]
        );
        assert_eq!(found(".pages[].id", &v), vec![json!(1), json!(2)]);
        assert_eq!(
            found(".a.b.c", &json!({"a": {"b": {"c": "deep"}}})),
            vec![json!("deep")]
        );
        assert_eq!(
            found(".[]", &json!([1, 2, 3])),
            vec![json!(1), json!(2), json!(3)]
        );
        assert_eq!(
            found(".[]", &json!({"a": 1, "b": 2})),
            vec![json!(1), json!(2)],
            "every value of an object, the way jq reads it"
        );
        assert_eq!(
            found(".[][]", &json!([[1], [2, 3]])),
            vec![json!(1), json!(2), json!(3)]
        );
        assert_eq!(
            found(".is-set_2", &json!({"is-set_2": true})),
            vec![json!(true)],
            "a key is letters, digits, _ and -"
        );
    }

    #[test]
    fn a_step_that_names_nothing_yields_nothing_rather_than_failing() {
        let v = pages();
        assert!(found(".nope", &v).is_empty(), "a key nothing has");
        assert!(found(".pages[9]", &v).is_empty(), "an index past the end");
        assert!(found(".pages[0].nope", &v).is_empty(), "and one step in");
        assert!(
            found(".pages.url", &v).is_empty(),
            "a key asked of an array"
        );
        assert!(
            found(".pages[0].url[]", &v).is_empty(),
            "every element of a string"
        );
        for step in [".key", ".[0]", ".[]"] {
            for scalar in [json!(42), json!("text"), json!(null), json!(true)] {
                assert!(
                    found(step, &scalar).is_empty(),
                    "{step} of the scalar {scalar} is nothing"
                );
            }
        }
        assert_eq!(
            found(".", &json!(42)),
            vec![json!(42)],
            "but the whole of a scalar is still the scalar"
        );
    }

    #[test]
    fn an_expression_outside_the_grammar_is_refused_by_name() {
        for bad in [
            "",
            "pages",
            "..",
            "..pages",
            ".pages[",
            ".pages[x]",
            ".pages[-1]",
            ".pages[0",
            ".a b",
            ".\"quoted key\"",
            ".pages | length",
            ".a?",
        ] {
            let e = parse(bad).unwrap_err();
            assert!(matches!(e, Error::Usage(_)), "{bad:?} gave {e}");
            let said = e.to_string();
            assert!(said.contains(".key"), "{bad:?} says the forms: {said}");
            assert!(said.contains(".[]"), "{bad:?} says the forms: {said}");
        }
    }

    #[test]
    fn a_filter_reads_what_the_result_is_about() {
        let structured = json!({
            "content": [{"type": "text", "text": "1 page open"}],
            "structuredContent": {"pages": [{"url": "https://example.com/"}]},
        });
        assert_eq!(printed(".pages[].url", &structured), "https://example.com/");

        let json_text = json!({"content": [{"type": "text", "text": r#"{"sum": 3}"#}]});
        assert_eq!(printed(".sum", &json_text), "3");

        let prose = json!({"content": [{"type": "text", "text": "The sum is 3."}]});
        assert_eq!(
            printed(".content[0].text", &prose),
            "The sum is 3.",
            "text that is not JSON leaves the result object as the subject"
        );

        let raw = json!({"tools": [{"name": "echo"}, {"name": "add"}]});
        assert_eq!(printed(".tools[].name", &raw), "echo\nadd");
    }

    #[test]
    fn a_string_loses_its_quotes_and_everything_else_keeps_its_json() {
        let v = json!({"a": "text", "b": 2, "c": {"d": true}, "e": [1], "f": null});
        assert_eq!(printed(".a", &v), "text");
        assert_eq!(printed(".b", &v), "2");
        assert_eq!(printed(".c", &v), r#"{"d":true}"#);
        assert_eq!(printed(".e", &v), "[1]");
        assert_eq!(printed(".f", &v), "null");
        let quoted = Filter::parse(".a")
            .unwrap()
            .apply(&v, true)
            .unwrap()
            .text
            .unwrap();
        assert_eq!(quoted, r#""text""#, "--json keeps a line parseable");
    }

    #[test]
    fn a_path_that_matches_nothing_prints_nothing_and_says_which_one_did() {
        let filtered = Filter::parse(".nope[].url")
            .unwrap()
            .apply(&pages(), false)
            .unwrap();
        assert!(filtered.text.is_none(), "nothing at all on stdout");
        assert_eq!(filtered.note, ".nope[].url matched nothing in this result");
    }

    #[test]
    fn jq_is_a_program_with_arguments_never_a_command_line() {
        let Filter::Jq(args) = Filter::parse("jq -r '.pages[] | .url'").unwrap() else {
            panic!("a filter starting with jq is a jq filter");
        };
        assert_eq!(
            args,
            vec!["-r", ".pages[] | .url"],
            "quotes make one argument"
        );
        let Filter::Jq(args) = Filter::parse("jq").unwrap() else {
            panic!("jq alone is still jq");
        };
        assert_eq!(args, vec![IDENTITY], "jq with no filter prints the result");
        assert!(
            matches!(Filter::parse("jq 'unterminated"), Err(Error::Usage(_))),
            "a quote nobody closed is answered here, not by jq"
        );
        // `jqx` is nobody's jq: a name that merely starts with those letters is
        // read as a path, and refused as one.
        assert!(matches!(Filter::parse("jqx ."), Err(Error::Usage(_))));
    }

    /// A jq built for Windows writes its lines with a carriage return, which is
    /// its stdout's doing and not part of any value it selected.
    #[test]
    fn a_filter_that_ends_its_lines_the_way_windows_does_prints_the_values_alone() {
        assert_eq!(lines_of("a\r\nb\r\n"), "a\nb");
        assert_eq!(lines_of("a\nb\n"), "a\nb");
        assert_eq!(lines_of("only\n"), "only");
        assert_eq!(lines_of("\n"), "", "a blank line is still a line");
        assert_eq!(lines_of("a\rb\n"), "a\rb", "a return inside a line is data");
    }

    /// Only where jq is installed, which is the only place this path runs.
    #[test]
    fn jq_takes_the_result_on_stdin_and_a_failure_of_its_own_is_reported_as_one() {
        if on_path(JQ).is_none() {
            return;
        }
        let v = pages();
        let out = Filter::parse("jq -r '.pages[].url'")
            .unwrap()
            .apply(&v, false)
            .unwrap();
        assert_eq!(
            out.text.unwrap(),
            "https://example.com/\nhttps://example.com/about"
        );

        // A result whose contents would be a command line if anything ever
        // built one. They reach jq as a string and come back as a string.
        let hostile =
            json!({"content": [{"type": "text", "text": "; touch /tmp/mcpdial-jq-test"}]});
        let out = Filter::parse("jq -r '.content[0].text'")
            .unwrap()
            .apply(&hostile, false)
            .unwrap();
        assert_eq!(out.text.unwrap(), "; touch /tmp/mcpdial-jq-test");
        assert!(!std::path::Path::new("/tmp/mcpdial-jq-test").exists());

        let e = Filter::parse("jq '.pages['")
            .unwrap()
            .apply(&v, false)
            .unwrap_err();
        assert!(e.to_string().starts_with("jq failed"), "{e}");

        // A filter that reads none of its input still gets its answer out.
        let out = Filter::parse("jq -n 42").unwrap().apply(&v, false).unwrap();
        assert_eq!(out.text.unwrap(), "42");
    }

    /// A megabyte through jq: the input is written from its own thread, so a
    /// filter printing more than one pipe holds cannot wait on a writer that is
    /// waiting on it.
    #[test]
    fn a_result_larger_than_a_pipe_goes_through_without_either_side_waiting() {
        if on_path(JQ).is_none() {
            return;
        }
        let big = json!({"rows": (0..40_000).map(|n| json!({"n": n})).collect::<Vec<_>>()});
        let out = Filter::parse("jq -c '.rows[]'")
            .unwrap()
            .apply(&big, false)
            .unwrap();
        assert_eq!(out.text.unwrap().lines().count(), 40_000);
    }
}
