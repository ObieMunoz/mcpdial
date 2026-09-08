//! What to say when a request does not work, and what the caller probably meant.
//!
//! A refusal from a server is rarely the whole story: a tool that does not exist
//! is usually one that was mistyped, a `-32602` is usually an argument the schema
//! would have explained, and a `-32601` is usually a capability the server never
//! advertised. Each of those is a different sentence, and picking the right one is
//! all this file does. Nothing here dials, and nothing here decides an exit code.

use crate::validate::requote_object;
use crate::{args, Failure};
use mcpdial::client::describe_params;
use mcpdial::protocol::{is_not_found, INVALID_PARAMS, METHOD_NOT_FOUND};
use mcpdial::{client, Error};
use serde_json::Value;

pub(crate) const NO_SERVERS: &str =
    "no servers saved yet. Try: mcpdial add wiki --http https://mcp.deepwiki.com/mcp";

/// The hint after an argument object failed to parse on the command line: the
/// same command with its quotes back when the shell removed them, else `lookup`,
/// which says where the expected shape is.
pub(crate) fn json_arg_hint(arguments: &str, command: &str, lookup: String) -> String {
    match requote_object(arguments) {
        Some(fixed) => format!(
            "the shell removed the double quotes; single quotes keep them:\n  {command} {}",
            shell_word(&fixed)
        ),
        None => lookup,
    }
}

/// What a JSON value is, for an error message.
pub(crate) fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Damerau-Levenshtein distance (optimal string alignment), for "did you mean"
/// suggestions: two letters swapped, `ecoh` for `echo`, is one slip of the
/// fingers, so it counts as one edit.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut two_back = vec![0usize; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let substitute = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let mut best = substitute.min(prev[j] + 1).min(cur[j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(two_back[j - 2] + 1);
            }
            cur[j] = best;
        }
        std::mem::swap(&mut two_back, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The nearest candidate to `word`, when one is close enough to be worth naming.
pub(crate) fn closest<'a>(
    word: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    let word = word.to_lowercase();
    let limit = if word.chars().count() <= 4 { 1 } else { 2 };
    candidates
        .map(|c| (edit_distance(&word, &c.to_lowercase()), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

pub(crate) fn find_tool<'a>(tools: &'a [Value], name: &str) -> Option<&'a Value> {
    tools.iter().find(|t| t["name"] == name)
}

pub(crate) fn field_values(items: &[Value], key: &str) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| i[key].as_str())
        .map(String::from)
        .collect()
}

/// The shape a tool expects: a call that would be well-formed, then one line per
/// parameter. `prefix` is whatever comes before the tool name in that call, and
/// `quote` wraps the argument object, which a shell needs quoted and the REPL does not.
pub(crate) fn tool_usage(tool: &Value, prefix: &str, quote: &str) -> String {
    let name = tool["name"].as_str().unwrap_or("?");
    // Whoever is reading this has just had a call refused. What the tool says a
    // call does belongs above the parameters it takes, not under them.
    let hints = client::hint_tags(tool);
    let headline = match hints.is_empty() {
        true => String::new(),
        false => format!("{name}{hints}\n"),
    };
    let mut out = format!(
        "{headline}usage: {prefix} {name} {quote}{}{quote}",
        client::example_arguments(tool)
    );
    // The same call in the other form, so a retry after either one goes wrong
    // needs no second lookup.
    let pairs = args::example_pairs(tool);
    if !pairs.is_empty() {
        out.push_str(&format!("\n   or: {prefix} {name} {pairs}"));
    }
    for p in describe_params(tool) {
        out.push_str("\n  ");
        out.push_str(&p);
    }
    out
}

/// A target written so it survives a copy-paste into a shell.
pub(crate) fn shell_word(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || "\"'$`\\*?~<>|&;()[]{}#!".contains(c)) {
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        s.to_string()
    }
}

/// What to say about a tool name this server does not have.
pub(crate) fn suggest_tool(tools: &[Value], name: &str, tools_cmd: &str) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let names = tools.iter().filter_map(|t| t["name"].as_str());
    Some(match closest(name, names) {
        Some(near) => format!(
            "did you mean {near}? {tools_cmd} lists all {}.",
            tools.len()
        ),
        None => format!(
            "{tools_cmd} lists all {} tools on this server.",
            tools.len()
        ),
    })
}

/// The hint that answers "what did you want?" after a call went wrong: a near
/// miss when the tool does not exist, however the server chose to say so; the
/// tool's own usage when it does and `argument_error` says the arguments were
/// the mistake; nothing when the tool simply failed at its job.
pub(crate) fn call_hint(
    tools: &[Value],
    name: &str,
    argument_error: bool,
    prefix: &str,
    quote: &str,
    tools_cmd: &str,
) -> Option<String> {
    match find_tool(tools, name) {
        Some(t) => argument_error.then(|| tool_usage(t, prefix, quote)),
        None => suggest_tool(tools, name, tools_cmd),
    }
}

/// An answer from the server, as opposed to a line that never got through: only
/// then is it worth a second request to find out what it would have accepted.
pub(crate) fn server_refused(e: &Error) -> bool {
    matches!(e, Error::Rpc { .. })
}

/// A server error that the tool's schema would have prevented. Any other error
/// is the tool's own failure, and printing a schema under it is just noise.
///
/// `-32602` is asked to carry more meanings with every revision - 2026-07-28 gave
/// it a missing resource too - so it only settles the question here because the one
/// caller is a `tools/call` that named a tool the server has. A name it does not
/// have is answered by `call_hint` before this is consulted.
pub(crate) fn is_argument_error(e: &Error) -> bool {
    match e {
        Error::Rpc { code, .. } if *code == INVALID_PARAMS => true,
        Error::Rpc { message, .. } => reads_as_argument_error(message),
        _ => false,
    }
}

/// The same complaint, arriving as text. Not every server raises a JSON-RPC
/// error for a schema violation: chrome-devtools, and anything else built on the
/// TypeScript SDK's tool wrapper, hands back a failed *result* whose content is
/// the `-32602` message. To whoever typed the line it is the same mistake.
pub(crate) fn reads_as_argument_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("-32602") || t.contains("invalid arguments") || t.contains("validation error")
}

/// A server that never implemented `resources/*` or `prompts/*` answers `-32601`,
/// which reads as a mistake in the request. Its `initialize` or `server/discover`
/// result already listed what it does implement, so point there instead of at the
/// bare code.
pub(crate) fn missing_capability(e: Error, capability: &str, info_cmd: &str) -> Failure {
    let never_implemented = matches!(&e, Error::Rpc { code, .. } if *code == METHOD_NOT_FOUND);
    if never_implemented {
        Failure::hinted(
            e,
            format!("this server offers no {capability}; {info_cmd} lists what it does offer."),
        )
    } else {
        e.into()
    }
}

/// One resource or prompt the server does not have, as against a whole capability
/// it never implemented.
///
/// 2026-07-28 answers a URI or a name it does not know with `-32602`, the code every
/// other method spends on arguments it would not take; the revisions before it
/// answered a missing resource with `-32002`, which that one retired. Neither number
/// says so on its own, so the listing does.
pub(crate) fn missing_item(e: Error, capability: &str, list_cmd: &str, info_cmd: &str) -> Failure {
    match &e {
        Error::Rpc { code, .. } if is_not_found(*code) => Failure::hinted(
            e,
            format!("{list_cmd} lists the {capability} this server does have."),
        ),
        _ => missing_capability(e, capability, info_cmd),
    }
}

pub(crate) fn advertises(server_info: &Value, capability: &str) -> bool {
    server_info["capabilities"].get(capability).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::help::SHELL_COMMANDS;
    use mcpdial::protocol::{INVALID_PARAMS, METHOD_NOT_FOUND};
    use mcpdial::Error;

    #[test]
    fn suggestions_stay_close_and_quoting_survives_a_shell() {
        assert_eq!(
            closest("tolls", SHELL_COMMANDS.iter().copied()),
            Some("tools")
        );
        assert_eq!(
            closest("Tools", SHELL_COMMANDS.iter().copied()),
            Some("tools")
        );
        // Far enough away that a guess would be noise.
        assert_eq!(closest("profile", SHELL_COMMANDS.iter().copied()), None);
        assert_eq!(closest("xyz", ["ab"].into_iter()), None);
        // Two letters swapped is one slip, not two, even in a short word.
        assert_eq!(closest("ecoh", ["echo"].into_iter()), Some("echo"));
        assert_eq!(closest("raed", ["read", "raw"].into_iter()), Some("read"));
        assert_eq!(edit_distance("ecoh", "echo"), 1);
        assert_eq!(edit_distance("echo", "echo"), 0);
        assert_eq!(edit_distance("", "echo"), 4);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        // A transposition is one edit; the letters around it still cost their own.
        assert_eq!(edit_distance("ca", "abc"), 3);

        assert_eq!(shell_word("chrome"), "chrome");
        assert_eq!(shell_word("https://x/mcp"), "https://x/mcp");
        assert_eq!(shell_word("stdio:npx -y thing"), "'stdio:npx -y thing'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }

    fn rpc(code: i64, message: &str) -> Error {
        Error::Rpc {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[test]
    fn a_capability_a_server_never_had_is_told_from_one_item_it_lacks() {
        let listing = "`mcpdial resources web`";
        let info = "`mcpdial info web`";

        let none_at_all = missing_capability(
            rpc(METHOD_NOT_FOUND, "Method not found: resources/list"),
            "resources",
            info,
        );
        assert_eq!(
            none_at_all.hint.as_deref(),
            Some("this server offers no resources; `mcpdial info web` lists what it does offer.")
        );

        // The code 2026-07-28 gives a resource that is not there, and the one every
        // revision before it gave: the same answer, so the same hint.
        for code in [INVALID_PARAMS, mcpdial::protocol::RESOURCE_NOT_FOUND_LEGACY] {
            let one_missing = missing_item(
                rpc(code, "Resource not found: file:///nope"),
                "resources",
                listing,
                info,
            );
            assert_eq!(
                one_missing.hint.as_deref(),
                Some("`mcpdial resources web` lists the resources this server does have."),
                "{code}"
            );
        }

        // A method the server never implemented still reads as the capability being
        // absent, whichever way the request was framed.
        let no_capability = missing_item(
            rpc(METHOD_NOT_FOUND, "Method not found: resources/read"),
            "resources",
            listing,
            info,
        );
        assert!(
            no_capability.hint.as_deref().unwrap().contains("offers no"),
            "{:?}",
            no_capability.hint
        );

        // Anything else is the server's own trouble and gets no hint invented for it.
        assert!(
            missing_item(rpc(-32603, "boom"), "resources", listing, info)
                .hint
                .is_none()
        );
    }

    #[test]
    fn argument_errors_are_recognised_in_both_shapes() {
        assert!(is_argument_error(&rpc(INVALID_PARAMS, "bad")));
        // The same complaint arriving as the text of a failed result.
        assert!(reads_as_argument_error(
            "MCP error -32602: Invalid arguments for tool press_key: Required at pageId"
        ));
        assert!(reads_as_argument_error("Input validation error: nope"));
        // A tool that simply failed does not get a schema dumped under it.
        assert!(!reads_as_argument_error("Navigation timed out after 30s"));
        assert!(!is_argument_error(&Error::usage("no")));
    }
}
