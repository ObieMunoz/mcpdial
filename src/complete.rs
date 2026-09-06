//! What Tab offers at the shell prompt.
//!
//! Three pools, in the order a line is typed: the command names, then whatever
//! the command that was typed takes as its one argument, then — inside the
//! arguments object of a `call` — the property names of the tool's `inputSchema`
//! and the values an `enum` allows.
//!
//! An arguments object under the cursor is JSON that is not finished yet, so it
//! cannot be parsed; it is scanned instead, far enough to say which object the
//! cursor is in, whether a key or a value is being typed, and which keys the
//! object already has. Anything the scan cannot make sense of offers nothing,
//! which is what an unfinished line deserves.
//!
//! None of this reaches an agent. rustyline is built only for the terminal arm of
//! the shell's reader, which needs a terminal on both stdin and stdout; a piped
//! session reads lines itself and never constructs a helper, let alone calls one.

use crate::SHELL_COMMANDS;
use mcpdial::schema;
use serde_json::Value;

/// Objects deep a key is completed: the arguments object, and one object nested
/// in it. Deeper than that, a line typed by hand is long enough that the reader
/// is better served by `schema TOOL`.
const NESTING: usize = 1;

/// Tab completion for the shell.
#[derive(Default)]
pub struct ShellHelper {
    /// Whole tool objects, not just names: the names complete after `call`, and
    /// the same objects' `inputSchema` completes what goes inside the arguments.
    pub tools: Vec<Value>,
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
}

impl rustyline::completion::Completer for ShellHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let head = line.get(..pos).unwrap_or(line);
        Ok(self
            .inside_arguments(head)
            .unwrap_or_else(|| self.word(head)))
    }
}

impl rustyline::highlight::Highlighter for ShellHelper {}
impl rustyline::validate::Validator for ShellHelper {}
impl rustyline::hint::Hinter for ShellHelper {
    type Hint = String;
}
impl rustyline::Helper for ShellHelper {}

impl ShellHelper {
    /// The last word of the line as a command name, a tool, a resource URI or a
    /// prompt, depending on what came before it.
    fn word(&self, head: &str) -> (usize, Vec<String>) {
        let start = head
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let (before, word) = head.split_at(start);
        let pool: Vec<String> = match before.split_whitespace().collect::<Vec<_>>()[..] {
            [] => SHELL_COMMANDS.iter().map(|c| c.to_string()).collect(),
            ["call" | "schema" | "help"] => self.tool_names(),
            ["read"] => self.resources.clone(),
            ["prompt"] => self.prompts.clone(),
            _ => Vec::new(),
        };
        (
            start,
            pool.into_iter().filter(|c| c.starts_with(word)).collect(),
        )
    }

    fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .map(String::from)
            .collect()
    }

    /// What may go where the cursor is inside a `call`'s arguments object, or
    /// `None` when the cursor is not in one.
    fn inside_arguments(&self, head: &str) -> Option<(usize, Vec<String>)> {
        let (name, arguments, at) = call_arguments(head)?;
        let tool = self.tools.iter().find(|t| t["name"] == name)?;
        let opening = Opening::read(arguments)?;
        let object = opening.enclosing(&tool["inputSchema"])?;
        Some((at + opening.start, opening.fill(object)))
    }
}

/// The tool and the arguments text of a `call` line, and the byte of the line
/// the arguments start at. `None` for every other line, a `call` whose tool name
/// is still being typed included.
fn call_arguments(head: &str) -> Option<(&str, &str, usize)> {
    let (command, rest, at) = word_then_rest(head)?;
    if command != "call" {
        return None;
    }
    let (tool, arguments, offset) = word_then_rest(rest)?;
    Some((tool, arguments, at + offset))
}

/// The first word of `text`, what follows the whitespace after it, and the byte
/// that follows starts at. `None` when there is no whitespace after the word,
/// which is to say the word itself is still being typed.
fn word_then_rest(text: &str) -> Option<(&str, &str, usize)> {
    let begins = text.find(|c: char| !c.is_whitespace())?;
    let word = &text[begins..];
    let ends = word.find(char::is_whitespace)?;
    let next = word[ends..].find(|c: char| !c.is_whitespace())? + ends;
    Some((&word[..ends], &word[next..], begins + next))
}

/// One container the scan is inside.
enum Frame {
    Object {
        /// The key in the enclosing object whose value this object is, which is
        /// how the schema for it is found. `None` for the arguments object.
        of: Option<String>,
        /// The keys this object already has, which are not offered a second time.
        written: Vec<String>,
        /// The key whose value is being written, once a `:` has been typed.
        key: Option<String>,
        after_colon: bool,
    },
    Array,
}

/// Where a half-typed arguments object leaves the cursor.
struct Opening {
    /// The byte of the arguments text the token being completed starts at, which
    /// is the opening quote when there is one.
    start: usize,
    /// That token so far, with its quotes taken off.
    typed: String,
    /// The keys leading to the object the cursor is in, outermost first. Empty
    /// when that object is the arguments object itself.
    path: Vec<String>,
    /// The key whose value is being typed, or `None` when the key is.
    value_of: Option<String>,
    written: Vec<String>,
}

impl Opening {
    /// A scan of unfinished JSON, which stops at anything it cannot read rather
    /// than guessing: an unbalanced `}`, an array, a nesting deeper than
    /// [`NESTING`], or a cursor sitting just past a value that is already whole.
    fn read(text: &str) -> Option<Self> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut open: Option<(usize, String, bool)> = None;
        let mut closed: Option<String> = None;
        let mut escaped = false;
        for (at, c) in text.char_indices() {
            if let Some((_, buf, quoted)) = open.as_mut() {
                if *quoted {
                    match c {
                        _ if escaped => {
                            escaped = false;
                            buf.push(c);
                        }
                        '\\' => escaped = true,
                        '"' => {
                            closed = Some(std::mem::take(buf));
                            open = None;
                        }
                        _ => buf.push(c),
                    }
                    continue;
                }
                // A bare word or number runs until anything structural.
                if !c.is_whitespace() && !"{}[],:\"".contains(c) {
                    buf.push(c);
                    continue;
                }
                closed = Some(std::mem::take(buf));
                open = None;
            }
            match c {
                _ if c.is_whitespace() => continue,
                '"' => open = Some((at, String::new(), true)),
                '{' => {
                    let of = match stack.last() {
                        Some(Frame::Object {
                            key,
                            after_colon: true,
                            ..
                        }) => key.clone(),
                        _ => None,
                    };
                    stack.push(Frame::Object {
                        of,
                        written: Vec::new(),
                        key: None,
                        after_colon: false,
                    });
                }
                '[' => stack.push(Frame::Array),
                '}' | ']' => {
                    stack.pop()?;
                }
                ':' => {
                    if let Some(Frame::Object {
                        key, after_colon, ..
                    }) = stack.last_mut()
                    {
                        *key = closed.clone();
                        *after_colon = true;
                    }
                }
                ',' => {
                    if let Some(Frame::Object {
                        written,
                        key,
                        after_colon,
                        ..
                    }) = stack.last_mut()
                    {
                        written.extend(key.take());
                        *after_colon = false;
                    }
                }
                _ => open = Some((at, c.to_string(), false)),
            }
            closed = None;
        }

        let mut path = Vec::new();
        for frame in &stack {
            match frame {
                Frame::Object { of, .. } => path.extend(of.clone()),
                Frame::Array => return None,
            }
        }
        if path.len() > NESTING {
            return None;
        }
        let Some(Frame::Object {
            written,
            key,
            after_colon,
            ..
        }) = stack.last()
        else {
            return None;
        };
        let value_of = match (after_colon, key) {
            (true, Some(key)) => Some(key.clone()),
            (true, None) => return None,
            (false, _) => None,
        };
        let (start, typed) = match open {
            Some((at, text, _)) => (at, text),
            // The cursor sits just past a finished string or number, where the
            // next thing to type is punctuation and no pool has anything to say.
            None if closed.is_some() => return None,
            None => (text.len(), String::new()),
        };
        Some(Opening {
            start,
            typed,
            path,
            value_of,
            written: written.clone(),
        })
    }

    /// The schema of the object the cursor is in, walked down from the tool's
    /// own `inputSchema` through the keys that lead to it.
    fn enclosing<'a>(&self, input: &'a Value) -> Option<&'a Value> {
        self.path
            .iter()
            .try_fold(input, |spec, key| schema::properties(spec)?.get(key))
    }

    /// The candidates for the cursor: a value's allowed values, else the keys
    /// the object has not been given yet, each with the `: ` that follows it.
    fn fill(&self, object: &Value) -> Vec<String> {
        let properties = schema::properties(object);
        let Some(key) = &self.value_of else {
            return properties
                .into_iter()
                .flat_map(|props| props.keys())
                .filter(|name| !self.written.contains(name) && self.matches(name))
                .map(|name| format!("{}: ", Value::String(name.clone())))
                .collect();
        };
        properties
            .and_then(|props| props.get(key))
            .map(schema::allowed_values)
            .unwrap_or_default()
            .into_iter()
            .filter(|value| self.matches(&content(value)))
            .map(Value::to_string)
            .collect()
    }

    /// Whether a candidate is what is being typed. The comparison is against the
    /// candidate's content rather than its JSON form, so that a value reached
    /// with the opening quote typed and one reached without both find it.
    fn matches(&self, content: &str) -> bool {
        content.starts_with(&self.typed)
    }
}

/// A value as it reads without its quotes: `fast` for `"fast"`, `5` for `5`.
fn content(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::completion::Completer;
    use rustyline::history::DefaultHistory;
    use serde_json::json;

    fn complete(helper: &ShellHelper, line: &str) -> (usize, Vec<String>) {
        let history = DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        helper.complete(line, line.len(), &ctx).unwrap()
    }

    fn named(names: [&str; 3]) -> Vec<Value> {
        names.iter().map(|n| json!({"name": n})).collect()
    }

    /// A server whose one tool has a required string, an optional enum, and one
    /// nested object with an enum of its own.
    fn browser() -> ShellHelper {
        ShellHelper {
            tools: vec![json!({
                "name": "new_page",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "user": {"type": "string"},
                        "wait": {"type": "string", "enum": ["load", "idle", "never"]},
                        "retries": {"enum": [1, 2, 3]},
                        "viewport": {
                            "type": "object",
                            "properties": {
                                "width": {"type": "number"},
                                "scale": {"enum": ["fit", "fill"]},
                                "inner": {"type": "object", "properties": {"deep": {}}},
                            },
                        },
                    },
                    "required": ["url"],
                },
            })],
            ..ShellHelper::default()
        }
    }

    #[test]
    fn completes_commands_then_tool_and_prompt_names_and_resource_uris() {
        let helper = ShellHelper {
            tools: named(["list_pages", "list_console_messages", "new_page"]),
            resources: ["file:///a.md", "file:///b.png"].map(String::from).to_vec(),
            prompts: ["summarize", "translate"].map(String::from).to_vec(),
        };
        // The first word is a command.
        let (at, found) = complete(&helper, "sch");
        assert_eq!((at, found), (0, vec!["schema".to_string()]));
        // The argument to these three is a tool, and completion starts at it.
        let (at, found) = complete(&helper, "call list_");
        assert_eq!(at, 5);
        assert_eq!(found, ["list_pages", "list_console_messages"]);
        assert_eq!(complete(&helper, "schema new").1, ["new_page"]);
        assert_eq!(complete(&helper, "help li").1.len(), 2);
        // Each of the other two pools answers to its own command.
        assert_eq!(complete(&helper, "read file:///b").1, ["file:///b.png"]);
        assert_eq!(complete(&helper, "prompt sum").1, ["summarize"]);
        assert!(complete(&helper, "read sum").1.is_empty());
        // Nothing to say about other commands, or about a tool with no schema.
        assert!(complete(&helper, "raw tools/").1.is_empty());
        assert!(complete(&helper, "call new_page {\"ur").1.is_empty());
        // A server that lists no tools simply offers nothing.
        assert!(complete(&ShellHelper::default(), "call li").1.is_empty());
    }

    #[test]
    fn completes_a_key_from_the_quote_it_was_started_with() {
        let helper = browser();
        // The candidate replaces the opening quote too, so that it is one token.
        let (at, found) = complete(&helper, "call new_page {\"u");
        assert_eq!(at, 15);
        assert_eq!(found, [r#""url": "#, r#""user": "#]);
        // An object just opened, with or without a quote, offers every key.
        assert_eq!(complete(&helper, "call new_page {").1.len(), 5);
        assert_eq!(complete(&helper, "call new_page { \"").1.len(), 5);
        // A key already written is not offered again.
        let (at, found) = complete(&helper, "call new_page {\"url\": \"x\", \"");
        assert_eq!(at, 27);
        assert_eq!(
            found,
            [
                r#""retries": "#,
                r#""user": "#,
                r#""viewport": "#,
                r#""wait": "#
            ]
        );
        assert_eq!(
            complete(
                &helper,
                "call new_page {\"url\": \"x\", \"user\": \"y\", \"w"
            )
            .1,
            [r#""wait": "#]
        );
        // Whitespace and an unfinished pair do not move where the token starts.
        let (at, found) = complete(&helper, "call new_page   {  \"url\" : \"x\" ,  \"vi");
        assert_eq!((at, found), (34, vec![r#""viewport": "#.to_string()]));
    }

    #[test]
    fn completes_the_values_an_enum_allows_and_nothing_where_it_says_nothing() {
        let helper = browser();
        // With the quote typed, and without it: both reach the same values.
        let (at, found) = complete(&helper, "call new_page {\"wait\": \"i");
        assert_eq!((at, found), (23, vec![r#""idle""#.to_string()]));
        assert_eq!(
            complete(&helper, "call new_page {\"wait\": i").1,
            [r#""idle""#]
        );
        assert_eq!(complete(&helper, "call new_page {\"wait\": ").1.len(), 3);
        assert_eq!(complete(&helper, "call new_page {\"wait\":").1.len(), 3);
        // A non-string enum keeps its own JSON form.
        assert_eq!(
            complete(&helper, "call new_page {\"retries\": ").1,
            ["1", "2", "3"]
        );
        // A value the schema does not name is the caller's to write.
        assert!(complete(&helper, "call new_page {\"url\": \"htt")
            .1
            .is_empty());
        assert!(complete(&helper, "call new_page {\"nope\": ").1.is_empty());
    }

    #[test]
    fn completes_one_object_in_and_stops_there() {
        let helper = browser();
        let (at, found) = complete(&helper, "call new_page {\"viewport\": {\"");
        assert_eq!(at, 28);
        assert_eq!(found, [r#""inner": "#, r#""scale": "#, r#""width": "#]);
        assert_eq!(
            complete(&helper, "call new_page {\"viewport\": {\"scale\": \"f").1,
            [r#""fit""#, r#""fill""#]
        );
        // Keys are counted per object: the outer one's are offered again after it.
        assert_eq!(
            complete(
                &helper,
                "call new_page {\"url\": \"x\", \"viewport\": {\"width\": 1, \"s"
            )
            .1,
            [r#""scale": "#]
        );
        // Two objects in is past what a typed line should be guessed at.
        assert!(
            complete(&helper, "call new_page {\"viewport\": {\"inner\": {\"")
                .1
                .is_empty()
        );
    }

    #[test]
    fn an_unfinished_line_that_cannot_be_read_offers_nothing() {
        let helper = browser();
        // Not a JSON object at all: pairs, and a tool still being named.
        assert!(complete(&helper, "call new_page url=").1.is_empty());
        assert_eq!(complete(&helper, "call new_pa").1, ["new_page"]);
        // Arrays are not walked into, nor is a brace that closed too often.
        assert!(complete(&helper, "call new_page {\"tags\": [\"")
            .1
            .is_empty());
        assert!(complete(&helper, "call new_page {\"url\": \"x\"}}{\"")
            .1
            .is_empty());
        // Just past a whole value there is punctuation to type, not a name.
        assert!(complete(&helper, "call new_page {\"url\"").1.is_empty());
        assert!(complete(&helper, "call new_page {\"url\": \"x\" ")
            .1
            .is_empty());
        // An escaped quote does not end the string it is inside.
        assert!(complete(&helper, "call new_page {\"url\": \"a\\\"b")
            .1
            .is_empty());
        // Only `call` has an arguments object; `schema` and `raw` do not.
        assert!(complete(&helper, "raw tools/call {\"").1.is_empty());
        assert!(complete(&helper, "schema new_page {\"").1.is_empty());
    }
}
