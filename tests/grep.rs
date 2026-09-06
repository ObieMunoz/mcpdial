//! `grep`: finding the tool, resource, prompt or instruction that mentions a
//! word, across one saved server or all of them at once.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode, Out};
use serde_json::Value;

fn saved_fake(tag: &str) -> (common::FakeServer, std::path::PathBuf) {
    let server = start(Mode::Stateless);
    let home = temp_home(tag);
    let o = run(mcpdial(&home).args(["add", "web", "--http", &server.url, "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    (server, home)
}

fn json_of(o: &Out) -> Value {
    serde_json::from_str(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", o.stdout))
}

fn matches(o: &Out) -> Vec<Value> {
    json_of(o)["matches"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_word_in_a_description_finds_the_tool_and_names_the_field_it_was_in() {
    let (_s, home) = saved_fake("grep-description");

    let o = run(mcpdial(&home).args(["grep", "numbers", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout, "web\n  tool  add  Add two numbers.\n",
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "grep", "numbers", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found = json_of(&o);
    assert_eq!(
        found["matches"],
        serde_json::json!([{
            "server": "web",
            "kind": "tool",
            "name": "add",
            "description": "Add two numbers.",
            "matched": "description",
        }]),
        "{}",
        o.stdout
    );
    assert_eq!(found["skipped"], serde_json::json!([]), "{}", o.stdout);
}

#[test]
fn nothing_matching_is_exit_1_so_it_composes_with_and() {
    let (_s, home) = saved_fake("grep-nothing");

    let o = run(mcpdial(&home).args(["grep", "calendar", "web"]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(o.stdout, "");
    assert!(
        o.stderr.contains(r#"no match for "calendar""#),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "grep", "calendar", "web"]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(
        json_of(&o)["matches"],
        serde_json::json!([]),
        "{}",
        o.stdout
    );
}

#[test]
fn a_kind_flag_searches_that_kind_alone() {
    let (_s, home) = saved_fake("grep-kinds");

    // `readme` is the name and the URI of a resource, and nothing a tool says.
    let o = run(mcpdial(&home).args(["--json", "grep", "readme", "web"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(matches(&o)[0]["kind"], "resource");
    assert_eq!(matches(&o)[0]["name"], "file:///readme.md");
    assert_eq!(matches(&o)[0]["matched"], "uri");

    let o = run(mcpdial(&home).args(["grep", "readme", "web", "--tools"]));
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(o.stdout, "");

    // The flags combine, and one that is asked for is still searched.
    let o = run(mcpdial(&home).args(["--json", "grep", "readme", "web", "--tools", "--resources"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(matches(&o).len(), 1, "{}", o.stdout);
}

#[test]
fn resources_templates_and_prompts_are_searched_beside_the_tools() {
    let (_s, home) = saved_fake("grep-every-kind");

    let kinds = |pattern: &str| {
        let o = run(mcpdial(&home).args(["--json", "grep", pattern, "web"]));
        assert_eq!(o.code, 0, "{pattern}: {}", o.stderr);
        matches(&o)
            .iter()
            .map(|m| {
                format!(
                    "{}/{}",
                    m["kind"].as_str().unwrap_or("?"),
                    m["matched"].as_str().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(kinds("image/png"), ["resource/mimeType"]);
    assert_eq!(kinds("notes/"), ["template/uriTemplate"]);
    assert_eq!(kinds("Summarize a document"), ["prompt/description"]);
    // `style` is one of `summarize`'s arguments and nothing else on the server.
    assert_eq!(kinds("style"), ["prompt/argument"]);
    // `What to echo` is the `message` parameter's description, not the tool's.
    assert_eq!(kinds("What to echo"), ["tool/parameter"]);
}

#[test]
fn a_servers_own_instructions_are_searched_too() {
    let home = temp_home("grep-instructions");
    let o = run(mcpdial(&home).args([
        "add",
        "echo",
        "--stdio",
        &echo_command(),
        "--env",
        "ECHO_SERVER_TAG=calendar",
        "--no-probe",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);

    let o = run(mcpdial(&home).args(["--json", "grep", "calendar", "echo"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found = matches(&o);
    assert_eq!(found.len(), 1, "{}", o.stdout);
    assert_eq!(found[0]["kind"], "instructions");
    assert_eq!(found[0]["description"], "tag=calendar");
    assert_eq!(found[0]["matched"], "instructions");
    // Instructions are prose, not a listing: there is no name to call next.
    assert!(found[0].get("name").is_none(), "{}", o.stdout);

    let o = run(mcpdial(&home).args(["grep", "calendar", "echo", "--tools"]));
    assert_eq!(o.code, 1, "{}", o.stdout);
}

#[test]
fn one_server_that_cannot_be_reached_does_not_end_the_search() {
    let open = start(Mode::Stateless);
    let locked = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("grep-skipped");
    for (name, url) in [
        ("web", open.url.clone()),
        ("work", locked.url.clone()),
        ("dead", "http://127.0.0.1:1/mcp".to_string()),
    ] {
        let o = run(mcpdial(&home).args(["add", name, "--http", &url, "--no-probe"]));
        assert_eq!(o.code, 0, "{}", o.stderr);
    }

    let o = run(mcpdial(&home).args(["--json", "grep", "numbers"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let found = json_of(&o);
    assert_eq!(
        found["matches"]
            .as_array()
            .map(|m| m.iter().map(|m| m["server"].clone()).collect::<Vec<_>>()),
        Some(vec![Value::from("web")]),
        "{}",
        o.stdout
    );
    let skipped: Vec<(String, String)> = found["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["server"].as_str().unwrap().to_string(),
                s["status"]["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [
            ("dead".to_string(), "unreachable".to_string()),
            ("work".to_string(), "auth_required".to_string()),
        ],
        "{}",
        o.stdout
    );

    // Without --json the same two are one note on stderr, so that an answer
    // from one server is never read as an answer from all of them.
    let o = run(mcpdial(&home).args(["grep", "numbers"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("web\n"), "{}", o.stdout);
    assert!(
        o.stderr.contains("2 server(s) not searched:")
            && o.stderr.contains("dead (unreachable)")
            && o.stderr.contains("work (auth required)"),
        "{}",
        o.stderr
    );
}

#[test]
fn case_regexes_and_a_cap_are_each_asked_for() {
    let (_s, home) = saved_fake("grep-flags");

    assert_eq!(
        run(mcpdial(&home).args(["grep", "SUMMARIZE", "web"])).code,
        1
    );
    let o = run(mcpdial(&home).args(["--json", "grep", "SUMMARIZE", "web", "-i"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(matches(&o)[0]["name"], "summarize");

    // A substring is a substring: the anchors mean nothing until -E.
    assert_eq!(run(mcpdial(&home).args(["grep", "^add$", "web"])).code, 1);
    let o = run(mcpdial(&home).args(["--json", "grep", "^add$", "web", "-E"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(matches(&o)[0]["name"], "add");
    assert_eq!(matches(&o)[0]["matched"], "name");

    let o = run(mcpdial(&home).args(["grep", "a(", "web", "-E"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stderr.contains("is not a regular expression"),
        "{}",
        o.stderr
    );

    let every = matches(&run(mcpdial(&home).args(["--json", "grep", "e", "web"])));
    assert!(every.len() > 2, "{every:?}");
    let capped = matches(&run(
        mcpdial(&home).args(["--json", "grep", "e", "web", "-m", "2"])
    ));
    assert_eq!(capped, every[..2], "{capped:?}");
}

#[test]
fn a_target_that_cannot_be_dialed_is_this_commands_own_failure() {
    let home = temp_home("grep-bad-target");

    let o = run(mcpdial(&home).args(["grep", "anything", "nobody"]));
    assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("unknown server"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["--timeout", "5", "grep", "x", "http://127.0.0.1:1/mcp"]));
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("could not reach"), "{}", o.stderr);
}
