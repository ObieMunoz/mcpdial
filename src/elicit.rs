//! Answering `elicitation/create`: the one request a server makes mid-call that
//! wants a person, or something standing in for one.
//!
//! A server missing one fact - a confirmation, a region, a parameter nobody
//! passed - asks for it and blocks until the client replies. The reply is one of
//! three actions and two of them need no human at all: `decline` says the value
//! is not coming, `cancel` says whoever was asked walked away. That is why an
//! unattended run never stalls here: with no terminal and no `--elicit`, the
//! answer is a decline, sent at once, and the server degrades however it likes.

use crate::protocol::Responder;
use crate::schema::plain;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::rc::Rc;

/// The request this module exists to answer.
pub const METHOD: &str = "elicitation/create";

/// What one invocation can do when a server elicits.
#[derive(Debug, Clone, Default)]
pub struct Elicit {
    /// Values `--elicit` supplied. A form whose required properties they all
    /// cover is accepted with them; anything else is declined.
    pub answers: Option<Map<String, Value>>,
    /// A person is there to be asked: stdin and stderr are both terminals and
    /// the output is not being parsed by a program.
    pub ask: bool,
    /// Answers may still arrive after `initialize`, as `shell`'s `elicit`
    /// command supplies them. The form capability is declared with none in hand
    /// because a shell session can always be handed some later.
    pub later: bool,
    /// Open a url-mode elicitation in a browser, the way `login` does.
    pub browser: bool,
    /// Report each elicitation as one JSON object on stderr rather than a
    /// sentence, so `--json` keeps one object per line on either stream.
    pub json: bool,
}

impl Elicit {
    /// Whether a form elicitation stands any chance of being answered.
    fn fills_forms(&self) -> bool {
        self.ask || self.answers.is_some() || self.later
    }

    /// The `capabilities` object `initialize` declares.
    ///
    /// The spec reads a bare `elicitation` object as form-only, so url mode is
    /// named explicitly: printing a URL and letting the interaction happen out
    /// of band needs nobody at a terminal and is always on offer.
    pub fn capabilities(&self) -> Value {
        let mut modes = json!({ "url": {} });
        if self.fills_forms() {
            modes["form"] = json!({});
        }
        json!({ "elicitation": modes })
    }

    /// What a daemon declares on behalf of whichever caller is attached.
    ///
    /// The daemon holds one handshake for many callers, and relays what the
    /// server asks to the caller of the moment. A caller that cannot fill in a
    /// form declines, which is an answer, so the capability is honest.
    pub fn relayed() -> Value {
        Elicit {
            later: true,
            ..Elicit::default()
        }
        .capabilities()
    }
}

/// The answers a form elicitation is filled in with, shared with the live
/// session so `shell`'s `elicit` command can replace them mid-flight.
#[derive(Clone, Default)]
pub struct Answers(Rc<RefCell<Option<Map<String, Value>>>>);

impl Answers {
    pub fn new(values: Option<Map<String, Value>>) -> Self {
        Answers(Rc::new(RefCell::new(values)))
    }

    /// Replace what later elicitations are answered with.
    pub fn set(&self, values: Map<String, Value>) {
        *self.0.borrow_mut() = Some(values);
    }

    fn get(&self) -> Option<Map<String, Value>> {
        self.0.borrow().clone()
    }
}

/// The handler [`crate::Transport::answer_requests`] installs on a transport.
pub fn responder(elicit: &Elicit, answers: Answers) -> Responder {
    let mut state = Elicitor {
        elicit: elicit.clone(),
        answers,
        // Whether there is anybody at the terminal is `Elicit::ask`, settled
        // from the presenter before the transport was ever dialed; nothing
        // down here gets a second opinion about it.
        reader: match elicit.ask {
            true => Reader::Unopened,
            false => Reader::Plain,
        },
    };
    Box::new(move |method, params| (method == METHOD).then(|| state.respond(params)))
}

struct Elicitor {
    elicit: Elicit,
    answers: Answers,
    reader: Reader,
}

/// The reply to one elicitation, and what the line on stderr says about it.
struct Reply {
    action: &'static str,
    content: Option<Map<String, Value>>,
    detail: String,
    /// The address a url-mode request named, reported under `--json`.
    url: Option<String>,
    /// Set when the question is already on stderr above the prompts, so the
    /// closing line does not repeat it.
    announced: bool,
}

impl Reply {
    fn declined(reason: impl std::fmt::Display) -> Reply {
        Reply {
            action: "decline",
            content: None,
            detail: format!("declined ({reason})"),
            url: None,
            announced: false,
        }
    }

    fn accepted(content: Map<String, Value>, detail: impl Into<String>) -> Reply {
        Reply {
            action: "accept",
            content: Some(content),
            detail: detail.into(),
            url: None,
            announced: false,
        }
    }
}

impl Elicitor {
    fn respond(&mut self, params: &Value) -> Value {
        let message = params["message"].as_str().unwrap_or("").to_string();
        let reply = match params["mode"].as_str().unwrap_or("form") {
            "form" => self.form(&message, params),
            "url" => self.url(params),
            other => Reply::declined(format!("mcpdial does not answer {other:?} elicitations")),
        };
        self.report(&message, &reply);

        // `content` belongs to an accepted form and nothing else: the spec has a
        // url-mode accept carry none, and a decline or a cancel say by
        // definition that there is nothing to carry.
        let mut result = json!({ "action": reply.action });
        if let Some(content) = reply.content {
            result["content"] = Value::Object(content);
        }
        result
    }

    fn report(&self, message: &str, reply: &Reply) {
        if self.elicit.json {
            let mut note = json!({ "action": reply.action, "message": message });
            note["detail"] = json!(reply.detail);
            if let Some(url) = &reply.url {
                note["url"] = json!(url);
            }
            eprintln!("{}", json!({ "elicitation": note }));
        } else if reply.announced {
            eprintln!("{}", reply.detail);
        } else {
            eprintln!("server asked: {message}; {}", reply.detail);
        }
    }

    fn form(&mut self, message: &str, params: &Value) -> Reply {
        let fields = match Field::list(&params["requestedSchema"]) {
            Ok(fields) => fields,
            Err(why) => return Reply::declined(why),
        };
        if let Some(values) = self.answers.get() {
            return fill(&fields, &values);
        }
        if !self.elicit.ask {
            return Reply::declined("no terminal; use --elicit");
        }
        eprintln!("server asked: {message}");
        self.ask(&fields)
    }

    /// Put every property to whoever is at the terminal, one at a time.
    fn ask(&mut self, fields: &[Field]) -> Reply {
        let mut content = Map::new();
        for field in fields {
            if let Some(about) = &field.description {
                eprintln!("  {about}");
            }
            loop {
                let Some(line) = self.reader.line(&field.prompt()) else {
                    return Reply {
                        action: "cancel",
                        content: None,
                        detail: "cancelled".into(),
                        url: None,
                        announced: true,
                    };
                };
                let line = line.trim();
                if line.is_empty() {
                    match &field.default {
                        Some(d) => {
                            content.insert(field.name.clone(), d.clone());
                            break;
                        }
                        None if !field.required => break,
                        None => {
                            eprintln!("  {} is required", field.name);
                            continue;
                        }
                    }
                }
                match field.read(line) {
                    Ok(value) => {
                        content.insert(field.name.clone(), value);
                        break;
                    }
                    Err(why) => eprintln!("  {why}"),
                }
            }
        }
        Reply {
            announced: true,
            ..Reply::accepted(content, "elicitation answered")
        }
    }

    /// url mode: the interaction happens out of band, so the answer is an
    /// immediate accept with no content and the address goes on stderr for
    /// whoever is watching.
    fn url(&self, params: &Value) -> Reply {
        let Some(url) = params["url"].as_str() else {
            return Reply::declined("the request names no url");
        };
        let opened = self.elicit.browser && open_browser(url);
        Reply {
            action: "accept",
            content: None,
            detail: match opened {
                true => format!("accepted; opened {url}"),
                false => format!("accepted; complete it at {url}"),
            },
            url: Some(url.to_string()),
            announced: false,
        }
    }
}

/// A URL the server chose is handed to the desktop's opener only when it is a
/// web address: any other scheme is a local handler, and some of those take
/// arguments.
fn open_browser(url: &str) -> bool {
    let is_web = url.starts_with("https://") || url.starts_with("http://");
    is_web && crate::oauth::open_browser(url)
}

/// Fill the form from values given ahead of time. Every required property has to
/// be covered, and every value has to pass the checks a typed one would.
fn fill(fields: &[Field], values: &Map<String, Value>) -> Reply {
    let mut content = Map::new();
    for field in fields {
        let Some(given) = values.get(&field.name) else {
            if field.required {
                return Reply::declined(format!(
                    "--elicit has no value for {:?}",
                    field.name.as_str()
                ));
            }
            continue;
        };
        match field.check(given) {
            Ok(value) => {
                content.insert(field.name.clone(), value);
            }
            Err(why) => return Reply::declined(format!("--elicit {}: {why}", field.name)),
        }
    }
    Reply::accepted(content, "answered from --elicit")
}

/// Where a typed answer comes from.
///
/// A terminal on all three streams gets the line editor, so that `^C` answers
/// `cancel` instead of killing the process mid-call; anything else reads stdin a
/// line at a time, where the end of input is the only way out. Which of the two
/// this is was decided by [`Elicit::ask`], not here.
enum Reader {
    /// The line editor a person gets, not built yet: the first question is
    /// what opens it, and a run that never asks one never pays for it.
    Unopened,
    Editing(Box<rustyline::DefaultEditor>),
    Plain,
}

impl Reader {
    /// The next answer, or `None` for `^C`, `^D` or a closed stdin.
    fn line(&mut self, prompt: &str) -> Option<String> {
        if let Reader::Unopened = self {
            *self = match rustyline::DefaultEditor::new() {
                Ok(editor) => Reader::Editing(Box::new(editor)),
                Err(_) => Reader::Plain,
            };
        }
        match self {
            Reader::Editing(editor) => editor.readline(prompt).ok(),
            _ => {
                eprint!("{prompt}");
                std::io::stderr().flush().ok();
                let mut line = String::new();
                match std::io::stdin().lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                }
            }
        }
    }
}

/// One property of the flat schema a form elicitation carries.
struct Field {
    name: String,
    title: String,
    description: Option<String>,
    kind: Kind,
    required: bool,
    default: Option<Value>,
}

/// The property types the spec's restricted schema allows.
enum Kind {
    Text {
        min: Option<u64>,
        max: Option<u64>,
        pattern: Option<String>,
        format: Option<String>,
    },
    Number {
        whole: bool,
        min: Option<f64>,
        max: Option<f64>,
    },
    Boolean,
    /// One of these values: `enum`, or `oneOf` of `const` and `title`.
    Choice(Vec<Choice>),
    /// Any number of them, between `min` and `max`.
    Multi {
        of: Vec<Choice>,
        min: Option<u64>,
        max: Option<u64>,
    },
}

struct Choice {
    value: Value,
    title: String,
}

impl Field {
    fn list(schema: &Value) -> Result<Vec<Field>, String> {
        let properties = schema["properties"]
            .as_object()
            .ok_or("the requested schema has no properties")?;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut fields = Vec::new();
        for (name, spec) in properties {
            let required = required.contains(&name.as_str());
            match Kind::of(spec) {
                Some(kind) => fields.push(Field {
                    name: name.clone(),
                    title: spec["title"].as_str().unwrap_or(name).to_string(),
                    description: spec["description"].as_str().map(str::to_string),
                    kind,
                    required,
                    default: spec.get("default").cloned(),
                }),
                None if required => {
                    return Err(format!("{name} is of a type mcpdial cannot fill in"))
                }
                None => {}
            }
        }
        Ok(fields)
    }

    /// Validate one value against this property's constraints, and hand back
    /// what should go in `content`.
    fn check(&self, v: &Value) -> Result<Value, String> {
        match &self.kind {
            Kind::Text {
                min, max, format, ..
            } => {
                let s = v.as_str().ok_or_else(|| format!("{v} is not a string"))?;
                let length = s.chars().count() as u64;
                if min.is_some_and(|m| length < m) {
                    return Err(format!("{v} is shorter than the minimum {}", min.unwrap()));
                }
                if max.is_some_and(|m| length > m) {
                    return Err(format!("{v} is longer than the maximum {}", max.unwrap()));
                }
                if let Some(format) = format {
                    check_format(format, s)?;
                }
                Ok(v.clone())
            }
            Kind::Number { whole, min, max } => {
                let n = v.as_f64().ok_or_else(|| format!("{v} is not a number"))?;
                if *whole && n.fract() != 0.0 {
                    return Err(format!("{v} is not a whole number"));
                }
                if min.is_some_and(|m| n < m) {
                    return Err(format!("{v} is less than the minimum {}", min.unwrap()));
                }
                if max.is_some_and(|m| n > m) {
                    return Err(format!("{v} is more than the maximum {}", max.unwrap()));
                }
                Ok(if *whole { json!(n as i64) } else { v.clone() })
            }
            Kind::Boolean => v
                .as_bool()
                .map(|b| json!(b))
                .ok_or_else(|| format!("{v} is not true or false")),
            Kind::Choice(of) => of
                .iter()
                .find(|c| c.value == *v)
                .map(|c| c.value.clone())
                .ok_or_else(|| format!("{v} is not one of {}", listed(of))),
            Kind::Multi { of, min, max } => {
                let items = v.as_array().ok_or_else(|| format!("{v} is not a list"))?;
                if min.is_some_and(|m| (items.len() as u64) < m) {
                    return Err(format!("{v} has fewer than the minimum {}", min.unwrap()));
                }
                if max.is_some_and(|m| (items.len() as u64) > m) {
                    return Err(format!("{v} has more than the maximum {}", max.unwrap()));
                }
                let chosen = items
                    .iter()
                    .map(|item| {
                        of.iter()
                            .find(|c| c.value == *item)
                            .map(|c| c.value.clone())
                            .ok_or_else(|| format!("{item} is not one of {}", listed(of)))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Array(chosen))
            }
        }
    }

    /// One line of typing turned into a value, then checked as any other is.
    fn read(&self, line: &str) -> Result<Value, String> {
        let value = match &self.kind {
            Kind::Text { .. } => json!(line),
            Kind::Number { .. } => line
                .parse::<f64>()
                .map(|n| json!(n))
                .map_err(|_| format!("{line:?} is not a number"))?,
            Kind::Boolean => match line.to_ascii_lowercase().as_str() {
                "y" | "yes" | "true" | "t" | "1" => json!(true),
                "n" | "no" | "false" | "f" | "0" => json!(false),
                _ => return Err("answer yes or no".to_string()),
            },
            Kind::Choice(of) => pick(of, line)?,
            Kind::Multi { of, .. } => Value::Array(
                line.split(',')
                    .map(str::trim)
                    .filter(|word| !word.is_empty())
                    .map(|word| pick(of, word))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        self.check(&value)
    }

    /// What is printed before the cursor: the property, what it takes, and what
    /// pressing Enter alone would answer.
    fn prompt(&self) -> String {
        let shape = match &self.kind {
            Kind::Text {
                min,
                max,
                pattern,
                format,
            } => {
                let mut parts = vec!["text".to_string()];
                if let Some(f) = format {
                    parts.push(f.clone());
                }
                match (min, max) {
                    (Some(a), Some(b)) => parts.push(format!("{a}-{b} characters")),
                    (Some(a), None) => parts.push(format!("at least {a} characters")),
                    (None, Some(b)) => parts.push(format!("at most {b} characters")),
                    (None, None) => {}
                }
                if let Some(p) = pattern {
                    parts.push(format!("matching {p}"));
                }
                parts.join(", ")
            }
            Kind::Number { whole, min, max } => {
                let kind = if *whole { "integer" } else { "number" };
                match (min, max) {
                    (Some(a), Some(b)) => format!("{kind} {a} to {b}"),
                    (Some(a), None) => format!("{kind} from {a}"),
                    (None, Some(b)) => format!("{kind} up to {b}"),
                    (None, None) => kind.to_string(),
                }
            }
            Kind::Boolean => "yes/no".to_string(),
            Kind::Choice(of) => listed(of),
            Kind::Multi { of, .. } => format!("any of {}, comma separated", listed(of)),
        };
        let optional = if self.required { "" } else { ", optional" };
        let default = match &self.default {
            Some(d) => format!(" [{d}]"),
            None => String::new(),
        };
        format!("{} ({shape}{optional}){default}: ", self.title)
    }
}

impl Kind {
    fn of(spec: &Value) -> Option<Kind> {
        if let Some(of) = choices(spec) {
            return Some(Kind::Choice(of));
        }
        match spec["type"].as_str()? {
            "string" => Some(Kind::Text {
                min: count(spec, "minLength"),
                max: count(spec, "maxLength"),
                pattern: spec["pattern"].as_str().map(str::to_string),
                format: spec["format"].as_str().map(str::to_string),
            }),
            kind @ ("number" | "integer") => Some(Kind::Number {
                whole: kind == "integer",
                min: spec["minimum"].as_f64(),
                max: spec["maximum"].as_f64(),
            }),
            "boolean" => Some(Kind::Boolean),
            "array" => Some(Kind::Multi {
                of: choices(&spec["items"])?,
                min: count(spec, "minItems"),
                max: count(spec, "maxItems"),
            }),
            _ => None,
        }
    }
}

/// The options behind a single-select property, written either way the spec
/// allows: a bare `enum`, or a `oneOf` of `const` values with titles.
fn choices(spec: &Value) -> Option<Vec<Choice>> {
    if let Some(values) = spec["enum"].as_array() {
        let names = spec["enumNames"].as_array();
        return Some(
            values
                .iter()
                .enumerate()
                .map(|(i, value)| Choice {
                    title: names
                        .and_then(|n| n.get(i))
                        .and_then(Value::as_str)
                        .map_or_else(|| plain(value), str::to_string),
                    value: value.clone(),
                })
                .collect(),
        );
    }
    let variants = spec["oneOf"].as_array()?;
    variants
        .iter()
        .map(|variant| {
            let value = variant.get("const")?.clone();
            Some(Choice {
                title: variant["title"]
                    .as_str()
                    .map_or_else(|| plain(&value), str::to_string),
                value,
            })
        })
        .collect()
}

/// A word typed at a prompt matched against the options: its number in the list,
/// its value, or its title.
fn pick(of: &[Choice], word: &str) -> Result<Value, String> {
    if let Ok(n) = word.parse::<usize>() {
        if let Some(c) = n.checked_sub(1).and_then(|i| of.get(i)) {
            return Ok(c.value.clone());
        }
    }
    of.iter()
        .find(|c| plain(&c.value) == word || c.title.eq_ignore_ascii_case(word))
        .map(|c| c.value.clone())
        .ok_or_else(|| format!("{word:?} is not one of {}", listed(of)))
}

fn listed(of: &[Choice]) -> String {
    of.iter()
        .enumerate()
        .map(|(i, c)| format!("{}) {}", i + 1, c.title))
        .collect::<Vec<_>>()
        .join(" ")
}

fn count(spec: &Value, key: &str) -> Option<u64> {
    spec[key].as_u64()
}

/// The four formats the spec allows on a string, checked as far as they can be
/// without a parser. The server validates them again, and a false rejection here
/// would keep a valid answer from ever reaching it, so each test is the loosest
/// one that still catches a typo.
fn check_format(format: &str, s: &str) -> Result<(), String> {
    let well_formed = match format {
        "email" => s
            .split_once('@')
            .is_some_and(|(user, host)| !user.is_empty() && host.contains('.')),
        "uri" => s
            .split_once(':')
            .is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty()),
        "date" => is_date(s),
        "date-time" => s
            .split_once('T')
            .is_some_and(|(date, time)| is_date(date) && !time.is_empty()),
        _ => true,
    };
    match well_formed {
        true => Ok(()),
        false => Err(format!("{s:?} is not a valid {format}")),
    }
}

fn is_date(s: &str) -> bool {
    let digits_and_dashes = |(i, c): (usize, char)| match i {
        4 | 7 => c == '-',
        _ => c.is_ascii_digit(),
    };
    s.len() == 10 && s.char_indices().all(digits_and_dashes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "confirm": {"type": "boolean", "title": "Go ahead?"},
                "count": {"type": "integer", "minimum": 1, "maximum": 3},
                "region": {"type": "string", "enum": ["us", "eu"]},
            },
            "required": ["confirm", "region"],
        })
    }

    fn field(name: &str) -> Field {
        Field::list(&schema())
            .unwrap()
            .into_iter()
            .find(|f| f.name == name)
            .unwrap()
    }

    #[test]
    fn a_form_covered_by_the_answers_is_accepted() {
        let fields = Field::list(&schema()).unwrap();
        let given = json!({"confirm": true, "region": "eu", "spare": "ignored"});
        let reply = fill(&fields, given.as_object().unwrap());
        assert_eq!(reply.action, "accept");
        assert_eq!(
            Value::Object(reply.content.unwrap()),
            json!({"confirm": true, "region": "eu"})
        );
    }

    #[test]
    fn a_missing_required_property_is_declined_by_name() {
        let fields = Field::list(&schema()).unwrap();
        let reply = fill(&fields, json!({"confirm": true}).as_object().unwrap());
        assert_eq!(reply.action, "decline");
        assert!(reply.detail.contains("region"), "{}", reply.detail);
        assert!(reply.content.is_none());
    }

    #[test]
    fn a_value_outside_its_bounds_is_declined_before_it_is_sent() {
        let fields = Field::list(&schema()).unwrap();
        let given = json!({"confirm": true, "region": "eu", "count": 9});
        let reply = fill(&fields, given.as_object().unwrap());
        assert_eq!(reply.action, "decline");
        assert!(
            reply.detail.contains("count") && reply.detail.contains("maximum 3"),
            "{}",
            reply.detail
        );
    }

    #[test]
    fn an_optional_property_left_out_is_simply_absent() {
        let fields = Field::list(&schema()).unwrap();
        let given = json!({"confirm": false, "region": "us"});
        let reply = fill(&fields, given.as_object().unwrap());
        assert_eq!(reply.action, "accept");
        assert!(!reply.content.unwrap().contains_key("count"));
    }

    #[test]
    fn a_value_of_the_wrong_type_is_declined() {
        let fields = Field::list(&schema()).unwrap();
        let given = json!({"confirm": "yes", "region": "eu"});
        assert_eq!(fill(&fields, given.as_object().unwrap()).action, "decline");
        let given = json!({"confirm": true, "region": "moon"});
        let reply = fill(&fields, given.as_object().unwrap());
        assert_eq!(reply.action, "decline");
        assert!(reply.detail.contains("us"), "{}", reply.detail);
    }

    #[test]
    fn typed_answers_are_read_by_word_or_by_number() {
        assert_eq!(field("confirm").read("yes").unwrap(), json!(true));
        assert_eq!(field("confirm").read("N").unwrap(), json!(false));
        assert!(field("confirm").read("maybe").is_err());
        assert_eq!(field("region").read("2").unwrap(), json!("eu"));
        assert_eq!(field("region").read("us").unwrap(), json!("us"));
        assert!(field("region").read("4").is_err());
        assert_eq!(field("count").read("3").unwrap(), json!(3));
        assert!(field("count").read("4").is_err());
        assert!(field("count").read("two").is_err());
    }

    #[test]
    fn a_prompt_names_the_type_the_options_and_the_default() {
        assert!(field("confirm").prompt().starts_with("Go ahead? (yes/no)"));
        assert!(field("region").prompt().contains("1) us 2) eu"));
        let count = field("count").prompt();
        assert!(
            count.contains("integer 1 to 3") && count.contains("optional"),
            "{count}"
        );
    }

    #[test]
    fn a_multi_select_takes_a_comma_separated_list() {
        let spec = json!({
            "type": "object",
            "properties": {"tags": {"type": "array", "items": {"enum": ["a", "b", "c"]},
                                    "minItems": 1, "maxItems": 2}},
        });
        let fields = Field::list(&spec).unwrap();
        assert_eq!(fields[0].read("a, 3").unwrap(), json!(["a", "c"]));
        assert!(fields[0].read("a, b, c").is_err());
        assert!(fields[0].read("").is_err());
    }

    #[test]
    fn one_of_with_titles_is_the_other_way_to_write_a_choice() {
        let spec = json!({
            "type": "object",
            "properties": {"plan": {"oneOf": [
                {"const": "free", "title": "Free"},
                {"const": "pro", "title": "Pro"},
            ]}},
        });
        let fields = Field::list(&spec).unwrap();
        assert!(fields[0].prompt().contains("1) Free 2) Pro"));
        assert_eq!(fields[0].read("pro").unwrap(), json!("pro"));
        assert_eq!(fields[0].read("free").unwrap(), json!("free"));
    }

    #[test]
    fn a_required_property_of_an_unsupported_type_declines_the_whole_form() {
        let spec = json!({
            "type": "object",
            "properties": {"blob": {"type": "object"}},
            "required": ["blob"],
        });
        assert!(Field::list(&spec).is_err());
        // Optional, the same property is simply skipped.
        let spec = json!({"type": "object", "properties": {"blob": {"type": "object"}}});
        assert!(Field::list(&spec).unwrap().is_empty());
        assert!(Field::list(&json!({"type": "object"})).is_err());
    }

    #[test]
    fn string_constraints_are_checked_and_formats_loosely() {
        let spec = json!({"type": "object", "properties": {
            "name": {"type": "string", "minLength": 2, "maxLength": 4},
            "mail": {"type": "string", "format": "email"},
            "when": {"type": "string", "format": "date"},
        }});
        let fields = Field::list(&spec).unwrap();
        let of = |name: &str| fields.iter().find(|f| f.name == name).unwrap();
        assert!(of("name").read("a").is_err());
        assert!(of("name").read("abcde").is_err());
        assert_eq!(of("name").read("abc").unwrap(), json!("abc"));
        assert!(of("mail").read("nobody").is_err());
        assert_eq!(of("mail").read("a@b.co").unwrap(), json!("a@b.co"));
        assert!(of("when").read("2026-1-1").is_err());
        assert_eq!(of("when").read("2026-01-01").unwrap(), json!("2026-01-01"));
    }

    #[test]
    fn the_declared_capability_says_what_can_actually_answer() {
        let nobody = Elicit::default();
        assert_eq!(nobody.capabilities(), json!({"elicitation": {"url": {}}}));
        let scripted = Elicit {
            answers: Some(Map::new()),
            ..Elicit::default()
        };
        assert_eq!(
            scripted.capabilities(),
            json!({"elicitation": {"form": {}, "url": {}}})
        );
        let at_a_terminal = Elicit {
            ask: true,
            ..Elicit::default()
        };
        assert_eq!(at_a_terminal.capabilities(), scripted.capabilities());
        assert_eq!(Elicit::relayed(), scripted.capabilities());
    }

    #[test]
    fn a_url_request_is_accepted_with_no_content_and_the_address_reported() {
        let mut e = Elicitor {
            elicit: Elicit::default(),
            answers: Answers::default(),
            reader: Reader::Unopened,
        };
        let asked = json!({"mode": "url", "message": "sign in",
                           "url": "https://example.test/consent", "elicitationId": "e1"});
        let reply = e.respond(&asked);
        assert_eq!(reply, json!({"action": "accept"}));
        let missing = json!({"mode": "url", "message": "sign in"});
        assert_eq!(e.respond(&missing)["action"], "decline");
    }

    #[test]
    fn nothing_to_answer_with_declines_rather_than_waiting() {
        let mut e = Elicitor {
            elicit: Elicit::default(),
            answers: Answers::default(),
            reader: Reader::Unopened,
        };
        let asked = json!({"message": "which region?", "requestedSchema": schema()});
        assert_eq!(e.respond(&asked), json!({"action": "decline"}));
        assert_eq!(e.respond(&json!({"mode": "sideband"}))["action"], "decline");
    }

    #[test]
    fn answers_set_later_are_the_ones_used() {
        let answers = Answers::default();
        let mut e = Elicitor {
            elicit: Elicit {
                later: true,
                ..Elicit::default()
            },
            answers: answers.clone(),
            reader: Reader::Unopened,
        };
        let asked = json!({"message": "which region?", "requestedSchema": schema()});
        assert_eq!(e.respond(&asked)["action"], "decline");
        answers.set(
            json!({"confirm": true, "region": "us"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            e.respond(&asked),
            json!({"action": "accept", "content": {"confirm": true, "region": "us"}})
        );
    }

    #[test]
    fn only_elicitation_reaches_the_responder() {
        let mut r = responder(&Elicit::default(), Answers::default());
        assert!(r("ping", &json!({})).is_none());
        assert!(r("sampling/createMessage", &json!({})).is_none());
        assert!(r(METHOD, &json!({"mode": "url", "url": "https://x.test/"})).is_some());
    }
}
