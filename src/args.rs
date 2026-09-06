//! `key=value` arguments for `call` and `prompt`.
//!
//! A JSON object is exact but awkward to type at a shell, which fights over every
//! quote. A list of pairs is not, and the tool's own `inputSchema` says what each
//! value should have been: `count=10` is a number where the schema says `integer`
//! and stays text where it says `string`. `key:=json` skips the schema and takes
//! the value as written, which is how arrays, objects and a forced string arrive.

use crate::{closest, parse_object};
use mcpdial::transport::stdio::split_command;
use mcpdial::{Error, Result};
use serde_json::{Map, Number, Value};

/// The arguments of a `call` or `prompt` with nothing after the tool name.
const NO_ARGUMENTS: &str = "{}";

const BOTH_FORMS: &str =
    "arguments are one JSON object or a list of key=value pairs, not both at once";

/// Which of the two argument forms a command line used.
#[derive(Debug)]
pub enum Form<'a> {
    /// One JSON object: written inline, `@path` to read a file, or `-` for stdin.
    Json(&'a str),
    /// `key=value` and `key:=json` words, still to be read against a schema.
    Pairs(&'a [String]),
}

impl<'a> Form<'a> {
    /// The text of the JSON object, when that is the form used. It is settled
    /// before anything is dialed, as it always was.
    pub fn json(&self) -> Option<&'a str> {
        match self {
            Form::Json(text) => Some(text),
            Form::Pairs(_) => None,
        }
    }

    /// The pairs, when that is the form used.
    pub fn pairs(&self) -> &'a [String] {
        match self {
            Form::Json(_) => &[],
            Form::Pairs(pairs) => pairs,
        }
    }
}

/// Which form the words after a tool name took, with every pair checked for shape
/// so that a malformed one costs no connection.
pub fn form(arguments: &[String]) -> Result<Form<'_>> {
    let Some(first) = arguments.first() else {
        return Ok(Form::Json(NO_ARGUMENTS));
    };
    if is_json_form(first) {
        return match arguments.len() {
            1 => Ok(Form::Json(first)),
            _ => Err(Error::usage(BOTH_FORMS)),
        };
    }
    // One word on its own that is no pair was meant as the object, and gets the
    // answer an unparseable object has always got.
    if arguments.len() == 1 && !looks_like_pair(first) {
        return Ok(Form::Json(first));
    }
    for word in arguments {
        if is_json_form(word) {
            return Err(Error::usage(BOTH_FORMS));
        }
        split(word)?;
    }
    Ok(Form::Pairs(arguments))
}

/// `key=value` words as the arguments object a tool asked for. `schema` is that
/// tool's `inputSchema`; `Value::Null` stands for one that was never fetched and
/// leaves every `=` value a string.
pub fn parse_pairs(pairs: &[String], schema: &Value) -> Result<Value> {
    let mut out = Map::new();
    for word in pairs {
        let Pair { key, value } = split(word)?;
        let spec = schema["properties"].get(key);
        if spec.is_none() {
            reject_unknown_key(schema, key)?;
        }
        let value = match value {
            Raw::Json(v) => v,
            Raw::Text(text) => coerce(key, text, spec.unwrap_or(&Value::Null))?,
        };
        out.insert(key.to_string(), value);
    }
    Ok(Value::Object(out))
}

/// Whether the tool's schema has to be fetched before these pairs mean anything.
/// A `:=` value is JSON already, and no schema can say otherwise.
pub fn needs_schema(pairs: &[String]) -> bool {
    !pairs.iter().all(|word| {
        matches!(
            split(word),
            Ok(Pair {
                value: Raw::Json(_),
                ..
            })
        )
    })
}

/// One `call` or `prompt` line of the shell, where there is no shell in front to
/// take the quotes off and neither `@path` nor `-` is a form. The rest of the line
/// arrives whole, so anything that does not open with a pair is read as the object
/// it was before pairs existed. `schema` is called only when a pair needs one,
/// since the tool list costs a request.
pub fn shell_arguments(rest: &str, schema: impl FnOnce() -> Value) -> Result<Value> {
    let text = rest.trim();
    if !looks_like_pair(first_word(text)) {
        return parse_object(rest, "arguments");
    }
    let pairs = split_line(text)?;
    let schema = if needs_schema(&pairs) {
        schema()
    } else {
        Value::Null
    };
    parse_pairs(&pairs, &schema)
}

/// The required arguments of a tool written as pairs: the other half of the usage
/// line, so a call that failed in one form can be retried in the other. Empty when
/// the tool requires nothing, which the JSON form already says as `{}`.
pub fn example_pairs(tool: &Value) -> String {
    let schema = &tool["inputSchema"];
    let props = schema["properties"].as_object();
    schema["required"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(|name| {
            let spec = props.and_then(|p| p.get(name)).unwrap_or(&Value::Null);
            match spec["type"].as_str() {
                Some("array") => format!("{name}:=[...]"),
                Some("object") => format!("{name}:={{...}}"),
                _ => format!("{name}={}", pair_placeholder(spec)),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One word split into its key and what follows the `=`, with a `key:=json` value
/// parsed here so a value that is not JSON is caught before anything is dialed.
struct Pair<'a> {
    key: &'a str,
    value: Raw<'a>,
}

enum Raw<'a> {
    /// `key=value`: text, to be read as whatever type the schema declares.
    Text(&'a str),
    /// `key:=json`: a JSON value already, whatever the schema declares.
    Json(Value),
}

fn split(word: &str) -> Result<Pair<'_>> {
    let Some((left, value)) = word.split_once('=') else {
        return Err(not_a_pair(word));
    };
    let (key, as_json) = match left.strip_suffix(':') {
        Some(key) => (key, true),
        None => (left, false),
    };
    if key.is_empty() {
        return Err(Error::usage(format!(
            "a key=value pair needs a name before the =; {word:?} has none"
        )));
    }
    if !as_json {
        return Ok(Pair {
            key,
            value: Raw::Text(value),
        });
    }
    let parsed = serde_json::from_str(value).map_err(|e| {
        Error::usage(format!(
            "{key}:= takes a JSON value; {value:?} is not JSON ({e})"
        ))
    })?;
    Ok(Pair {
        key,
        value: Raw::Json(parsed),
    })
}

/// The JSON object form announces itself in its first character, so that a value
/// holding an `=` is never mistaken for a pair.
fn is_json_form(word: &str) -> bool {
    word.starts_with('{') || word.starts_with('@') || word == "-"
}

fn looks_like_pair(word: &str) -> bool {
    !is_json_form(word)
        && word
            .split_once('=')
            .is_some_and(|(left, _)| !left.strip_suffix(':').unwrap_or(left).is_empty())
}

fn first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or(text)
}

/// A word that is neither form. One holding a `:` is most likely a single field of
/// an object a shell split at its commas, which is worth saying outright.
fn not_a_pair(word: &str) -> Error {
    let mut message = format!(
        "arguments must be a JSON object like {{\"key\": \"value\"}} or key=value pairs; \
         {word:?} is neither"
    );
    if word.contains(':') {
        message.push_str(
            "; the shell split a JSON object at its commas, and single quotes keep it \
             whole: '{\"key\": \"value\", ...}'",
        );
    }
    Error::usage(message)
}

/// A schema that closes itself to extra properties has already said this call
/// would fail, so say so here rather than spend a round trip finding out.
fn reject_unknown_key(schema: &Value, key: &str) -> Result<()> {
    if schema["additionalProperties"] != Value::Bool(false) {
        return Ok(());
    }
    let names: Vec<&str> = schema["properties"]
        .as_object()
        .map(|props| props.keys().map(String::as_str).collect())
        .unwrap_or_default();
    Err(Error::usage(match closest(key, names.iter().copied()) {
        Some(near) => format!("no argument named {key:?}; did you mean {near:?}?"),
        None if names.is_empty() => format!("this tool takes no arguments, so {key:?} is not one"),
        None => format!(
            "no argument named {key:?}; this tool takes {}",
            names.join(", ")
        ),
    }))
}

/// Text from the command line as the type the schema declared. Anything the schema
/// does not pin to one scalar type stays a string: that is what a server asking for
/// a string wants, and the safe reading of a union type, which `:=` overrides.
fn coerce(key: &str, text: &str, spec: &Value) -> Result<Value> {
    match spec["type"].as_str() {
        Some("integer" | "number") => serde_json::from_str::<Number>(text)
            .map(Value::Number)
            .map_err(|_| Error::usage(format!("{key} takes a number; {text:?} is not one"))),
        Some("boolean") => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(Error::usage(format!(
                "{key} takes true or false; {text:?} is neither"
            ))),
        },
        Some("array") => Err(needs_json_form(key, "an array", r#"["a","b"]"#)),
        Some("object") => Err(needs_json_form(key, "an object", r#"{"k":1}"#)),
        _ => Ok(Value::String(text.to_string())),
    }
}

fn needs_json_form(key: &str, shape: &str, example: &str) -> Error {
    Error::usage(format!(
        "{key} takes {shape}; write it as JSON after :=, like {key}:='{example}'"
    ))
}

/// What stands in for one value in [`example_pairs`]: the skeleton the JSON form
/// shows, without the quotes that only JSON needs.
fn pair_placeholder(spec: &Value) -> String {
    if let Some(values) = spec["enum"].as_array().filter(|v| !v.is_empty()) {
        return values
            .iter()
            .take(4)
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join("|");
    }
    match spec["type"].as_str() {
        Some("integer" | "number") => "<number>".into(),
        Some("boolean") => "true|false".into(),
        Some("string") => "<string>".into(),
        _ => "<value>".into(),
    }
}

/// The REPL has no shell in front of it, so a quoted value still carries its
/// quotes: take them off the way a shell would before reading the pairs.
fn split_line(text: &str) -> Result<Vec<String>> {
    split_command(text)
        .map_err(|_| Error::usage(format!("unterminated quote in arguments {text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "fuzzy": {"type": "boolean"},
                "tags": {"type": "array"},
                "meta": {"type": "object"},
                "id": {"type": ["string", "number"]},
                "loose": {},
            },
        })
    }

    fn parse(line: &str) -> Result<Value> {
        parse_pairs(&words(line), &schema())
    }

    fn message(line: &str) -> String {
        parse(line).unwrap_err().to_string()
    }

    #[test]
    fn each_declared_type_is_read_out_of_its_text() {
        assert_eq!(
            parse("query=rust limit=5 ratio=1.5 fuzzy=true").unwrap(),
            json!({"query": "rust", "limit": 5, "ratio": 1.5, "fuzzy": true})
        );
        // A union type, an untyped property and a key the schema never mentions
        // all stay text: a string is what a server asking for one wants, and the
        // safe reading of everything else.
        assert_eq!(
            parse("id=123 loose=7 novel=x").unwrap(),
            json!({"id": "123", "loose": "7", "novel": "x"})
        );
        // := takes the value as written, whatever the schema says.
        assert_eq!(
            parse_pairs(
                &[
                    r#"tags:=["a","b"]"#.into(),
                    r#"meta:={"k":1}"#.into(),
                    "id:=\"123\"".into()
                ],
                &schema()
            )
            .unwrap(),
            json!({"tags": ["a", "b"], "meta": {"k": 1}, "id": "123"})
        );
    }

    #[test]
    fn a_value_the_schema_cannot_take_is_refused_before_anything_is_sent() {
        assert!(message("limit=many").contains("limit takes a number"));
        assert!(message("ratio=").contains("ratio takes a number"));
        assert!(message("fuzzy=yes").contains("fuzzy takes true or false"));
        // An array or an object has to come through the form that carries one.
        assert!(message("tags=a,b").contains(r#"tags:='["a","b"]'"#));
        assert!(message("meta=k:1").contains(r#"meta:='{"k":1}'"#));
        // A JSON value that is not JSON says so, and names the key.
        let e = parse_pairs(&["tags:=[a".into()], &schema()).unwrap_err();
        assert!(
            e.to_string().starts_with("tags:= takes a JSON value"),
            "{e}"
        );
    }

    #[test]
    fn a_key_the_schema_shuts_out_is_named_with_its_near_miss() {
        let mut closed = schema();
        closed["additionalProperties"] = json!(false);
        let refused = |line: &str| parse_pairs(&words(line), &closed).unwrap_err().to_string();
        let near = refused("quary=rust");
        assert!(near.contains(r#"did you mean "query"?"#), "{near}");
        let listed = refused("nowhere=1");
        assert!(
            listed.starts_with(r#"no argument named "nowhere""#),
            "{listed}"
        );
        assert!(
            listed.contains("query") && listed.contains("limit"),
            "{listed}"
        );
        // The check does not care which form the value came in.
        assert!(refused("quary:=1").contains("did you mean \"query\"?"));
        // An open schema sends the key on and lets the server rule on it.
        assert_eq!(parse("quary=rust").unwrap(), json!({"quary": "rust"}));
    }

    #[test]
    fn a_pair_is_split_at_its_first_equals_only() {
        assert_eq!(
            parse("query=a=b loose== fuzzy=false").unwrap(),
            json!({"query": "a=b", "loose": "=", "fuzzy": false})
        );
        // An empty value is an empty string, not a missing one.
        assert_eq!(parse("query=").unwrap(), json!({"query": ""}));
        assert!(parse_pairs(&["=x".into()], &schema())
            .unwrap_err()
            .to_string()
            .contains("needs a name before the ="));
    }

    #[test]
    fn the_json_object_form_is_told_from_the_pairs_by_its_first_character() {
        assert_eq!(form(&[]).unwrap().json(), Some("{}"));
        for object in [r#"{"a":1}"#, "@args.json", "-", "[1,2]", "not json at all"] {
            let arguments = [object.to_string()];
            assert_eq!(form(&arguments).unwrap().json(), Some(object), "{object}");
        }
        let pairs = words("query=rust limit=5");
        assert_eq!(form(&pairs).unwrap().pairs(), &pairs[..]);
        // A lone word carrying an = is a pair, not a would-be object.
        assert!(form(&["query=rust".into()]).unwrap().json().is_none());
    }

    #[test]
    fn the_two_forms_cannot_be_mixed() {
        for mixed in [
            words(r#"{"a":1} b=2"#),
            words("b=2 @args.json"),
            words("b=2 -"),
        ] {
            let e = form(&mixed).unwrap_err().to_string();
            assert!(e.contains("not both at once"), "{mixed:?}: {e}");
        }
        // Two words that are neither is the object a shell split at its commas.
        let e = form(&words("message:hi n:2")).unwrap_err().to_string();
        assert!(e.contains("split a JSON object at its commas"), "{e}");
    }

    #[test]
    fn only_a_pair_that_carries_no_json_of_its_own_needs_the_schema() {
        assert!(!needs_schema(&[r#"tags:=["a"]"#.into(), "id:=1".into()]));
        assert!(needs_schema(&["query=rust".into(), "id:=1".into()]));
        assert!(!needs_schema(&[]));
    }

    #[test]
    fn a_shell_line_keeps_the_quotes_a_shell_would_have_taken_off() {
        let coerced = |line: &str| shell_arguments(line, schema).unwrap();
        assert_eq!(
            coerced(r#"query="rust ureq" limit=5"#),
            json!({"query": "rust ureq", "limit": 5})
        );
        // The object form, and the empty line that stands for an empty object.
        assert_eq!(coerced(r#"{"query":"x"}"#), json!({"query": "x"}));
        assert_eq!(coerced("{}"), json!({}));
        // A line that is neither still answers as a would-be object.
        let e = shell_arguments("nonsense", schema).unwrap_err().to_string();
        assert!(e.contains("must be a JSON object"), "{e}");
    }

    #[test]
    fn the_usage_line_offers_the_pair_form_of_what_a_tool_requires() {
        let tool = json!({
            "name": "search",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"},
                    "tags": {"type": "array"},
                    "mode": {"enum": ["fast", "slow"]},
                },
                "required": ["query", "limit", "tags", "mode"],
            },
        });
        assert_eq!(
            example_pairs(&tool),
            "query=<string> limit=<number> tags:=[...] mode=fast|slow"
        );
        // A tool that requires nothing has no pair form worth printing.
        assert_eq!(example_pairs(&json!({"name": "count"})), "");
    }
}
