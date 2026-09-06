//! `grep`: one pass across every saved server for the thing that mentions a word.
//!
//! An agent with ten saved servers should not read ten `tools` listings and
//! fifty schemas to find the one that creates a calendar event. This dials the
//! servers at once, runs a pattern over the names and the prose of everything
//! they offer, and prints one line per hit; `schema TARGET TOOL` then fetches
//! the chosen tool in full. It is the step between the two, and the reason a
//! listing may stay as short as [`crate::brief`] makes it.
//!
//! What is searched is what `initialize` said the server has: the `tools`,
//! `resources` and `prompts` capabilities decide which listings are asked for,
//! so a resources-only server costs one request and a `-32601` never turns a
//! healthy server into a failed one.
//!
//! Dialing several servers is several ways to be let down, and no one of them
//! may end the search: a server that will not answer, or wants a token nobody
//! saved, is reported beside the matches under `skipped` and the rest are
//! searched anyway. Each dial is bounded by the ten seconds a status probe
//! waits rather than the minute a call gets, for the reason `ls` is: one server
//! that is down must not hold the whole search.

use crate::present::{truncate_at, Presenter};
use crate::{advertises, Failure, EXIT_ERROR, NO_SERVERS};
use mcpdial::client::{self, Options, Status, PROBE_TIMEOUT};
use mcpdial::config::ServerConfig;
use mcpdial::{Error, Store};
use regex_lite::Regex;
use serde::{Serialize, Serializer};
use serde_json::Value;

/// How wide the column naming each match grows before a long URI is left to
/// push its own line over rather than every other line with it.
const NAME_COLUMN: usize = 44;

/// How much of a description one line shows, the width `resources` uses.
const DESCRIPTION: usize = 74;

#[derive(clap::Args)]
pub struct Flags {
    /// What to look for: a substring, or a regular expression with -E
    pattern: String,
    /// A saved name, an http(s):// URL, or stdio:<command>; every saved server when omitted
    target: Option<String>,
    /// Search tool names, titles, descriptions and parameters
    #[arg(long)]
    tools: bool,
    /// Search resource URIs, templates, names, descriptions and mime types
    #[arg(long)]
    resources: bool,
    /// Search prompt names, descriptions and argument names
    #[arg(long)]
    prompts: bool,
    /// Search the instructions the server sent at startup
    #[arg(long)]
    instructions: bool,
    /// Match regardless of case
    #[arg(short = 'i', long)]
    ignore_case: bool,
    /// Read PATTERN as a regular expression instead of a substring
    #[arg(short = 'E', long)]
    regex: bool,
    /// Report at most N matches
    #[arg(short = 'm', long, value_name = "N")]
    max_count: Option<usize>,
}

pub fn run(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    json: bool,
    flags: Flags,
) -> Result<u8, Failure> {
    let matcher = Matcher::new(&flags.pattern, flags.regex, flags.ignore_case)?;
    let wanted = Wanted::of(&flags);
    let readings = match &flags.target {
        // A target named on the command line that will not answer is this
        // command's own failure, as it is for `tools TARGET`: nothing else was
        // asked for, so there is nothing to carry on with.
        Some(target) => {
            let resolved = client::resolve(store, target)?;
            vec![Reading {
                server: resolved.name.clone(),
                offerings: Ok(offerings(store, &resolved, opts, &wanted)?),
            }]
        }
        None => every_server(store, opts, &wanted)?,
    };

    let mut matches: Vec<Match> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    for reading in &readings {
        match &reading.offerings {
            Ok(offerings) => matches.extend(searched(&reading.server, offerings, &matcher)),
            Err(status) => skipped.push(Skipped {
                server: reading.server.clone(),
                status: status.clone(),
            }),
        }
    }
    if let Some(most) = flags.max_count {
        matches.truncate(most);
    }

    if json {
        crate::print_json(
            ui,
            &Found {
                matches: &matches,
                skipped: &skipped,
            },
        );
    } else {
        if !skipped.is_empty() {
            ui.note(&not_searched(&skipped));
        }
        if matches.is_empty() {
            ui.err_line(&nothing_found(
                &flags.pattern,
                readings.len() - skipped.len(),
            ));
        } else {
            ui.paged(|| print(ui, &matches));
        }
    }
    Ok(if matches.is_empty() { EXIT_ERROR } else { 0 })
}

/// What a match is, in the words the listing commands use for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tool,
    Resource,
    Template,
    Prompt,
    Instructions,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Tool => "tool",
            Kind::Resource => "resource",
            Kind::Template => "template",
            Kind::Prompt => "prompt",
            Kind::Instructions => "instructions",
        }
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(self.label())
    }
}

/// One line of the answer.
#[derive(Serialize)]
struct Match {
    server: String,
    kind: Kind,
    /// What the command that comes next names: a tool or prompt name, a
    /// resource's URI. A server's instructions have no name of their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    description: String,
    /// Which field held the pattern, under the server's own name for it.
    matched: &'static str,
}

/// A server the search could not read, and why, in the words `ls` uses.
#[derive(Serialize)]
struct Skipped {
    server: String,
    status: Status,
}

#[derive(Serialize)]
struct Found<'a> {
    matches: &'a [Match],
    skipped: &'a [Skipped],
}

/// Which listings a search has to fetch. No flag means all of them, and the
/// flags combine, so `--tools --prompts` fetches two.
struct Wanted {
    tools: bool,
    resources: bool,
    prompts: bool,
    instructions: bool,
}

impl Wanted {
    fn of(flags: &Flags) -> Self {
        let everything = !(flags.tools || flags.resources || flags.prompts || flags.instructions);
        Self {
            tools: flags.tools || everything,
            resources: flags.resources || everything,
            prompts: flags.prompts || everything,
            instructions: flags.instructions || everything,
        }
    }
}

/// The pattern, as `-E` and `-i` leave it.
enum Matcher {
    Substring(String),
    /// `-i` without `-E`: the needle folded once, and each field folded as it
    /// is read, since neither has a case the other has to respect.
    Folded(String),
    Regex(Regex),
}

impl Matcher {
    fn new(pattern: &str, regex: bool, ignore_case: bool) -> Result<Self, Error> {
        if !regex {
            return Ok(match ignore_case {
                true => Matcher::Folded(pattern.to_lowercase()),
                false => Matcher::Substring(pattern.to_string()),
            });
        }
        let source = match ignore_case {
            true => format!("(?i){pattern}"),
            false => pattern.to_string(),
        };
        Regex::new(&source)
            .map(Matcher::Regex)
            .map_err(|e| Error::usage(format!("{pattern:?} is not a regular expression: {e}")))
    }

    fn matches(&self, text: &str) -> bool {
        match self {
            Matcher::Substring(needle) => text.contains(needle.as_str()),
            Matcher::Folded(needle) => text.to_lowercase().contains(needle.as_str()),
            Matcher::Regex(re) => re.is_match(text),
        }
    }
}

/// Everything one server offers that the search asked for.
#[derive(Default)]
struct Offerings {
    instructions: Option<String>,
    tools: Vec<Value>,
    resources: Vec<Value>,
    templates: Vec<Value>,
    prompts: Vec<Value>,
}

/// What one server contributed: what it offers, or why it could not say.
struct Reading {
    server: String,
    offerings: Result<Offerings, Status>,
}

/// One dial, and every listing `wanted` asked for that the server says it has.
/// Resource templates are optional even where resources are not, so a server
/// with none of them is not a server that failed.
fn offerings(
    store: &Store,
    resolved: &client::Resolved,
    opts: &Options,
    wanted: &Wanted,
) -> Result<Offerings, Error> {
    let mut conn = client::connect(store, resolved, opts)?;
    let mut found = Offerings {
        instructions: wanted
            .instructions
            .then(|| {
                conn.server_info["instructions"]
                    .as_str()
                    .map(str::to_string)
            })
            .flatten(),
        ..Offerings::default()
    };
    if wanted.tools && advertises(&conn.server_info, "tools") {
        found.tools = conn.list_tools()?;
    }
    if wanted.resources && advertises(&conn.server_info, "resources") {
        found.resources = conn.session.list_resources()?;
        found.templates = conn.session.list_resource_templates().unwrap_or_default();
    }
    if wanted.prompts && advertises(&conn.server_info, "prompts") {
        found.prompts = conn.session.list_prompts()?;
    }
    Ok(found)
}

/// Every saved server, dialed at once and reported in the store's order, so
/// that the answer does not depend on which server replied first.
fn every_server(store: &Store, opts: &Options, wanted: &Wanted) -> Result<Vec<Reading>, Error> {
    let servers: Vec<(String, ServerConfig)> = store.servers()?.into_iter().collect();
    // Ten servers tracing at once is noise rather than a trace, and a survey
    // waits what `ls` waits rather than what one deliberate call gets.
    let surveying = Options {
        verbose: false,
        fallback_timeout: PROBE_TIMEOUT,
        ..opts.clone()
    };
    let mut readings: Vec<Option<Reading>> = (0..servers.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        for (slot, (name, config)) in readings.iter_mut().zip(servers) {
            let opts = &surveying;
            scope.spawn(move || {
                let resolved = client::Resolved {
                    name,
                    config,
                    saved: true,
                };
                *slot = Some(Reading {
                    server: resolved.name.clone(),
                    offerings: offerings(store, &resolved, opts, wanted)
                        .map_err(|e| client::status_of(store, &resolved, opts, &e)),
                });
            });
        }
    });
    Ok(readings.into_iter().flatten().collect())
}

/// Every match one server's offerings hold, in listing order: tools, then
/// resources and the templates beside them, then prompts, then the server's
/// own instructions.
fn searched(server: &str, offerings: &Offerings, matcher: &Matcher) -> Vec<Match> {
    let mut found = Vec::new();
    let mut hit = |kind, name: Option<&str>, description: &str, matched| {
        found.push(Match {
            server: server.to_string(),
            kind,
            name: name.map(str::to_string),
            description: description.to_string(),
            matched,
        });
    };
    for tool in &offerings.tools {
        if let Some(matched) = in_tool(tool, matcher) {
            let name = field(tool, "name");
            hit(
                Kind::Tool,
                Some(name),
                first_line(field(tool, "description")),
                matched,
            );
        }
    }
    for (kind, listing) in [
        (Kind::Resource, &offerings.resources),
        (Kind::Template, &offerings.templates),
    ] {
        for resource in listing {
            if let Some(matched) = in_resource(resource, matcher) {
                hit(kind, Some(address(resource)), summary(resource), matched);
            }
        }
    }
    for prompt in &offerings.prompts {
        if let Some(matched) = in_prompt(prompt, matcher) {
            let name = field(prompt, "name");
            hit(
                Kind::Prompt,
                Some(name),
                first_line(field(prompt, "description")),
                matched,
            );
        }
    }
    // Instructions are prose rather than a listing, so they are read the way
    // grep reads a file: the line the pattern is on is the line reported.
    if let Some(instructions) = &offerings.instructions {
        if let Some(line) = instructions.lines().find(|l| matcher.matches(l)) {
            hit(Kind::Instructions, None, line.trim(), "instructions");
        }
    }
    found
}

/// A tool matches on what it is called, on what it says it does, or on the
/// names and descriptions of the parameters its `inputSchema` declares.
fn in_tool(tool: &Value, matcher: &Matcher) -> Option<&'static str> {
    if let Some(matched) = first_field(tool, &["name", "title", "description"], matcher) {
        return Some(matched);
    }
    let properties = tool["inputSchema"]["properties"].as_object()?;
    properties
        .iter()
        .any(|(name, spec)| matcher.matches(name) || matcher.matches(field(spec, "description")))
        .then_some("parameter")
}

/// A resource matches on what addresses it, what it is called, what it says it
/// holds or the type of what it holds. A template is addressed by a pattern
/// for a URI rather than by one, and otherwise reads the same.
fn in_resource(resource: &Value, matcher: &Matcher) -> Option<&'static str> {
    first_field(
        resource,
        &["uri", "uriTemplate", "name", "description", "mimeType"],
        matcher,
    )
}

/// A prompt matches on its name, on what it says it renders, or on the names
/// of the arguments it takes, which are names and prose with no schema behind
/// them.
fn in_prompt(prompt: &Value, matcher: &Matcher) -> Option<&'static str> {
    if let Some(matched) = first_field(prompt, &["name", "description"], matcher) {
        return Some(matched);
    }
    let arguments = prompt["arguments"].as_array()?;
    arguments
        .iter()
        .any(|a| matcher.matches(field(a, "name")))
        .then_some("argument")
}

/// The first of `fields` the pattern is in, which is the one reported: a
/// listing is searched in the order a reader would read it.
fn first_field(item: &Value, fields: &[&'static str], matcher: &Matcher) -> Option<&'static str> {
    fields
        .iter()
        .copied()
        .find(|name| matcher.matches(field(item, name)))
}

fn field<'a>(item: &'a Value, name: &str) -> &'a str {
    item[name].as_str().unwrap_or("")
}

/// What names a resource in a command: its URI, or the pattern for one.
fn address(resource: &Value) -> &str {
    resource["uri"]
        .as_str()
        .or_else(|| resource["uriTemplate"].as_str())
        .unwrap_or("")
}

/// A resource with no description at all still has a name to show, which is
/// what `resources` falls back to.
fn summary(resource: &Value) -> &str {
    match first_line(field(resource, "description")) {
        "" => field(resource, "name"),
        line => line,
    }
}

fn first_line(description: &str) -> &str {
    description.trim().lines().next().unwrap_or("").trim_end()
}

/// One block per server, one line per match: what it is, what the next command
/// names, and the line of prose the choice is made on.
fn print(ui: &dyn Presenter, matches: &[Match]) {
    let widest = |pick: fn(&Match) -> &str| {
        matches
            .iter()
            .map(|m| pick(m).chars().count())
            .max()
            .unwrap_or(0)
    };
    let kinds = widest(|m| m.kind.label());
    let names = widest(|m| m.name.as_deref().unwrap_or("")).min(NAME_COLUMN);
    let mut listed: Option<&str> = None;
    for m in matches {
        if listed != Some(m.server.as_str()) {
            ui.line(&m.server);
            listed = Some(&m.server);
        }
        let description = truncate_at(&m.description, DESCRIPTION);
        // Nothing matched here has a name only when nothing matched anywhere
        // has one, so the column goes rather than standing empty on every line.
        let line = match names {
            0 => format!("  {:<kinds$}  {description}", m.kind.label()),
            _ => format!(
                "  {:<kinds$}  {:<names$}  {description}",
                m.kind.label(),
                m.name.as_deref().unwrap_or("")
            ),
        };
        ui.line(line.trim_end());
    }
}

/// The servers that could not be read, in one line, so that an empty answer is
/// never mistaken for a complete one.
fn not_searched(skipped: &[Skipped]) -> String {
    let named: Vec<String> = skipped
        .iter()
        .map(|s| format!("{} ({})", s.server, s.status.label()))
        .collect();
    format!(
        "{} server(s) not searched: {}",
        skipped.len(),
        named.join(", ")
    )
}

fn nothing_found(pattern: &str, searched: usize) -> String {
    match searched {
        0 => NO_SERVERS.to_string(),
        n => format!("no match for {pattern:?} in {n} server(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn substring(pattern: &str) -> Matcher {
        Matcher::new(pattern, false, false).unwrap()
    }

    fn echo() -> Value {
        json!({
            "name": "echo",
            "title": "Echo",
            "description": "Echo a message back.\nSecond line.",
            "inputSchema": {"type": "object", "properties": {
                "message": {"type": "string", "description": "What to echo"},
                "repeats": {"type": "number"}}},
        })
    }

    #[test]
    fn a_tool_reports_the_first_field_the_pattern_is_in() {
        assert_eq!(in_tool(&echo(), &substring("echo")), Some("name"));
        assert_eq!(in_tool(&echo(), &substring("Echo")), Some("title"));
        assert_eq!(
            in_tool(&echo(), &substring("message back")),
            Some("description")
        );
        assert_eq!(
            in_tool(&echo(), &substring("Second line")),
            Some("description")
        );
        assert_eq!(in_tool(&echo(), &substring("What to")), Some("parameter"));
        assert_eq!(in_tool(&echo(), &substring("repeats")), Some("parameter"));
        // A word the description carries too is reported as the description,
        // which is the first field a reader would have found it in.
        assert_eq!(in_tool(&echo(), &substring("message")), Some("description"));
        assert_eq!(in_tool(&echo(), &substring("calendar")), None);
        assert_eq!(in_tool(&json!({"name": "x"}), &substring("y")), None);
    }

    #[test]
    fn a_resource_matches_on_what_addresses_it_and_a_prompt_on_what_it_takes() {
        let readme = json!({"uri": "file:///readme.md", "name": "readme",
            "description": "The project readme.", "mimeType": "text/markdown"});
        assert_eq!(in_resource(&readme, &substring("file:///")), Some("uri"));
        assert_eq!(
            in_resource(&readme, &substring("project")),
            Some("description")
        );
        assert_eq!(
            in_resource(&readme, &substring("markdown")),
            Some("mimeType")
        );
        let template = json!({"uriTemplate": "file:///notes/{name}.md", "name": "note"});
        assert_eq!(
            in_resource(&template, &substring("notes/")),
            Some("uriTemplate")
        );
        assert_eq!(address(&template), "file:///notes/{name}.md");
        assert_eq!(summary(&template), "note");

        let summarize = json!({"name": "summarize", "description": "Summarize a document.",
            "arguments": [{"name": "style", "description": "terse or thorough"}]});
        assert_eq!(
            in_prompt(&summarize, &substring("document")),
            Some("description")
        );
        assert_eq!(in_prompt(&summarize, &substring("style")), Some("argument"));
        assert_eq!(in_prompt(&summarize, &substring("thorough")), None);
    }

    #[test]
    fn case_and_regexes_are_asked_for_rather_than_guessed() {
        assert!(!substring("ECHO").matches("echo a message"));
        assert!(Matcher::new("ECHO", false, true)
            .unwrap()
            .matches("echo a message"));
        assert!(Matcher::new("^echo$", true, false).unwrap().matches("echo"));
        assert!(!Matcher::new("^echo$", true, false)
            .unwrap()
            .matches("echoes"));
        assert!(Matcher::new("^ECHO$", true, true).unwrap().matches("echo"));
        // A regex is only a regex when it was asked to be one.
        assert!(!substring("^echo$").matches("echo"));
        match Matcher::new("a(", true, false) {
            Err(Error::Usage(message)) => {
                assert!(message.contains("is not a regular expression"), "{message}");
            }
            _ => panic!("a broken pattern is a usage error, and names itself"),
        }
    }

    #[test]
    fn instructions_are_read_a_line_at_a_time_and_report_the_line() {
        let offerings = Offerings {
            instructions: Some("Call list_events first.\nThen create_event.".into()),
            ..Offerings::default()
        };
        let found = searched("work", &offerings, &substring("create_event"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind.label(), "instructions");
        assert_eq!(found[0].name, None);
        assert_eq!(found[0].description, "Then create_event.");
        assert_eq!(found[0].matched, "instructions");
        assert!(searched("work", &offerings, &substring("nowhere")).is_empty());
    }

    #[test]
    fn a_kind_flag_narrows_what_is_fetched_and_no_flag_fetches_everything() {
        let flags = |args: &[&str]| {
            use clap::Parser;
            #[derive(Parser)]
            struct Only {
                #[command(flatten)]
                grep: Flags,
            }
            Only::parse_from(std::iter::once("grep").chain(args.iter().copied())).grep
        };
        let all = Wanted::of(&flags(&["pattern"]));
        assert!(all.tools && all.resources && all.prompts && all.instructions);
        let some = Wanted::of(&flags(&["pattern", "--tools", "--prompts"]));
        assert!(some.tools && some.prompts);
        assert!(!some.resources && !some.instructions);
    }
}
