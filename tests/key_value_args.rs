//! `call` and `prompt` with `key=value` pairs instead of a JSON object: the types
//! the tool's schema asks for, arriving at the server as the schema asked.

mod common;

use common::{echo_command, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// The echo server with its `typed` tool switched on, saved under one name.
fn typed_home(tag: &str) -> std::path::PathBuf {
    let home = temp_home(tag);
    let o = run(mcpdial(&home).args(["add", "echo", "--stdio", &echo_command(), "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    home
}

fn typed(home: &std::path::Path) -> Command {
    let mut c = mcpdial(home);
    c.env("ECHO_SERVER_TYPES", "1");
    c
}

#[test]
fn every_declared_type_is_read_out_of_the_text_that_arrived() {
    let home = typed_home("pairs-types");

    let o = run(typed(&home).args([
        "call",
        "echo",
        "typed",
        "text=hi",
        "count=5",
        "ratio=1.5",
        "flag=true",
        r#"tags:=["a","b"]"#,
        r#"meta:={"k":1}"#,
        "id=123",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let arrived: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(arrived["text"], "hi");
    assert_eq!(arrived["count"], 5);
    assert_eq!(arrived["ratio"], 1.5);
    assert_eq!(arrived["flag"], true);
    assert_eq!(arrived["tags"], serde_json::json!(["a", "b"]));
    assert_eq!(arrived["meta"], serde_json::json!({"k": 1}));
    // A union type stays text under a plain `=`, which is the safe reading.
    assert_eq!(arrived["id"], "123");

    // `:=` overrides the schema in both directions.
    let o = run(typed(&home).args(["call", "echo", "typed", "count:=7", "id:=9"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let arrived: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(arrived["count"], 7);
    assert_eq!(arrived["id"], 9);

    // An empty value is an empty string, and a value may carry its own `=`.
    let o = run(typed(&home).args(["call", "echo", "typed", "text=", "id=a=b"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let arrived: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(arrived["text"], "");
    assert_eq!(arrived["id"], "a=b");
}

#[test]
fn a_value_the_schema_cannot_take_is_refused_before_the_call_goes_out() {
    let home = typed_home("pairs-refused");

    for (pair, said) in [
        ("count=many", "count takes a number"),
        ("flag=yes", "flag takes true or false"),
        ("tags=a,b", r#"tags:='["a","b"]'"#),
        ("meta=k:1", r#"meta:='{"k":1}'"#),
    ] {
        let o = run(typed(&home).args(["call", "echo", "typed", pair]));
        assert_eq!(o.code, 2, "{pair}: {}", o.stderr);
        assert!(o.stdout.is_empty(), "{pair}: {}", o.stdout);
        assert!(o.stderr.contains(said), "{pair}: {}", o.stderr);
        // The usage line rides under it, in both forms.
        assert!(
            o.stderr.contains("usage: mcpdial call echo typed"),
            "{}",
            o.stderr
        );
    }

    // A key the schema shuts out never reaches the server either.
    let o = run(typed(&home).args(["call", "echo", "typed", "cont=1"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains(r#"no argument named "cont"; did you mean "count"?"#),
        "{}",
        o.stderr
    );

    // A `:=` value that is not JSON is a usage error, with nothing dialed.
    let o = run(typed(&home).args(["call", "echo", "typed", "tags:=[a"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("tags:= takes a JSON value"),
        "{}",
        o.stderr
    );

    // With --json the error keeps the shape every other usage error has.
    let o = run(typed(&home).args(["--json", "call", "echo", "typed", "count=many"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("count takes a number"),
        "{}",
        o.stderr
    );
    assert!(
        e["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("mcpdial call echo typed"),
        "{}",
        o.stderr
    );
}

#[test]
fn the_two_argument_forms_cannot_be_mixed() {
    let home = typed_home("pairs-mixed");
    let both = [
        vec!["call", "echo", "typed", r#"{"text":"hi"}"#, "count=1"],
        vec!["call", "echo", "typed", "count=1", r#"{"text":"hi"}"#],
        vec!["call", "echo", "typed", "count=1", "@args.json"],
        vec!["prompt", "echo", "poster", "{}", "tone=dry"],
    ];
    for argv in both {
        let o = run(typed(&home).args(&argv));
        assert_eq!(o.code, 2, "{argv:?}: {}", o.stderr);
        assert!(
            o.stderr.contains("not both at once"),
            "{argv:?}: {}",
            o.stderr
        );
    }
}

#[test]
fn the_schema_is_fetched_only_for_a_pair_that_needs_one() {
    let s = start(Mode::Stateless);
    let home = temp_home("pairs-lookup");
    let listings = |before: usize| {
        s.requests
            .lock()
            .unwrap()
            .iter()
            .skip(before)
            .filter(|r| r.json()["method"] == "tools/list")
            .count()
    };

    // `add` declares two numbers, and answers with their sum: text that was not
    // coerced would arrive as null and sum to zero.
    let o = run(mcpdial(&home).args(["call", &s.url, "add", "a=1", "b=2"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
    assert!(listings(0) > 0, "a coerced pair needs the schema");

    // Every value already JSON: no schema can change one, so none is fetched.
    let before = s.requests.lock().unwrap().len();
    let o = run(mcpdial(&home).args(["call", &s.url, "add", "a:=1", "b:=2"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
    assert_eq!(listings(before), 0, "`:=` alone needs no tool list");
}

#[test]
fn a_prompt_takes_pairs_too_and_every_value_is_a_string() {
    let s = start(Mode::Stateless);
    let home = temp_home("pairs-prompt");

    let o = run(mcpdial(&home).args(["prompt", &s.url, "summarize", "text=a report"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("Summarize this: a report"),
        "{}",
        o.stdout
    );

    // The JSON object form is untouched.
    let o = run(mcpdial(&home).args(["prompt", &s.url, "summarize", r#"{"text":"a report"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("Summarize this: a report"),
        "{}",
        o.stdout
    );
}

#[test]
fn the_shell_reads_pairs_on_a_call_line_and_keeps_their_quotes() {
    let home = temp_home("pairs-shell");
    let target = format!("stdio:{}", echo_command());

    let mut child = mcpdial(&home)
        .env("ECHO_SERVER_TYPES", "1")
        .args(["shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"call echo message=hi\n\
              call typed text=\"two words\" count=3\n\
              call typed count=many\n\
              call echo {\"message\":\"still json\"}\n",
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(lines[0], "Echo: hi", "{stdout}");
    let arrived: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(
        arrived["text"], "two words",
        "the REPL takes the quotes off"
    );
    assert_eq!(arrived["count"], 3);
    assert_eq!(lines[2], "Echo: still json", "{stdout}");
    // The refused pair is a failed command, and says what the tool takes.
    assert!(stderr.contains("count takes a number"), "{stderr}");
    assert!(stderr.contains("usage: call typed"), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "a failed command still exits 1");
}
