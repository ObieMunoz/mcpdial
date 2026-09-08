//! Resources, templates and prompts, and a server that offers none of them.

use crate::common::{echo_command, mcpdial, run, start, temp_home, Mode};
use serde_json::Value;

#[test]
fn resources_and_templates_are_listed_and_paginated() {
    let s = start(Mode::Stateless);
    let home = temp_home("resources");

    let o = run(mcpdial(&home).args(["resources", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 resource(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("file:///readme.md") && o.stdout.contains("file:///logo.png"),
        "both pages: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains("1 template(s):") && o.stdout.contains("file:///notes/{name}.md"),
        "templates are listed apart from the URIs: {}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("Second line"),
        "short listing shows the first line only"
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "resources/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "res-2");

    let o = run(mcpdial(&home).args(["resources", &s.url, "--long"]));
    assert!(
        o.stdout.contains("Second line") && o.stdout.contains("type: text/markdown"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "resources", &s.url]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    let uris: Vec<&str> = v["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert_eq!(uris, ["file:///readme.md", "file:///logo.png"]);
    assert_eq!(
        v["resourceTemplates"][0]["uriTemplate"],
        "file:///notes/{name}.md"
    );
}

#[test]
fn a_resource_reads_as_text_and_a_blob_as_raw_bytes() {
    let s = start(Mode::Stateless);
    let home = temp_home("read");

    let o = run(mcpdial(&home).args(["read", &s.url, "file:///readme.md"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout, "# fake-mcp\nA readme.\n",
        "text arrives unchanged"
    );

    // stdout is a pipe here, so a blob lands as the bytes it stands for.
    let out = mcpdial(&home)
        .args(["read", &s.url, "file:///logo.png"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, crate::common::PNG_MAGIC);

    let o = run(mcpdial(&home).args(["--json", "read", &s.url, "file:///logo.png"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["contents"][0]["blob"], "iVBORw0KGgo=");
    assert_eq!(v["contents"][0]["mimeType"], "image/png");

    let o = run(mcpdial(&home).args(["read", &s.url, "file:///nope"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Resource not found"), "{}", o.stderr);
    assert!(
        !o.stderr.contains("offers no resources"),
        "the capability is there; only the URI was wrong: {}",
        o.stderr
    );
}

#[test]
fn prompts_are_listed_paginated_and_expanded() {
    let s = start(Mode::Stateless);
    let home = temp_home("prompts");

    let o = run(mcpdial(&home).args(["prompts", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("2 prompt(s):"), "{}", o.stdout);
    assert!(
        o.stdout.contains("summarize") && o.stdout.contains("greet"),
        "both pages: {}",
        o.stdout
    );

    let pages: Vec<Value> = s
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.json())
        .filter(|m| m["method"] == "prompts/list")
        .collect();
    assert_eq!(pages.len(), 2, "{pages:?}");
    assert!(pages[0]["params"].get("cursor").is_none());
    assert_eq!(pages[1]["params"]["cursor"], "prompt-2");

    let o = run(mcpdial(&home).args(["prompts", &s.url, "--long"]));
    assert!(
        o.stdout.contains("arguments:") && o.stdout.contains("text (required) - What to summarize"),
        "{}",
        o.stdout
    );

    let o = run(mcpdial(&home).args(["--json", "prompts", &s.url, "--long"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["prompts"].as_array().unwrap().len(), 2);
    assert_eq!(v["prompts"][0]["arguments"][0]["name"], "text");

    let o = run(mcpdial(&home).args(["prompt", &s.url, "summarize", r#"{"text":"a memo"}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "user: Summarize this: a memo\nassistant: Sure.\n");
    assert!(
        o.stderr.contains("Summarize a document."),
        "the description stays off stdout: {}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "prompt", &s.url, "greet"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["messages"][0]["content"]["text"], "Hello.");

    let o = run(mcpdial(&home).args(["prompt", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Prompt nope not found"), "{}", o.stderr);
}

#[test]
fn a_server_without_the_capability_names_it_instead_of_the_code() {
    let home = temp_home("no-capability");
    let target = format!("stdio:{}", echo_command());

    for capability in ["resources", "prompts"] {
        let o = run(mcpdial(&home).args([capability, &target]));
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(
            o.stderr
                .contains(&format!("this server offers no {capability}")),
            "{}",
            o.stderr
        );
    }

    let o = run(mcpdial(&home).args(["prompt", &target, "anything"]));
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("this server offers no prompts"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "read", &target, "file:///x"]));
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["code"], -32601);
    assert!(
        e["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("offers no resources"),
        "{}",
        o.stderr
    );
}
