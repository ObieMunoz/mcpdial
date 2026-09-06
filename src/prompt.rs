//! Asking for the arguments a call left out.
//!
//! The commonest mistake at a terminal is a call with an argument missing, and
//! the answer to it has been an error and a usage line. The tool's own
//! `inputSchema` already says what is missing and what shape it takes, so at a
//! terminal there is no need to guess: ask.
//!
//! Nothing here decides when to ask. [`Presenter::asks`] does, and it answers no
//! for every program and for every pipe, so a missing argument outside a terminal
//! still gets the byte-for-byte error it always got. That is the one rule this
//! module lives under: a prompt with nobody in front of it waits for ever.
//!
//! What is typed is read against the schema by [`args::parse_pairs`], exactly as
//! the same text in a `key=value` pair on the command line would be, and the
//! finished call is echoed as that command line so it can go into a script.

use crate::args;
use crate::pick::{self, Fzf};
use crate::present::Presenter;
use mcpdial::schema::{self, Asked, Parameter};
use mcpdial::{Error, Result};
use serde_json::Value;

/// Asks for the properties of `schema` that `arguments` has not got, and adds
/// the answers to it. Nothing is asked unless there is a person to ask and one
/// of the missing properties is required: a call that is already complete, and
/// one made by a program, go on untouched.
///
/// `line` is the command up to its arguments; the filled-in call is printed as
/// that line with every argument on it.
pub fn fill(ui: &dyn Presenter, schema: &Value, arguments: &mut Value, line: &str) -> Result<()> {
    if !ui.asks() {
        return Ok(());
    }
    let missing = missing(schema, arguments);
    if !missing.iter().any(|p| p.required) {
        return Ok(());
    }
    ask_each(ui, schema, arguments, &missing)?;
    ui.aside(&format!(
        "{line} {}",
        args::as_pairs(arguments, schema).join(" ")
    ));
    Ok(())
}

/// Asks for every property `arguments` has not got, whether or not any of them
/// is required: what the picker does, where the call is being composed from
/// nothing rather than completed. Nothing is echoed, because the picker prints
/// the whole command line itself once the call has been made.
pub fn ask_all(ui: &dyn Presenter, schema: &Value, arguments: &mut Value) -> Result<()> {
    if !ui.asks() {
        return Ok(());
    }
    let missing = missing(schema, arguments);
    ask_each(ui, schema, arguments, &missing)
}

/// The properties of `schema` that `arguments` has not got. Anything but an
/// object has nothing to fill in.
fn missing<'a>(schema: &'a Value, arguments: &Value) -> Vec<Parameter<'a>> {
    let Some(given) = arguments.as_object() else {
        return Vec::new();
    };
    schema::parameters(schema)
        .into_iter()
        .filter(|p| !given.contains_key(p.name))
        .collect()
}

fn ask_each(
    ui: &dyn Presenter,
    schema: &Value,
    arguments: &mut Value,
    missing: &[Parameter<'_>],
) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let mut asker = Asker::new()?;
    for p in missing {
        if let Some(value) = asker.ask(ui, p, schema)? {
            arguments[p.name] = value;
        }
    }
    Ok(())
}

/// The line editor the answers are typed on, kept for the whole run of
/// questions so the terminal is put into and out of raw mode once.
struct Asker {
    editor: rustyline::DefaultEditor,
}

impl Asker {
    fn new() -> Result<Self> {
        rustyline::DefaultEditor::new()
            .map(|editor| Self { editor })
            .map_err(|e| Error::usage(format!("cannot ask for the missing arguments: {e}")))
    }

    /// The value for one property, or `None` where an optional one was left out.
    /// A value the schema cannot take is said so and asked for again; `^C` and
    /// `^D` give up on the call.
    fn ask(
        &mut self,
        ui: &dyn Presenter,
        p: &Parameter<'_>,
        schema: &Value,
    ) -> Result<Option<Value>> {
        if let Some(about) = schema::summary(p.spec) {
            ui.aside(&format!("  {about}"));
        }
        let asked = schema::asked(p.spec);
        let question = question(p, &asked);
        if let Asked::Choice(values) = &asked {
            return self.pick(ui, p, values, &question);
        }
        loop {
            let answer = self.line(&question)?;
            let answer = answer.trim();
            if answer.is_empty() {
                match p.required {
                    true => ui.aside(&format!("  {} is required", p.name)),
                    false => return Ok(None),
                }
                continue;
            }
            match value_of(p.name, answer, &asked, schema) {
                Ok(value) => return Ok(Some(value)),
                Err(e) => ui.aside(&format!("  {e}")),
            }
        }
    }

    /// One of the values the schema names: `fzf` where it is installed, else a
    /// numbered list, which either the number or the value itself answers.
    fn pick(
        &mut self,
        ui: &dyn Presenter,
        p: &Parameter<'_>,
        values: &[&Value],
        question: &str,
    ) -> Result<Option<Value>> {
        let shown: Vec<String> = values.iter().map(|v| schema::plain(v)).collect();
        match pick::fzf(&format!("{}: ", p.name), &shown) {
            Fzf::Picked(answer) => return Ok(chosen(values, &shown, &answer)),
            // Quitting the picker leaves the value out, which for a required
            // one is leaving out the call.
            Fzf::Quit if p.required => return Err(given_up()),
            Fzf::Quit => return Ok(None),
            Fzf::Absent => {}
        }
        for (i, choice) in shown.iter().enumerate() {
            ui.aside(&format!("    {}) {choice}", i + 1));
        }
        loop {
            let answer = self.line(question)?;
            let answer = answer.trim();
            if answer.is_empty() && !p.required {
                return Ok(None);
            }
            match chosen(values, &shown, answer) {
                Some(value) => return Ok(Some(value)),
                None => ui.aside(&format!("  {} is one of the {} above", p.name, shown.len())),
            }
        }
    }

    fn line(&mut self, question: &str) -> Result<String> {
        use rustyline::error::ReadlineError;
        match self.editor.readline(question) {
            Ok(line) => Ok(line),
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => Err(given_up()),
            Err(e) => Err(Error::usage(format!("cannot read the answer: {e}"))),
        }
    }
}

/// The line an answer is typed after: what the value is called, what it takes,
/// and whether it can be left out.
fn question(p: &Parameter<'_>, asked: &Asked<'_>) -> String {
    let takes = match asked {
        Asked::Choice(values) => format!("1-{}", values.len()),
        Asked::Boolean => "y/n".into(),
        Asked::Number => "number".into(),
        Asked::Json => "JSON".into(),
        Asked::Text => schema::type_name(p.spec),
    };
    let or_not = match p.required {
        true => "required",
        false => "enter to skip",
    };
    format!("  {} ({takes}, {or_not}): ", p.name)
}

/// One typed line as the value the schema declares, read exactly as the same
/// text in a `key=value` pair on the command line would have been read.
fn value_of(key: &str, answer: &str, asked: &Asked<'_>, schema: &Value) -> Result<Value> {
    let pair = match asked {
        // An array or an object is typed as JSON, which is what := takes.
        Asked::Json => format!("{key}:={answer}"),
        Asked::Boolean => format!("{key}={}", yes_or_no(answer)?),
        _ => format!("{key}={answer}"),
    };
    let mut object = args::parse_pairs(std::slice::from_ref(&pair), schema)?;
    Ok(object[key].take())
}

/// A y/n answer as the word the schema's `boolean` is written with.
fn yes_or_no(answer: &str) -> Result<&'static str> {
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" => Ok("true"),
        "n" | "no" | "false" => Ok("false"),
        _ => Err(Error::usage(format!("{answer:?} is neither y nor n"))),
    }
}

/// Which of the values an answer names: its number in the list, or the value
/// itself as the list shows it.
fn chosen(values: &[&Value], shown: &[String], answer: &str) -> Option<Value> {
    pick::numbered(shown, answer).map(|at| values[at].clone())
}

/// Giving up on a question gives up on the call: nothing has been sent, and the
/// line can be typed again with the arguments on it.
fn given_up() -> Error {
    Error::usage("cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to look for"},
                "limit": {"type": "integer"},
                "fuzzy": {"type": "boolean"},
                "tags": {"type": "array"},
                "mode": {"enum": ["fast", "slow"]},
                "id": {"type": ["string", "number"]},
            },
            "required": ["query", "mode"],
        })
    }

    fn parameter<'a>(schema: &'a Value, name: &str) -> Parameter<'a> {
        schema::parameters(schema)
            .into_iter()
            .find(|p| p.name == name)
            .expect("a parameter of the test schema")
    }

    fn asked_for(name: &str) -> String {
        let schema = schema();
        let p = parameter(&schema, name);
        question(&p, &schema::asked(p.spec))
    }

    #[test]
    fn the_question_names_the_value_what_it_takes_and_whether_it_can_be_left_out() {
        assert_eq!(asked_for("query"), "  query (string, required): ");
        assert_eq!(asked_for("limit"), "  limit (number, enter to skip): ");
        assert_eq!(asked_for("fuzzy"), "  fuzzy (y/n, enter to skip): ");
        assert_eq!(asked_for("tags"), "  tags (JSON, enter to skip): ");
        assert_eq!(asked_for("mode"), "  mode (1-2, required): ");
        // A union is typed as a line, and says which line it will be read as.
        assert_eq!(asked_for("id"), "  id (string|number, enter to skip): ");
    }

    #[test]
    fn an_answer_is_read_against_the_schema_as_a_key_value_pair_is() {
        let read = |name: &str, answer: &str| {
            let schema = schema();
            let p = parameter(&schema, name);
            value_of(name, answer, &schema::asked(p.spec), &schema)
        };
        assert_eq!(read("query", "rust ureq").unwrap(), json!("rust ureq"));
        assert_eq!(read("limit", "5").unwrap(), json!(5));
        assert_eq!(read("tags", r#"["a","b"]"#).unwrap(), json!(["a", "b"]));
        // A union stays the text that was typed, as `id=123` does.
        assert_eq!(read("id", "123").unwrap(), json!("123"));
        // y and n are the boolean the schema asked for.
        for yes in ["y", "Y", "yes", "true"] {
            assert_eq!(read("fuzzy", yes).unwrap(), json!(true));
        }
        for no in ["n", "no", "false"] {
            assert_eq!(read("fuzzy", no).unwrap(), json!(false));
        }
        // And what the schema cannot take is said in the words a bad pair gets.
        let e = read("limit", "many").unwrap_err().to_string();
        assert!(e.contains("limit takes a number"), "{e}");
        let e = read("fuzzy", "maybe").unwrap_err().to_string();
        assert!(e.contains("neither y nor n"), "{e}");
        let e = read("tags", "a,b").unwrap_err().to_string();
        assert!(e.contains("is not JSON"), "{e}");
    }

    #[test]
    fn a_named_value_is_picked_by_its_number_or_by_itself() {
        let values = [&json!("fast"), &json!("slow")];
        let shown = ["fast".to_string(), "slow".to_string()];
        assert_eq!(chosen(&values, &shown, "1"), Some(json!("fast")));
        assert_eq!(chosen(&values, &shown, "2"), Some(json!("slow")));
        assert_eq!(chosen(&values, &shown, "slow"), Some(json!("slow")));
        assert_eq!(chosen(&values, &shown, "0"), None);
        assert_eq!(chosen(&values, &shown, "3"), None);
        assert_eq!(chosen(&values, &shown, "medium"), None);
        // Numbers the schema names are picked by their own text too, and where
        // the two readings collide the list's numbering wins: `1` is the first
        // line offered, whatever is written on it.
        let numbers = [&json!(7), &json!(1)];
        let shown = ["7".to_string(), "1".to_string()];
        assert_eq!(chosen(&numbers, &shown, "1"), Some(json!(7)));
        assert_eq!(chosen(&numbers, &shown, "2"), Some(json!(1)));
        assert_eq!(chosen(&numbers, &shown, "7"), Some(json!(7)));
    }

    #[test]
    fn a_program_is_never_asked_anything() {
        let mut arguments = json!({});
        // `Plain` is what every pipe, every `--json` and every `--plain` gets,
        // and it answers no before a question can be composed, let alone read.
        assert!(!crate::present::Plain.asks());
        fill(
            &crate::present::Plain,
            &schema(),
            &mut arguments,
            "mcpdial call web search",
        )
        .unwrap();
        assert_eq!(
            arguments,
            json!({}),
            "nothing was filled in and nothing hung"
        );
    }
}
