//! What a tool's JSON Schema says one value is.
//!
//! `call` reads a `key=value` pair against a tool's `inputSchema`, the tool
//! summaries name the types out of that same schema, and a prompt asks for a
//! missing argument in the shape it declares. Reading it in one place is what
//! keeps the type a parameter is described as, the type a value is asked for as
//! and the type it is sent as the same type.
//!
//! Nothing here resolves `$ref`, and every step into a nested schema spends from
//! a fixed budget. A schema arrives from a server: one that nests a thousand deep
//! or points back at itself must cost a summary no more than a flat one does.

use serde_json::{Map, Value};

/// How many members of a list a summary names, be they an `anyOf`'s alternatives
/// or an object's properties, before it says only that there are more.
const LISTED: usize = 4;

/// Levels of structure a summary descends: an array's items, an object's
/// properties. One, so a parameter shows its own shape and not its whole tree.
const DEPTH: u8 = 1;

/// Layers of `anyOf` a summary spells out. One: alternatives of alternatives are
/// a shape to read in `schema`, not in a line about a parameter.
const UNIONS: u8 = 1;

/// The type of one value as a summary names it: `string`, `string[]`,
/// `string|string[]`, `object {x: number, y?: string}`.
pub fn type_name(spec: &Value) -> String {
    named(spec, DEPTH, UNIONS)
}

/// The properties of an object schema, keyed by name. `None` when the schema
/// describes something other than an object, or names no properties.
pub fn properties(spec: &Value) -> Option<&Map<String, Value>> {
    example_of(spec)["properties"]
        .as_object()
        .filter(|props| !props.is_empty())
}

/// Every value a schema allows outright, for offering a choice between them.
/// [`choices`] is the same list cut to what a one-line summary can carry; this
/// one is whole, because a picker that hid the fifth option would be wrong.
pub fn allowed_values(spec: &Value) -> Vec<&Value> {
    let spec = example_of(spec);
    if let Some(values) = spec["enum"].as_array().filter(|v| !v.is_empty()) {
        return values.iter().collect();
    }
    spec.get("const").into_iter().collect()
}

/// One parameter of a tool's object schema.
pub struct Parameter<'a> {
    pub name: &'a str,
    pub spec: &'a Value,
    pub required: bool,
}

/// A tool's parameters, the required ones first and each group in the order the
/// schema declares them: the order a summary lists them in, and the order a
/// prompt asks for them in.
pub fn parameters(schema: &Value) -> Vec<Parameter<'_>> {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = properties(schema) else {
        return Vec::new();
    };
    let mut ordered: Vec<Parameter<'_>> = props
        .iter()
        .map(|(name, spec)| Parameter {
            name,
            spec,
            required: required.contains(&name.as_str()),
        })
        .collect();
    // Required first: they are what a caller has to get right. The sort is
    // stable, so everything else keeps the order the schema listed it in.
    ordered.sort_by_key(|p| !p.required);
    ordered
}

/// What has to be asked for to fill in one value: the schema's own answer to
/// what a person would type here.
#[derive(Debug, PartialEq)]
pub enum Asked<'a> {
    /// One of the values the schema names outright.
    Choice(Vec<&'a Value>),
    /// Yes or no.
    Boolean,
    /// A number.
    Number,
    /// JSON, because no plain line of text is an array or an object.
    Json,
    /// A line of text, read as whatever type the schema declares.
    Text,
}

/// How to ask for a value of this schema. Alternatives are read as an example
/// value reads them: the first of them stands for all of them.
pub fn asked(spec: &Value) -> Asked<'_> {
    let values = allowed_values(spec);
    if !values.is_empty() {
        return Asked::Choice(values);
    }
    match example_of(spec)["type"].as_str() {
        Some("boolean") => Asked::Boolean,
        Some("number" | "integer") => Asked::Number,
        Some("array" | "object") => Asked::Json,
        _ => Asked::Text,
    }
}

/// One named value as it is written outside JSON: a string without its quotes,
/// anything else as JSON writes it.
pub fn plain(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

/// The one line of a schema's `description` worth putting beside a value: the
/// first, where there is one that says anything.
pub fn summary(spec: &Value) -> Option<&str> {
    spec["description"]
        .as_str()?
        .trim()
        .lines()
        .next()
        .filter(|first| !first.is_empty())
}

/// What stands in for one value in an example JSON arguments object.
pub fn json_placeholder(spec: &Value) -> String {
    let spec = example_of(spec);
    if let Some(values) = choices(spec) {
        return values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|");
    }
    match spec["type"].as_str() {
        Some("string") => "\"<string>\"".into(),
        Some("number" | "integer") => "<number>".into(),
        Some("boolean") => "true|false".into(),
        Some("array") => "[...]".into(),
        Some("object") => "{...}".into(),
        _ => "<value>".into(),
    }
}

/// What stands in for one value in an example `key=value` pair: the skeleton the
/// JSON form shows, without the quotes that only JSON needs.
pub fn pair_placeholder(spec: &Value) -> String {
    let spec = example_of(spec);
    if let Some(values) = choices(spec) {
        return values
            .iter()
            .map(|v| plain(v))
            .collect::<Vec<_>>()
            .join("|");
    }
    match spec["type"].as_str() {
        Some("number" | "integer") => "<number>".into(),
        Some("boolean") => "true|false".into(),
        Some("string") => "<string>".into(),
        _ => "<value>".into(),
    }
}

/// The type an example value for this schema is written as, which is the type of
/// the alternative a placeholder was taken from.
pub fn example_type(spec: &Value) -> Option<&str> {
    example_of(spec)["type"].as_str()
}

/// `depth` is how many levels of structure a summary may still descend, `unions`
/// how many layers of alternatives it may still spell out. Every call down spends
/// one of the two, which is what makes a schema of any depth finite here.
fn named(spec: &Value, depth: u8, unions: u8) -> String {
    let declared: Vec<&str> = match &spec["type"] {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !declared.is_empty() {
        return joined(
            declared
                .into_iter()
                .map(|ty| structured(ty, spec, depth, unions)),
        );
    }
    if let Some(branches) = alternatives(spec).filter(|_| unions > 0) {
        return joined(branches.iter().map(|b| named(b, depth, unions - 1)));
    }
    if spec.get("enum").is_some() {
        return "enum".into();
    }
    if let Some(only) = spec.get("const") {
        return only.to_string();
    }
    "any".into()
}

/// One declared type with whatever the schema says about its insides: an array's
/// item type, an object's properties. Anything else is its own name.
fn structured(ty: &str, spec: &Value, depth: u8, unions: u8) -> String {
    if depth == 0 {
        return ty.to_string();
    }
    match ty {
        "array" if spec["items"].is_object() => {
            format!("{}[]", grouped(named(&spec["items"], depth - 1, unions)))
        }
        "object" => match fields(spec, depth, unions) {
            Some(listed) => format!("object {{{listed}}}"),
            None => ty.to_string(),
        },
        _ => ty.to_string(),
    }
}

/// An object's properties, one level in, with everything the `required` list does
/// not name marked `?`.
fn fields(spec: &Value, depth: u8, unions: u8) -> Option<String> {
    let props = spec["properties"].as_object().filter(|p| !p.is_empty())?;
    let required: Vec<&str> = spec["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut listed: Vec<String> = props
        .iter()
        .take(LISTED)
        .map(|(name, field)| {
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{optional}: {}", named(field, depth - 1, unions))
        })
        .collect();
    if props.len() > LISTED {
        listed.push("...".into());
    }
    Some(listed.join(", "))
}

/// The alternatives of an `anyOf` or `oneOf`, where one value has more than one
/// shape.
fn alternatives(spec: &Value) -> Option<&Vec<Value>> {
    ["anyOf", "oneOf"]
        .into_iter()
        .find_map(|key| spec[key].as_array())
        .filter(|branches| !branches.is_empty())
}

/// As many of the values a schema names as a summary spells out.
fn choices(spec: &Value) -> Option<Vec<&Value>> {
    let values = allowed_values(spec);
    (!values.is_empty()).then(|| values.into_iter().take(LISTED).collect())
}

/// The schema an example value is written from: any one alternative satisfies an
/// `anyOf`, so the first stands in for all of them.
fn example_of(spec: &Value) -> &Value {
    let mut spec = spec;
    for _ in 0..UNIONS {
        match alternatives(spec) {
            Some(branches) if spec["type"].is_null() => spec = &branches[0],
            _ => break,
        }
    }
    spec
}

fn joined(parts: impl Iterator<Item = String>) -> String {
    let mut listed: Vec<String> = parts.take(LISTED + 1).collect();
    if listed.len() > LISTED {
        listed[LISTED] = "...".into();
    }
    listed.join("|")
}

/// An item type in brackets when it is a union, so that `(string|number)[]` is
/// not read as a string or an array of numbers.
fn grouped(item: String) -> String {
    if item.contains(['|', ' ']) {
        format!("({item})")
    } else {
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_union_is_spelled_out_and_a_container_carries_what_it_holds() {
        let name = |spec: Value| type_name(&spec);
        assert_eq!(name(json!({"type": "string"})), "string");
        assert_eq!(name(json!({"type": ["number", "null"]})), "number|null");
        assert_eq!(
            name(
                json!({"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]})
            ),
            "string|string[]"
        );
        assert_eq!(
            name(json!({"oneOf": [{"type": "integer"}, {"type": "null"}]})),
            "integer|null"
        );
        assert_eq!(
            name(json!({"type": "array", "items": {"type": "object"}})),
            "object[]"
        );
        // An array of a union takes brackets, so the [] is not read as the last
        // alternative's alone.
        assert_eq!(
            name(json!({"type": "array", "items": {"type": ["string", "number"]}})),
            "(string|number)[]"
        );
        // An array whose items say nothing, and the tuple form, stay `array`.
        assert_eq!(name(json!({"type": "array"})), "array");
        assert_eq!(
            name(json!({"type": "array", "items": [{"type": "string"}]})),
            "array"
        );
    }

    #[test]
    fn an_object_shows_its_fields_one_level_in() {
        assert_eq!(
            type_name(&json!({"type": "object", "properties": {
                "x": {"type": "number"},
                "y": {"type": "string"},
            }, "required": ["x"]})),
            "object {x: number, y?: string}"
        );
        // One level in: a field's own object is named, not unfolded.
        assert_eq!(
            type_name(&json!({"type": "object", "properties": {
                "at": {"type": "object", "properties": {"x": {"type": "number"}}},
            }})),
            "object {at?: object}"
        );
        // An object that declares no properties has nothing to add.
        assert_eq!(type_name(&json!({"type": "object"})), "object");
        assert_eq!(
            type_name(&json!({"type": "object", "properties": {}})),
            "object"
        );
    }

    #[test]
    fn a_named_value_keeps_its_word_and_an_unnamed_one_is_any() {
        assert_eq!(type_name(&json!({"enum": ["x", "y"]})), "enum");
        assert_eq!(type_name(&json!({"const": "gpt-4"})), r#""gpt-4""#);
        assert_eq!(type_name(&json!({"const": 5})), "5");
        assert_eq!(type_name(&json!({})), "any");
        assert_eq!(type_name(&json!({"$ref": "#/$defs/Point"})), "any");
        // A type the schema declares wins over an enum of its values, as it always did.
        assert_eq!(
            type_name(&json!({"type": "string", "enum": ["x"]})),
            "string"
        );
    }

    #[test]
    fn a_long_list_says_there_are_more_rather_than_running_on() {
        let many: Vec<Value> = ["string", "number", "boolean", "null", "integer", "object"]
            .iter()
            .map(|ty| json!({"type": ty}))
            .collect();
        assert_eq!(
            type_name(&json!({"anyOf": many})),
            "string|number|boolean|null|..."
        );
        let wide: serde_json::Map<String, Value> = "abcdefgh"
            .chars()
            .map(|c| (c.to_string(), json!({"type": "string"})))
            .collect();
        assert_eq!(
            type_name(&json!({"type": "object", "properties": wide})),
            "object {a?: string, b?: string, c?: string, d?: string, ...}"
        );
    }

    #[test]
    fn a_schema_without_a_bottom_costs_no_more_than_a_flat_one() {
        // Depth and alternatives are both spent on the way down, so neither an
        // array of an array of an array nor an anyOf of an anyOf can recur past
        // the budget, however deep the server's schema goes.
        let mut nested = json!({"type": "string"});
        let mut union = json!({"type": "string"});
        for _ in 0..500 {
            nested = json!({"type": "array", "items": nested});
            union = json!({"anyOf": [union]});
        }
        assert_eq!(type_name(&nested), "array[]");
        assert_eq!(type_name(&union), "any");
        assert_eq!(json_placeholder(&union), "<value>");

        // `$ref` is never followed, so a schema that points back at itself is
        // read as the one node it is.
        let cyclic = json!({
            "type": "object",
            "properties": {"child": {"$ref": "#"}},
            "$defs": {"loop": {"$ref": "#/$defs/loop"}},
        });
        assert_eq!(type_name(&cyclic), "object {child?: any}");
    }

    #[test]
    fn what_a_schema_offers_is_whole_where_a_summary_is_cut() {
        let props = |spec: Value| properties(&spec).map(|p| p.keys().cloned().collect::<Vec<_>>());
        assert_eq!(
            props(json!({"type": "object", "properties": {"b": {}, "a": {}}})),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        // An object with nothing to offer, and things that are no object at all.
        assert_eq!(props(json!({"type": "object", "properties": {}})), None);
        assert_eq!(props(json!({"type": "string"})), None);
        assert_eq!(props(Value::Null), None);
        // The alternative an example is written from is the one whose fields count.
        assert_eq!(
            props(json!({"anyOf": [{"type": "object", "properties": {"x": {}}}]})),
            Some(vec!["x".to_string()])
        );

        // Every value, where `type_name` and the placeholders stop at four.
        let many = json!({"enum": ["a", "b", "c", "d", "e", "f"]});
        assert_eq!(allowed_values(&many).len(), 6);
        assert_eq!(json_placeholder(&many), r#""a"|"b"|"c"|"d""#);
        assert_eq!(allowed_values(&json!({"const": 5})), vec![&json!(5)]);
        assert!(allowed_values(&json!({"type": "string"})).is_empty());
        assert!(allowed_values(&json!({"enum": []})).is_empty());
    }

    #[test]
    fn the_type_decides_what_a_prompt_asks_for() {
        let ask = |spec: Value| match asked(&spec) {
            Asked::Choice(values) => format!("choice {}", values.len()),
            Asked::Boolean => "boolean".into(),
            Asked::Number => "number".into(),
            Asked::Json => "json".into(),
            Asked::Text => "text".into(),
        };
        assert_eq!(ask(json!({"type": "string"})), "text");
        assert_eq!(ask(json!({"type": "boolean"})), "boolean");
        assert_eq!(ask(json!({"type": "number"})), "number");
        assert_eq!(ask(json!({"type": "integer"})), "number");
        assert_eq!(ask(json!({"type": "array"})), "json");
        assert_eq!(ask(json!({"type": "object"})), "json");
        // A union, and a property the schema says nothing about, are typed as a
        // line and read as `key=value` reads one.
        assert_eq!(ask(json!({"type": ["string", "null"]})), "text");
        assert_eq!(ask(json!({})), "text");
        // Named values are picked from, however many there are, and whichever
        // of the two ways the schema names them.
        assert_eq!(
            ask(json!({"enum": ["a", "b", "c", "d", "e", "f"]})),
            "choice 6"
        );
        assert_eq!(ask(json!({"const": "only"})), "choice 1");
        // Values named beside a declared type are still the values, as they are
        // in an example: a summary calls that property a string, but there is
        // only one string it can be.
        assert_eq!(ask(json!({"type": "string", "enum": ["x"]})), "choice 1");
        // One alternative satisfies an anyOf, so the first stands for all.
        assert_eq!(
            ask(json!({"anyOf": [{"type": "number"}, {"type": "string"}]})),
            "number"
        );
        assert_eq!(ask(json!({"enum": []})), "text");
    }

    #[test]
    fn parameters_come_out_required_first_and_otherwise_as_declared() {
        let schema = json!({"type": "object", "properties": {
            "b": {"type": "string"},
            "a": {"type": "number"},
            "z": {"type": "string"},
        }, "required": ["z", "a"]});
        let listed: Vec<(&str, bool)> = parameters(&schema)
            .iter()
            .map(|p| (p.name, p.required))
            .collect();
        assert_eq!(
            listed,
            [("a", true), ("z", true), ("b", false)],
            "required in the schema's order, then the rest in the schema's order"
        );
        assert!(parameters(&json!({"type": "object"})).is_empty());
        assert!(parameters(&Value::Null).is_empty());
    }

    #[test]
    fn a_named_value_is_written_without_the_quotes_json_needs() {
        assert_eq!(plain(&json!("fast")), "fast");
        assert_eq!(plain(&json!(5)), "5");
        assert_eq!(plain(&json!(null)), "null");
    }

    #[test]
    fn an_example_value_comes_from_the_first_alternative() {
        let repo_name = json!({"anyOf": [{"type": "string"}, {"type": "array"}]});
        assert_eq!(json_placeholder(&repo_name), "\"<string>\"");
        assert_eq!(pair_placeholder(&repo_name), "<string>");
        assert_eq!(example_type(&repo_name), Some("string"));
        // A declared type is the schema's own answer; the alternatives refine it.
        let counted = json!({"type": "integer", "anyOf": [{"type": "string"}]});
        assert_eq!(json_placeholder(&counted), "<number>");
        // The values a schema names are the example, whichever form asks.
        assert_eq!(
            json_placeholder(&json!({"enum": ["fast", "slow"]})),
            r#""fast"|"slow""#
        );
        assert_eq!(
            pair_placeholder(&json!({"enum": ["fast", "slow"]})),
            "fast|slow"
        );
        assert_eq!(json_placeholder(&json!({"const": 5})), "5");
        assert_eq!(pair_placeholder(&json!({"const": "only"})), "only");
        assert_eq!(json_placeholder(&json!({})), "<value>");
        assert_eq!(example_type(&json!({})), None);
    }
}
