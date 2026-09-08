//! What Tab offers at the shell prompt.
//!
//! Three pools, in the order a line is typed: the command names, then whatever
//! the command that was typed takes as its one argument, then — for a `call` or
//! a `prompt` — the names that go inside its arguments object and the values
//! that go against them. That third pool answers where the arguments object has
//! not been opened yet as well as inside it, so that Tab writes the `{` rather
//! than waiting to be given one.
//!
//! An arguments object under the cursor is JSON that is not finished yet, so it
//! cannot be parsed; it is scanned instead, far enough to say which object the
//! cursor is in, whether a key or a value is being typed, and which pairs the
//! object already has. Anything the scan cannot make sense of offers nothing,
//! which is what an unfinished line deserves.
//!
//! Most of it is answered from what the session already knows: a tool's
//! `inputSchema`, a prompt's declared arguments, the resource URIs the server
//! listed. Two places have to ask the server itself, through [`Suggests`]: the
//! value of a prompt argument, and the variable of a resource template. Those
//! are a round trip inside a keystroke, so they are bounded and their failure is
//! silence — see [`Suggests::values`].
//!
//! None of this reaches an agent. rustyline is built only for the terminal arm of
//! the shell's reader, which needs a terminal on both stdin and stdout; a piped
//! session reads lines itself and never constructs a helper, let alone calls one.

use super::help::SHELL_COMMANDS;
use mcpdial::schema;
use serde_json::{json, Map, Value};
use std::rc::Rc;

/// Objects deep a key is completed: the arguments object, and one object nested
/// in it. Deeper than that, a line typed by hand is long enough that the reader
/// is better served by `schema TOOL`.
const NESTING: usize = 1;

/// A server's own answer to what goes here, which is `completion/complete`
/// against the open session.
///
/// The line editor hands a completer a `&self` and calls it in the middle of a
/// keystroke, so what implements this reaches the session by borrowing it from
/// wherever the shell lends it, and answers with nothing at all when the server
/// is slow, wedged or gone. Nothing here reports a failure: a message printed
/// here would land in the middle of the line being typed. `-v` traces every
/// message the session sends, this one included, which is where a failed
/// completion is to be read.
pub trait Suggests {
    /// What the server offers for `argument` of `reference`, given `context`.
    fn values(&self, reference: Value, argument: Value, context: Option<Value>) -> Vec<String>;
}

/// Tab completion for the shell.
#[derive(Default)]
pub struct ShellHelper {
    /// Whole tool objects, not just names: the names complete after `call`, and
    /// the same objects' `inputSchema` completes what goes inside the arguments.
    pub tools: Vec<Value>,
    pub resources: Vec<String>,
    /// Whole prompt objects, for the same reason: the names complete after
    /// `prompt`, and the same objects' `arguments` complete the keys of the
    /// object after the name.
    pub prompts: Vec<Value>,
    /// The `uriTemplate` of each resource template the server listed, whose
    /// variables `read` completes one at a time.
    pub templates: Vec<String>,
    /// Where a server's own suggestions come from, when it declares that it has
    /// any. `None` for a server without the `completions` capability, which
    /// leaves every pool the local one it always was.
    pub suggests: Option<Rc<dyn Suggests>>,
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
        let typed: Vec<&str> = before.split_whitespace().collect();
        // A resource URI is the one pool with a server behind it, so it narrows
        // itself rather than being narrowed against what was typed.
        if let ["read"] = typed[..] {
            return (start, self.resource_uris(word));
        }
        let pool: Vec<String> = match typed[..] {
            [] => SHELL_COMMANDS.iter().map(|c| c.to_string()).collect(),
            ["call" | "retry" | "schema" | "help"] => self.tool_names(),
            ["prompt"] => self.named(&self.prompts),
            _ => Vec::new(),
        };
        (
            start,
            pool.into_iter().filter(|c| c.starts_with(word)).collect(),
        )
    }

    fn tool_names(&self) -> Vec<String> {
        self.named(&self.tools)
    }

    fn named(&self, items: &[Value]) -> Vec<String> {
        items
            .iter()
            .filter_map(|t| t["name"].as_str())
            .map(String::from)
            .collect()
    }

    /// What `read` offers: the URIs the server listed, and each template it
    /// listed carried one step further — the rest of the literal the cursor sits
    /// in, or what the server suggests for the variable it sits in.
    fn resource_uris(&self, typed: &str) -> Vec<String> {
        let mut found: Vec<String> = self
            .resources
            .iter()
            .filter(|uri| uri.starts_with(typed))
            .cloned()
            .collect();
        for template in &self.templates {
            match Ahead::of(template, typed) {
                Some(Ahead::Literal(uri)) => found.push(uri),
                Some(Ahead::Variable(filling)) => {
                    found.extend(self.fill_template(template, &filling));
                }
                None => {}
            }
        }
        found
    }

    /// The URIs a template makes once the server has said what its variable can
    /// be. Whatever the template says after that variable comes along with each
    /// one, later variables included, so that Tab reaches those next.
    fn fill_template(&self, template: &str, filling: &Filling<'_>) -> Vec<String> {
        let Some(suggests) = &self.suggests else {
            return Vec::new();
        };
        suggests
            .values(
                json!({ "type": "ref/resource", "uri": template }),
                json!({ "name": filling.variable, "value": filling.typed }),
                Some(json!({ "arguments": object(&filling.settled) })),
            )
            .into_iter()
            .map(|value| format!("{}{value}{}", filling.before, filling.rest))
            .collect()
    }

    /// What may go where the cursor is in a `call`'s or a `prompt`'s arguments,
    /// or `None` when the cursor is not in them.
    fn inside_arguments(&self, head: &str) -> Option<(usize, Vec<String>)> {
        let (command, name, arguments, at) = arguments_of(head)?;
        match command {
            "call" => self.inside_call(name, arguments, at),
            "prompt" => self.inside_prompt(name, arguments, at),
            _ => None,
        }
    }

    /// A tool's arguments, which its `inputSchema` describes whole: the property
    /// names, and the values an `enum` allows.
    fn inside_call(&self, name: &str, arguments: &str, at: usize) -> Option<(usize, Vec<String>)> {
        let tool = self.tools.iter().find(|t| t["name"] == name)?;
        let input = &tool["inputSchema"];
        if arguments.is_empty() {
            // Nothing to replace: the candidates go in at the end of the line.
            return Some((at, opens_with(Opening::default().fill(input))));
        }
        let opening = Opening::read(arguments)?;
        let object = opening.enclosing(input)?;
        Some((at + opening.start, opening.fill(object)))
    }

    /// A prompt's arguments, which the prompt declares by name alone: the names
    /// are ours to offer, and only the server knows what may go against one.
    fn inside_prompt(
        &self,
        name: &str,
        arguments: &str,
        at: usize,
    ) -> Option<(usize, Vec<String>)> {
        let declared = self.prompt_arguments(name)?;
        if arguments.is_empty() {
            return Some((at, opens_with(Opening::default().unwritten(&declared))));
        }
        let opening = Opening::read(arguments)?;
        // Every argument of a prompt is a string, so there is no object nested
        // in the arguments to walk into.
        if !opening.path.is_empty() {
            return None;
        }
        let Some(argument) = &opening.value_of else {
            return Some((at + opening.start, opening.unwritten(&declared)));
        };
        let suggests = self.suggests.as_ref()?;
        let values = suggests.values(
            json!({ "type": "ref/prompt", "name": name }),
            json!({ "name": argument, "value": opening.typed }),
            Some(json!({ "arguments": object(&opening.settled) })),
        );
        // Not filtered against what was typed: the server was told what that
        // was and ranked its answer by it, and a server that matches loosely
        // meant to.
        let quoted = values.into_iter().map(|v| Value::String(v).to_string());
        Some((at + opening.start, quoted.collect()))
    }

    /// The names of one prompt's declared arguments, or `None` for a prompt this
    /// server never listed.
    fn prompt_arguments(&self, name: &str) -> Option<Vec<String>> {
        let prompt = self.prompts.iter().find(|p| p["name"] == name)?;
        Some(
            prompt["arguments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["name"].as_str())
                .map(String::from)
                .collect(),
        )
    }
}

/// The command a half-typed arguments object belongs to, the tool or prompt it
/// names, the arguments text, and the byte of the line the arguments start at.
/// `None` for every other line, and for one whose tool or prompt name is still
/// being typed.
fn arguments_of(head: &str) -> Option<(&str, &str, &str, usize)> {
    let (command, rest, at) = word_then_rest(head)?;
    if !matches!(command, "call" | "prompt") {
        return None;
    }
    let (name, arguments, offset) = word_then_rest(rest)?;
    Some((command, name, arguments, at + offset))
}

/// The first word of `text`, what follows the whitespace after it, and the byte
/// that follows starts at. `None` when there is no whitespace after the word,
/// which is to say the word itself is still being typed. Whitespace running to
/// the end of `text` gives an empty rest at the end of it: the word is finished
/// and what comes after it has not been started.
fn word_then_rest(text: &str) -> Option<(&str, &str, usize)> {
    let begins = text.find(|c: char| !c.is_whitespace())?;
    let word = &text[begins..];
    let ends = word.find(char::is_whitespace)?;
    let next = word[ends..]
        .find(|c: char| !c.is_whitespace())
        .map_or(word.len(), |at| at + ends);
    Some((&word[..ends], &word[next..], begins + next))
}

/// Keys for an object that has not been opened yet, each carrying the `{` that
/// opens it, so that one keystroke does both. Somewhere with no keys to offer is
/// offered nothing — a lone `{` is not worth a keystroke.
fn opens_with(keys: Vec<String>) -> Vec<String> {
    keys.into_iter().map(|key| format!("{{{key}")).collect()
}

/// Settled pairs as the object `context.arguments` wants.
fn object(settled: &[(String, String)]) -> Map<String, Value> {
    settled
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect()
}

/// One container the scan is inside.
enum Frame {
    Object {
        /// The key in the enclosing object whose value this object is, which is
        /// how the schema for it is found. `None` for the arguments object.
        of: Option<String>,
        /// The pairs this object already has, whole. The keys are not offered a
        /// second time, and the values are what a server is told has been
        /// settled when it is asked about another one.
        settled: Vec<(String, String)>,
        /// The key whose value is being written, once a `:` has been typed.
        key: Option<String>,
        after_colon: bool,
    },
    Array,
}

/// Where a half-typed arguments object leaves the cursor. The default is the
/// cursor before any of it exists: the arguments object itself, with nothing
/// written in it and neither a key nor a value under way.
#[derive(Default)]
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
    settled: Vec<(String, String)>,
}

impl Opening {
    /// A scan of unfinished JSON, which stops at anything it cannot read rather
    /// than guessing: an unbalanced `}`, an array, a nesting deeper than
    /// [`NESTING`], or a cursor sitting just past a value that is already whole.
    pub(crate) fn read(text: &str) -> Option<Self> {
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
                        settled: Vec::new(),
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
                        settled,
                        key,
                        after_colon,
                        ..
                    }) = stack.last_mut()
                    {
                        // Whatever token closed on the way to this comma is the
                        // value of the key before it; a nested object or an
                        // array closed no token, and settles as nothing.
                        if let Some(key) = key.take() {
                            settled.push((key, closed.clone().unwrap_or_default()));
                        }
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
            settled,
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
            settled: settled.clone(),
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
            let names: Vec<String> = properties
                .into_iter()
                .flat_map(|props| props.keys())
                .cloned()
                .collect();
            return self.unwritten(&names);
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

    /// The names this object has not been given yet, each with the `: ` that
    /// follows it.
    fn unwritten(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|name| !self.has(name) && self.matches(name))
            .map(|name| format!("{}: ", Value::String(name.clone())))
            .collect()
    }

    fn has(&self, key: &str) -> bool {
        self.settled.iter().any(|(written, _)| written == key)
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

/// One piece of a URI template, as far as completing a URI needs to read one:
/// literal text, or one variable to fill in.
enum Piece<'a> {
    Text(&'a str),
    Variable(&'a str),
}

/// A template split into its pieces. `None` for a brace that never closes, and
/// for the operators and modifiers RFC 6570 allows inside one (`{+path}`,
/// `{?a,b}`, `{x:3}`): a template mcpdial cannot expand exactly is a template it
/// offers nothing for.
fn pieces(template: &str) -> Option<Vec<Piece<'_>>> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(opens) = rest.find('{') {
        let (text, from_brace) = rest.split_at(opens);
        if !text.is_empty() {
            found.push(Piece::Text(text));
        }
        let closes = from_brace.find('}')?;
        let name = &from_brace[1..closes];
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        found.push(Piece::Variable(name));
        rest = &from_brace[closes + 1..];
    }
    if !rest.is_empty() {
        found.push(Piece::Text(rest));
    }
    Some(found)
}

/// What a template still has to offer where the typed text leaves off.
enum Ahead<'a> {
    /// The cursor is partway through a literal, which nobody has to be asked
    /// about: the whole URI up to the end of that literal is the one candidate,
    /// and Tab again reaches whatever follows it.
    Literal(String),
    /// The cursor is in a variable, which only the server can fill in.
    Variable(Filling<'a>),
}

/// The variable of a template the cursor is in, and what it takes to write an
/// answer for it back into the line.
struct Filling<'a> {
    variable: &'a str,
    /// What has been typed of that variable's value so far.
    typed: &'a str,
    /// The line up to where that value starts.
    before: &'a str,
    /// What the template says after the variable, its later variables left as
    /// they were written so that Tab comes back for them.
    rest: String,
    /// The variables the typed text has already settled, for the server to
    /// narrow this one by.
    settled: Vec<(String, String)>,
}

impl<'a> Ahead<'a> {
    /// Where `typed` leaves off in `template`. `None` when the text does not
    /// follow the template at all, and when it follows it to the end: a URI that
    /// is already whole has nothing left to complete.
    fn of(template: &'a str, typed: &'a str) -> Option<Self> {
        let pieces = pieces(template)?;
        let mut left = typed;
        let mut settled: Vec<(String, String)> = Vec::new();
        for (i, piece) in pieces.iter().enumerate() {
            let done = typed.len() - left.len();
            match piece {
                Piece::Text(text) => match left.strip_prefix(text) {
                    Some(rest) => left = rest,
                    None if text.starts_with(left) => {
                        return Some(Ahead::Literal(format!("{}{text}", &typed[..done])))
                    }
                    None => return None,
                },
                Piece::Variable(variable) => {
                    let ends_at = match pieces.get(i + 1) {
                        Some(Piece::Text(next)) => left.find(next),
                        // Two variables running have nothing between them to
                        // tell one's value from the other's.
                        Some(Piece::Variable(_)) => return None,
                        None => None,
                    };
                    let Some(ends_at) = ends_at else {
                        return Some(Ahead::Variable(Filling {
                            variable,
                            typed: left,
                            before: &typed[..done],
                            rest: written(&pieces[i + 1..]),
                            settled,
                        }));
                    };
                    settled.push((variable.to_string(), left[..ends_at].to_string()));
                    left = &left[ends_at..];
                }
            }
        }
        None
    }
}

/// Template pieces as they were written, braces and all.
fn written(pieces: &[Piece<'_>]) -> String {
    pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(text) => (*text).to_string(),
            Piece::Variable(name) => format!("{{{name}}}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::completion::Completer;
    use rustyline::history::DefaultHistory;
    use serde_json::json;
    use std::cell::RefCell;

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

    /// A [`Suggests`] that answers from a fixed list and keeps what it was
    /// asked, so a test reads the request off it as well as the answer.
    #[derive(Default)]
    struct Stub {
        values: Vec<String>,
        asked: RefCell<Vec<Value>>,
    }

    impl Stub {
        fn offering(values: &[&str]) -> Rc<Self> {
            Rc::new(Self {
                values: values.iter().map(|v| (*v).to_string()).collect(),
                asked: RefCell::default(),
            })
        }

        fn last(&self) -> Value {
            self.asked.borrow().last().cloned().unwrap_or(Value::Null)
        }
    }

    impl Suggests for Stub {
        fn values(&self, reference: Value, argument: Value, context: Option<Value>) -> Vec<String> {
            self.asked.borrow_mut().push(json!({
                "ref": reference, "argument": argument, "context": context,
            }));
            self.values.clone()
        }
    }

    /// A server with one prompt that takes two arguments and one template with
    /// one variable, both of which only it can suggest values for.
    fn documents(suggests: Option<Rc<dyn Suggests>>) -> ShellHelper {
        ShellHelper {
            prompts: vec![
                json!({"name": "summarize", "arguments": [
                    {"name": "document", "required": true},
                    {"name": "style"},
                ]}),
                json!({"name": "greet"}),
            ],
            resources: vec!["file:///readme.md".to_string()],
            templates: vec!["file:///notes/{name}.md".to_string()],
            suggests,
            ..ShellHelper::default()
        }
    }

    #[test]
    fn completes_commands_then_tool_and_prompt_names_and_resource_uris() {
        let helper = ShellHelper {
            tools: named(["list_pages", "list_console_messages", "new_page"]),
            resources: ["file:///a.md", "file:///b.png"].map(String::from).to_vec(),
            prompts: vec![json!({"name": "summarize"}), json!({"name": "translate"})],
            ..ShellHelper::default()
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
    fn opens_the_arguments_object_where_one_has_not_been_typed() {
        let helper = browser();
        // Tab after the tool name writes the brace along with the key, and
        // replaces nothing: the candidates start at the end of the line.
        let (at, found) = complete(&helper, "call new_page ");
        assert_eq!(at, 14);
        assert_eq!(
            found,
            [
                r#"{"retries": "#,
                r#"{"url": "#,
                r#"{"user": "#,
                r#"{"viewport": "#,
                r#"{"wait": "#
            ]
        );
        assert_eq!(complete(&helper, "call new_page   ").0, 16);
        // A tool that takes no arguments has no object worth opening.
        let bare = ShellHelper {
            tools: vec![json!({
                "name": "count",
                "inputSchema": {"type": "object", "properties": {}},
            })],
            ..ShellHelper::default()
        };
        assert!(complete(&bare, "call count ").1.is_empty());
        assert!(complete(&ShellHelper::default(), "call count ")
            .1
            .is_empty());
        // The other ways of writing arguments are left where they were.
        assert!(complete(&helper, "call new_page @args.json").1.is_empty());
        assert!(complete(&helper, "call new_page -").1.is_empty());
        // Only `call` and `prompt` take an arguments object, and only after the
        // name of what they run.
        assert!(complete(&helper, "prompt poster ").1.is_empty());
        assert!(complete(&helper, "schema new_page ").1.is_empty());
        assert!(complete(&helper, "raw tools/call ").1.is_empty());
        assert_eq!(complete(&helper, "call ").1, ["new_page"]);
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
        // Neither `schema` nor `raw` has an arguments object.
        assert!(complete(&helper, "raw tools/call {\"").1.is_empty());
        assert!(complete(&helper, "schema new_page {\"").1.is_empty());
    }

    #[test]
    fn completes_a_prompt_argument_name_from_what_the_prompt_declares() {
        let helper = documents(None);
        // The names are the prompt's own, and Tab writes the brace with them.
        let (at, found) = complete(&helper, "prompt summarize ");
        assert_eq!(at, 17);
        assert_eq!(found, [r#"{"document": "#, r#"{"style": "#]);
        assert_eq!(
            complete(&helper, "prompt summarize {\"s").1,
            [r#""style": "#]
        );
        // One already written is not offered again.
        assert_eq!(
            complete(&helper, "prompt summarize {\"document\": \"a.md\", \"").1,
            [r#""style": "#]
        );
        // A prompt that declares no arguments, and one this server never listed.
        assert!(complete(&helper, "prompt greet ").1.is_empty());
        assert!(complete(&helper, "prompt nope {\"").1.is_empty());
    }

    #[test]
    fn asks_the_server_for_a_prompt_argument_value_with_what_is_settled() {
        let stub = Stub::offering(&["thorough", "terse"]);
        let helper = documents(Some(stub.clone()));
        let (at, found) = complete(
            &helper,
            "prompt summarize {\"document\": \"a.md\", \"style\": \"t",
        );
        assert_eq!(at, 47);
        assert_eq!(found, [r#""thorough""#, r#""terse""#]);
        assert_eq!(
            stub.last(),
            json!({
                "ref": {"type": "ref/prompt", "name": "summarize"},
                "argument": {"name": "style", "value": "t"},
                "context": {"arguments": {"document": "a.md"}},
            })
        );
        // Without the opening quote typed, and with nothing settled before it.
        assert_eq!(
            complete(&helper, "prompt summarize {\"style\": ").1.len(),
            2
        );
        assert_eq!(
            stub.last()["argument"],
            json!({"name": "style", "value": ""})
        );
        assert_eq!(stub.last()["context"], json!({"arguments": {}}));
    }

    #[test]
    fn a_server_that_suggests_nothing_leaves_every_local_pool_as_it_was() {
        // No `completions` capability at all: the keys still complete, and the
        // values simply have nobody to ask.
        let mute = documents(None);
        assert_eq!(complete(&mute, "prompt summarize {\"st").1.len(), 1);
        assert!(complete(&mute, "prompt summarize {\"style\": \"")
            .1
            .is_empty());
        assert_eq!(complete(&mute, "read file:///r").1, ["file:///readme.md"]);
        assert!(complete(&mute, "read file:///notes/we").1.is_empty());
        // A server that answers with nothing - slow, wedged, or gone - reads
        // exactly the same way.
        let silent = documents(Some(Stub::offering(&[])));
        assert!(complete(&silent, "prompt summarize {\"style\": \"")
            .1
            .is_empty());
        assert_eq!(complete(&silent, "read file:///r").1, ["file:///readme.md"]);
    }

    #[test]
    fn completes_a_template_variable_into_the_whole_uri() {
        let stub = Stub::offering(&["weekly", "daily"]);
        let helper = documents(Some(stub.clone()));
        // The candidate is the URI the template makes, not the value alone, so
        // that what follows the variable comes with it.
        let (at, found) = complete(&helper, "read file:///notes/we");
        assert_eq!(at, 5);
        assert_eq!(found, ["file:///notes/weekly.md", "file:///notes/daily.md"]);
        assert_eq!(
            stub.last(),
            json!({
                "ref": {"type": "ref/resource", "uri": "file:///notes/{name}.md"},
                "argument": {"name": "name", "value": "we"},
                "context": {"arguments": {}},
            })
        );
        // Partway through the literal ahead of the variable there is nobody to
        // ask: the literal itself finishes the word, and Tab again reaches the
        // variable after it.
        assert_eq!(complete(&helper, "read file:///not").1, ["file:///notes/"]);
        // A URI that is already whole, and one that is not this template's.
        assert!(complete(&helper, "read file:///notes/weekly.md")
            .1
            .is_empty());
        assert!(complete(&helper, "read https://other/x").1.is_empty());
    }

    #[test]
    fn reads_a_template_only_as_far_as_it_can_expand_one_exactly() {
        let stub = Stub::offering(&["x"]);
        let ambiguous = ShellHelper {
            templates: [
                // An operator, a modifier, a brace that never closes, and two
                // variables with nothing between them: none of these expand.
                "file:///{+path}",
                "file:///{name:3}.md",
                "file:///{name.md",
                "file:///{a}{b}.md",
            ]
            .map(String::from)
            .to_vec(),
            suggests: Some(stub.clone()),
            ..ShellHelper::default()
        };
        assert!(complete(&ambiguous, "read file:///w").1.is_empty());
        assert_eq!(stub.last(), Value::Null, "no server was asked");
        // Two variables the typed text has already settled between them.
        let two = ShellHelper {
            templates: vec!["db://{table}/rows/{id}".to_string()],
            suggests: Some(stub.clone()),
            ..ShellHelper::default()
        };
        assert_eq!(
            complete(&two, "read db://users/rows/4").1,
            ["db://users/rows/x"]
        );
        assert_eq!(
            stub.last()["context"],
            json!({"arguments": {"table": "users"}})
        );
    }
}
