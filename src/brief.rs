//! What `--json` says about a listed tool, prompt or resource.
//!
//! `tools/list` hands back every tool's whole `inputSchema`, and on a server
//! with fifty tools that is tens of kilobytes an agent has to read before it
//! can pick one. The name and the first line of the description are enough to
//! choose with, and `schema TARGET TOOL` fetches the chosen one in full, so a
//! listing carries that much; `--long` restores the server's own objects.

use serde_json::{Map, Value};

/// What a listed tool keeps beside its description: what to call it, and the
/// mark `tools --all` puts on one an allow or deny list hides.
const TOOL: [&str; 2] = ["name", "denied"];

/// A prompt is named the same way; its `arguments` are what `--long` is for.
const PROMPT: [&str; 1] = ["name"];

/// A resource is addressed by its URI, a template by the pattern for one, and
/// a listing with no description at all still has the name to show.
const RESOURCE: [&str; 3] = ["uri", "uriTemplate", "name"];

pub fn tools(tools: &[Value], long: bool) -> Vec<Value> {
    shorten(tools, long, &TOOL)
}

pub fn prompts(prompts: &[Value], long: bool) -> Vec<Value> {
    shorten(prompts, long, &PROMPT)
}

/// Resources and resource templates alike.
pub fn resources(resources: &[Value], long: bool) -> Vec<Value> {
    shorten(resources, long, &RESOURCE)
}

fn shorten(items: &[Value], long: bool, keep: &[&str]) -> Vec<Value> {
    if long {
        return items.to_vec();
    }
    items.iter().map(|item| kept(item, keep)).collect()
}

fn kept(item: &Value, keep: &[&str]) -> Value {
    let mut out = Map::new();
    for key in keep {
        if let Some(value) = item.get(key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    if let Some(description) = item.get("description").and_then(Value::as_str) {
        out.insert("description".into(), first_line(description).into());
    }
    Value::Object(out)
}

fn first_line(description: &str) -> &str {
    description.trim().lines().next().unwrap_or("").trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn echo() -> Value {
        json!({
            "name": "echo",
            "description": "Echo a message back.\nSecond line.",
            "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}},
            "outputSchema": {"type": "object"},
            "annotations": {"idempotentHint": true},
        })
    }

    #[test]
    fn a_listed_tool_is_its_name_and_the_first_line_of_its_description() {
        assert_eq!(
            tools(&[echo()], false),
            vec![json!({"name": "echo", "description": "Echo a message back."})]
        );
    }

    #[test]
    fn long_hands_back_the_servers_own_objects() {
        assert_eq!(tools(&[echo()], true), vec![echo()]);
    }

    #[test]
    fn a_hidden_tool_stays_marked_and_a_description_may_be_missing() {
        let listed = tools(
            &[json!({"name": "delete_file", "denied": true}), json!({})],
            false,
        );
        assert_eq!(
            listed,
            vec![json!({"name": "delete_file", "denied": true}), json!({})]
        );
    }

    #[test]
    fn a_prompt_drops_its_arguments_and_a_resource_keeps_what_addresses_it() {
        assert_eq!(
            prompts(
                &[json!({"name": "summarize", "description": "Summarize.", "arguments": []})],
                false
            ),
            vec![json!({"name": "summarize", "description": "Summarize."})]
        );
        assert_eq!(
            resources(
                &[
                    json!({"uri": "file:///readme.md", "name": "readme", "mimeType": "text/markdown"}),
                    json!({"uriTemplate": "file:///notes/{name}.md", "name": "note"}),
                ],
                false
            ),
            vec![
                json!({"uri": "file:///readme.md", "name": "readme"}),
                json!({"uriTemplate": "file:///notes/{name}.md", "name": "note"}),
            ]
        );
    }
}
