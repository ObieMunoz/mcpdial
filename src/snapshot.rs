//! A server's tool list as a file, and what has changed since it was written.
//!
//! A script written against `read_text_file(path)` breaks silently when the
//! server renames the parameter or adds a required one: the call goes out, the
//! server rejects it or, worse, accepts it and does something else. `tools
//! TARGET --snapshot FILE` writes down what the server offers today, a
//! repository commits that file next to the scripts that depend on it, and
//! `--check FILE` says whether the server still keeps the promise.
//!
//! A snapshot holds the server's own tool objects in full, not the name and
//! first line [`crate::brief`] shortens a listing to. Drift is a change in
//! `inputSchema`, which is exactly the part a listing drops.
//!
//! The comparison reaches one level into `inputSchema`: which properties the
//! object has, which of them `required` names, and what [`mcpdial::schema`]
//! calls each one's type - the same words a tool summary uses, so a parameter
//! is described here as it is described everywhere else. A nested schema is
//! compared as one value. That keeps the answer small and predictable, and
//! `--strict` is there for a caller who wants the whole object held still.

use crate::output;
use crate::present::Presenter;
use mcpdial::schema::type_name;
use mcpdial::Error;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

/// The flag a written snapshot's errors name.
const FLAG: &str = "--snapshot";

/// What a snapshot file holds beside the tools: enough to say which server was
/// dialed and what it was speaking, for a person reading the file later.
/// Neither is compared - a version bump is not drift, and a caller who dialed
/// the wrong server has a listing full of differences to tell them so.
pub fn document(server_info: &Value, tools: &[Value]) -> Value {
    let mut sorted = tools.to_vec();
    sorted.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    json!({
        "server": {
            "name": server_info["serverInfo"]["name"],
            "version": server_info["serverInfo"]["version"],
        },
        "protocolVersion": server_info["protocolVersion"],
        "tools": sorted,
    })
}

/// Whether the path can hold a snapshot, answered before anything is dialed,
/// so a path that was never going to work costs no connection.
pub fn reserve(path: &Path) -> Result<(), Error> {
    output::reserve(FLAG, path)
}

/// The document as a file that was not there a moment ago.
pub fn write(path: &Path, document: &Value) -> Result<(), Error> {
    output::create(FLAG, path, serialize(document).as_bytes())
}

/// The one line a written snapshot leaves in place of the listing it replaces.
pub fn wrote(ui: &dyn Presenter, json: bool, path: &Path, tools: usize) {
    let path = path.display().to_string();
    if json {
        ui.json(&json!({"snapshot": path, "tools": tools}).to_string());
    } else {
        ui.line(&format!("wrote {tools} tool(s) to {path}"));
    }
}

/// The document as the file holds it: pretty-printed, one trailing newline,
/// and every object's keys in sorted order, which `serde_json` does for us as
/// long as `preserve_order` stays off. A file a repository commits has to have
/// the same bytes on every machine, or the diff is noise.
fn serialize(document: &Value) -> String {
    let mut text = serde_json::to_string_pretty(document).expect("a JSON value is serializable");
    text.push('\n');
    text
}

/// The tools a snapshot file names, in the order it holds them.
///
/// Anything that is not a snapshot is a usage error rather than an empty
/// comparison that would pass: a caller who mistyped the path must hear about
/// it, not be told the server is fine.
pub fn read(path: &Path) -> Result<Vec<Value>, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::usage(format!("--check {}: {e}", path.display())))?;
    let document: Value = serde_json::from_str(&text)
        .map_err(|e| Error::usage(format!("--check {}: not JSON: {e}", path.display())))?;
    document["tools"].as_array().cloned().ok_or_else(|| {
        Error::usage(format!(
            "--check {}: no tools array; write one with mcpdial tools TARGET --snapshot",
            path.display()
        ))
    })
}

/// One way the server's list differs from the snapshot.
pub struct Difference {
    pub tool: String,
    /// A word a program can match on, stable across releases.
    pub kind: &'static str,
    /// The same difference for a person, without the tool's name in it.
    pub detail: String,
    /// Whether this is the kind of change that breaks a caller holding the
    /// snapshot. `--strict` makes almost every difference one.
    pub breaking: bool,
}

impl Difference {
    fn line(&self) -> String {
        let prefix = if self.breaking { "" } else { "info: " };
        format!("{prefix}tool {}: {}", self.tool, self.detail)
    }

    fn to_json(&self) -> Value {
        json!({
            "tool": self.tool,
            "kind": self.kind,
            "detail": self.detail,
            "level": if self.breaking { "fail" } else { "info" },
        })
    }
}

/// Everything [`compare`] found, and whether a caller holding the snapshot can
/// still go ahead.
pub struct Comparison {
    pub differences: Vec<Difference>,
}

/// Where a report goes. `tools --check` is asked for the comparison, so the
/// comparison is its stdout; `call --check` was asked for a tool's result, and
/// stdout must hold that or nothing, so its report goes where an error goes.
#[derive(Clone, Copy)]
pub enum Report {
    Stdout,
    Stderr,
}

impl Comparison {
    pub fn ok(&self) -> bool {
        !self.differences.iter().any(|d| d.breaking)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "differences": self.differences.iter().map(Difference::to_json).collect::<Vec<_>>(),
        })
    }

    /// One line per difference, then `ok` or the number of breaking ones, which
    /// is the number a caller has to do something about.
    pub fn report(&self, ui: &dyn Presenter, json: bool, to: Report) {
        let say = |line: &str| match to {
            Report::Stdout => ui.line(line),
            Report::Stderr => ui.err_line(line),
        };
        if json {
            say(&self.to_json().to_string());
            return;
        }
        for difference in &self.differences {
            say(&difference.line());
        }
        let breaking = self.differences.iter().filter(|d| d.breaking).count();
        say(&match breaking {
            0 => "ok".to_string(),
            1 => "1 difference".to_string(),
            n => format!("{n} differences"),
        });
    }
}

/// The one tool a `call --check` is about, from either list.
pub fn named(tools: &[Value], name: &str) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| t["name"] == name)
        .cloned()
        .collect()
}

/// The snapshot's tools against the server's, oldest promise first.
///
/// Compatible mode reports what breaks a caller: a tool that is gone, a
/// required property that was removed or changed type, a property that is
/// required now and was not. A new tool, a new optional property and a
/// requirement the server dropped are listed too, as `info`, because a reader
/// wants to see them and none of them breaks anything.
///
/// `strict` makes every one of those breaking but the arrival of a whole new
/// tool, and adds any remaining difference between the two objects, so that a
/// snapshotted tool has to be what it was, key for key.
pub fn compare(snapshot: &[Value], live: &[Value], strict: bool) -> Comparison {
    let mut differences = Vec::new();
    for was in snapshot {
        let name = name_of(was);
        let Some(now) = live.iter().find(|t| name_of(t) == name) else {
            differences.push(Difference {
                tool: name.to_string(),
                kind: "tool-missing",
                detail: "gone from the server".into(),
                breaking: true,
            });
            continue;
        };
        let found = differences.len();
        properties(name, was, now, strict, &mut differences);
        if strict {
            let explained = differences.len() > found;
            keys(name, was, now, explained, &mut differences);
        }
    }
    for now in live {
        let name = name_of(now);
        if !snapshot.iter().any(|t| name_of(t) == name) {
            differences.push(Difference {
                tool: name.to_string(),
                kind: "tool-added",
                detail: "not in the snapshot".into(),
                breaking: false,
            });
        }
    }
    Comparison { differences }
}

/// The top level of one tool's `inputSchema`, property by property.
fn properties(tool: &str, was: &Value, now: &Value, strict: bool, out: &mut Vec<Difference>) {
    let (before, after) = (&was["inputSchema"], &now["inputSchema"]);
    let (had, has) = (fields(before), fields(after));
    let (needed, needs) = (required(before), required(after));
    let mut difference = |kind, detail: String, breaking| {
        out.push(Difference {
            tool: tool.to_string(),
            kind,
            detail,
            breaking: breaking || strict,
        });
    };
    for (name, spec) in &had {
        match has.get(name) {
            None if needed.contains(name.as_str()) => difference(
                "property-removed",
                format!("required property {name} was removed"),
                true,
            ),
            None => difference(
                "property-removed",
                format!("optional property {name} was removed"),
                false,
            ),
            Some(now_spec) => {
                let (was_type, now_type) = (type_name(spec), type_name(now_spec));
                if was_type != now_type {
                    difference(
                        "type-changed",
                        format!("property {name} is {now_type}, was {was_type}"),
                        true,
                    );
                }
                if needs.contains(name.as_str()) && !needed.contains(name.as_str()) {
                    difference(
                        "now-required",
                        format!("property {name} is now required"),
                        true,
                    );
                } else if needed.contains(name.as_str()) && !needs.contains(name.as_str()) {
                    difference(
                        "now-optional",
                        format!("required property {name} is now optional"),
                        false,
                    );
                }
            }
        }
    }
    for name in has.keys() {
        if had.contains_key(name) {
            continue;
        }
        if needs.contains(name.as_str()) {
            difference(
                "now-required",
                format!("property {name} is new and required"),
                true,
            );
        } else {
            difference(
                "property-added",
                format!("optional property {name} was added"),
                false,
            );
        }
    }
}

/// What `--strict` adds: every top-level key of the tool object that is not
/// what the snapshot holds. `inputSchema` is left to [`properties`] whenever
/// that had something to say, so a renamed parameter is named once and not
/// followed by a line saying its schema differs.
fn keys(tool: &str, was: &Value, now: &Value, explained: bool, out: &mut Vec<Difference>) {
    let (before, after) = (object(was), object(now));
    let named: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for key in named {
        if key == "inputSchema" && explained {
            continue;
        }
        let detail = match (before.get(key), after.get(key)) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(_), Some(_)) => format!("{key} differs from the snapshot"),
            (Some(_), None) => format!("{key} was removed"),
            (None, _) => format!("{key} was added"),
        };
        out.push(Difference {
            tool: tool.to_string(),
            kind: "differs",
            detail,
            breaking: true,
        });
    }
}

fn name_of(tool: &Value) -> &str {
    tool["name"].as_str().unwrap_or("")
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn fields(input_schema: &Value) -> Map<String, Value> {
    object(&input_schema["properties"])
}

fn required(input_schema: &Value) -> BTreeSet<&str> {
    input_schema["required"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(properties: Value, required: Value) -> Value {
        json!({
            "name": "read_text_file",
            "description": "Read a file.",
            "inputSchema": {"type": "object", "properties": properties, "required": required},
        })
    }

    fn path_only() -> Value {
        tool(json!({"path": {"type": "string"}}), json!(["path"]))
    }

    fn lines(comparison: &Comparison) -> Vec<String> {
        comparison
            .differences
            .iter()
            .map(Difference::line)
            .collect()
    }

    #[test]
    fn a_list_that_did_not_move_has_nothing_to_say() {
        let comparison = compare(&[path_only()], &[path_only()], false);
        assert!(comparison.ok());
        assert!(comparison.differences.is_empty());
        assert!(compare(&[path_only()], &[path_only()], true).ok());
    }

    #[test]
    fn a_tool_that_is_gone_breaks_every_caller_holding_the_snapshot() {
        let comparison = compare(&[path_only()], &[], false);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: gone from the server"]
        );
        assert_eq!(comparison.differences[0].kind, "tool-missing");
    }

    #[test]
    fn a_required_property_that_was_removed_or_retyped_is_a_failure() {
        let renamed = tool(json!({"file": {"type": "string"}}), json!(["file"]));
        let comparison = compare(&[path_only()], &[renamed], false);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            [
                "tool read_text_file: required property path was removed",
                "tool read_text_file: property file is new and required",
            ]
        );

        let retyped = tool(
            json!({"path": {"type": "array", "items": {"type": "string"}}}),
            json!(["path"]),
        );
        let comparison = compare(&[path_only()], &[retyped], false);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: property path is string[], was string"]
        );
        assert_eq!(comparison.differences[0].kind, "type-changed");
    }

    #[test]
    fn a_new_requirement_is_a_failure_whether_the_property_is_new_or_not() {
        let both_required = tool(
            json!({"path": {"type": "string"}, "encoding": {"type": "string"}}),
            json!(["path", "encoding"]),
        );
        let optional_encoding = tool(
            json!({"path": {"type": "string"}, "encoding": {"type": "string"}}),
            json!(["path"]),
        );
        let comparison = compare(
            &[optional_encoding],
            std::slice::from_ref(&both_required),
            false,
        );
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: property encoding is now required"]
        );
        let comparison = compare(&[path_only()], std::slice::from_ref(&both_required), false);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: property encoding is new and required"]
        );
    }

    #[test]
    fn what_only_widens_what_the_server_takes_is_information_not_failure() {
        let relaxed = tool(
            json!({"path": {"type": "string"}, "encoding": {"type": "string"}}),
            json!([]),
        );
        let comparison = compare(&[path_only()], &[relaxed], false);
        assert!(comparison.ok(), "a caller still sends what it always sent");
        assert_eq!(
            lines(&comparison),
            [
                "info: tool read_text_file: required property path is now optional",
                "info: tool read_text_file: optional property encoding was added",
            ]
        );
        let kinds: Vec<&str> = comparison.differences.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, ["now-optional", "property-added"]);
    }

    #[test]
    fn an_optional_property_that_vanished_is_reported_without_failing() {
        let had_encoding = tool(
            json!({"path": {"type": "string"}, "encoding": {"type": "string"}}),
            json!(["path"]),
        );
        let comparison = compare(&[had_encoding], &[path_only()], false);
        assert!(comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["info: tool read_text_file: optional property encoding was removed"]
        );
    }

    #[test]
    fn a_tool_the_snapshot_never_saw_is_information_in_both_modes() {
        let added = json!({"name": "write_text_file", "inputSchema": {"type": "object"}});
        for strict in [false, true] {
            let comparison = compare(&[path_only()], &[path_only(), added.clone()], strict);
            assert!(comparison.ok(), "strict={strict}");
            assert_eq!(
                lines(&comparison),
                ["info: tool write_text_file: not in the snapshot"]
            );
        }
    }

    #[test]
    fn strict_holds_the_whole_object_still() {
        let reworded = json!({
            "name": "read_text_file",
            "description": "Read a text file.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}},
                            "required": ["path"]},
        });
        assert!(compare(&[path_only()], std::slice::from_ref(&reworded), false).ok());
        let comparison = compare(&[path_only()], std::slice::from_ref(&reworded), true);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: description differs from the snapshot"]
        );

        // A change deeper than the property walk reaches is still caught.
        let sealed = json!({
            "name": "read_text_file",
            "description": "Read a file.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}},
                            "required": ["path"], "additionalProperties": false},
        });
        let comparison = compare(&[path_only()], &[sealed], true);
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: inputSchema differs from the snapshot"]
        );

        // An annotation the snapshot does not carry is named on its own.
        let annotated = json!({
            "name": "read_text_file",
            "description": "Read a file.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}},
                            "required": ["path"]},
            "annotations": {"readOnlyHint": true},
        });
        let comparison = compare(&[path_only()], &[annotated], true);
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: annotations was added"]
        );
    }

    #[test]
    fn strict_promotes_what_compatible_mode_only_notes_and_names_it_once() {
        let relaxed = tool(json!({"path": {"type": "string"}}), json!([]));
        let comparison = compare(&[path_only()], &[relaxed], true);
        assert!(!comparison.ok());
        assert_eq!(
            lines(&comparison),
            ["tool read_text_file: required property path is now optional"],
            "the property walk explained inputSchema, so nothing repeats it"
        );
    }

    #[test]
    fn the_json_report_says_what_failed_and_what_merely_changed() {
        let renamed = tool(json!({"file": {"type": "string"}}), json!(["file"]));
        let added = json!({"name": "list_dir", "inputSchema": {"type": "object"}});
        let comparison = compare(&[path_only()], &[renamed, added], false);
        assert_eq!(
            comparison.to_json(),
            json!({
                "ok": false,
                "differences": [
                    {"tool": "read_text_file", "kind": "property-removed",
                     "detail": "required property path was removed", "level": "fail"},
                    {"tool": "read_text_file", "kind": "now-required",
                     "detail": "property file is new and required", "level": "fail"},
                    {"tool": "list_dir", "kind": "tool-added",
                     "detail": "not in the snapshot", "level": "info"},
                ],
            })
        );
    }

    #[test]
    fn a_snapshot_is_sorted_and_ends_in_a_newline_so_a_diff_is_only_what_changed() {
        let server_info = json!({
            "serverInfo": {"name": "fake-mcp", "version": "1.2.3"},
            "protocolVersion": "2025-06-18",
        });
        let tools = [
            json!({"name": "zip", "inputSchema": {"type": "object"}}),
            json!({"name": "add", "inputSchema": {"type": "object"}}),
        ];
        let document = document(&server_info, &tools);
        assert_eq!(document["server"]["name"], "fake-mcp");
        assert_eq!(document["server"]["version"], "1.2.3");
        assert_eq!(document["protocolVersion"], "2025-06-18");
        assert_eq!(
            document["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(name_of)
                .collect::<Vec<_>>(),
            ["add", "zip"]
        );
        let text = serialize(&document);
        assert!(
            text.ends_with("}\n"),
            "a file a repository commits ends in a newline"
        );

        // Keys are written in sorted order however the server sent them, which
        // is `serde_json` without `preserve_order`: the day that feature is
        // switched on, every committed snapshot starts diffing against itself.
        let mut jumbled = Map::new();
        for key in ["outputSchema", "name", "inputSchema", "description"] {
            jumbled.insert(key.to_string(), json!(key));
        }
        let written = serialize(&Value::Object(jumbled));
        let keys: Vec<&str> = written
            .lines()
            .filter_map(|l| l.trim().strip_prefix('"'))
            .filter_map(|l| l.split('"').next())
            .collect();
        assert_eq!(
            keys,
            ["description", "inputSchema", "name", "outputSchema"],
            "serde_json sorts object keys; preserve_order must stay off"
        );
    }

    #[test]
    fn a_file_that_is_not_a_snapshot_is_a_usage_error_and_never_a_pass() {
        let dir = std::env::temp_dir().join(format!("mcpdial-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("nowhere.json");
        assert!(matches!(read(&missing), Err(Error::Usage(_))));

        let not_json = dir.join("not.json");
        std::fs::write(&not_json, "{").unwrap();
        assert!(matches!(read(&not_json), Err(Error::Usage(_))));

        let wrong_shape = dir.join("wrong.json");
        std::fs::write(&wrong_shape, r#"{"servers": []}"#).unwrap();
        let message = read(&wrong_shape).unwrap_err().to_string();
        assert!(message.contains("no tools array"), "{message}");

        let good = dir.join("tools.json");
        std::fs::write(&good, serialize(&document(&json!({}), &[path_only()]))).unwrap();
        assert_eq!(read(&good).unwrap(), vec![path_only()]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
