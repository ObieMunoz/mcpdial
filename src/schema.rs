//! What a tool's JSON Schema says one value is.
//!
//! `call` reads a `key=value` pair against a tool's `inputSchema` and the tool
//! summaries name the types out of that same schema. Reading it in one place is
//! what keeps the type a parameter is described as and the type a value is sent
//! as the same type.
//!
//! Nothing here resolves `$ref`, and every step into a nested schema spends from
//! a fixed budget. A schema arrives from a server: one that nests a thousand deep
//! or points back at itself must cost a summary no more than a flat one does.

use serde_json::Value;

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
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
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

/// The values a schema names outright: an `enum`'s members, or the one value a
/// `const` allows.
fn choices(spec: &Value) -> Option<Vec<&Value>> {
    if let Some(values) = spec["enum"].as_array().filter(|v| !v.is_empty()) {
        return Some(values.iter().take(LISTED).collect());
    }
    spec.get("const").map(|only| vec![only])
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
