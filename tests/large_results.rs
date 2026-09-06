//! `--max-chars` and `--output`: what a caller does with a result too large to
//! print. The interesting half is `--json`, where a cut result has to say so in
//! a way a program can read, and where the text being cut is rarely ASCII.

mod common;

use common::{mcpdial, run, start, temp_home, Mode, PNG_MAGIC};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// The message `echo` gives back, prefixed as the fake server prefixes it.
fn echoed(message: &str) -> String {
    format!("Echo: {message}")
}

fn dial(home: &Path, url: &str) -> Command {
    let mut c = mcpdial(home);
    c.arg("call").arg(url).arg("echo");
    c
}

#[test]
fn max_chars_cuts_the_text_and_says_what_it_left() {
    let s = start(Mode::Stateless);
    let home = temp_home("max-chars");
    let long = "x".repeat(10_000);

    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .args(["--max-chars", "100"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim_end_matches('\n').chars().count(), 100);
    assert!(o.stdout.starts_with("Echo: xxx"), "{}", o.stdout);
    assert_eq!(
        o.stderr.trim(),
        "output truncated: 100 of 10,006 chars shown; \
         use --output FILE or --json for all of it"
    );

    // A result that fits is untouched, and so is one with no bound at all.
    let o = run(dial(&home, &s.url)
        .arg(r#"{"message":"short"}"#)
        .args(["--max-chars", "1000"]));
    assert_eq!(o.stdout.trim(), echoed("short"));
    assert_eq!(o.stderr, "");
    let o = run(dial(&home, &s.url).arg(r#"{"message":"short"}"#));
    assert_eq!(o.stdout.trim(), echoed("short"));
    assert_eq!(o.stderr, "");
}

#[test]
fn a_cut_never_lands_inside_a_character() {
    let s = start(Mode::Stateless);
    let home = temp_home("max-chars-utf8");
    // Every width UTF-8 has, so a bound counted in bytes would halve one of them.
    let message = "é€🌍アb".repeat(40);
    let whole = echoed(&message);

    for limit in [7usize, 8, 9, 10, 11, 12, 13, 199] {
        let o = run(dial(&home, &s.url)
            .arg(format!(r#"{{"message":"{message}"}}"#))
            .args(["--max-chars", &limit.to_string()]));
        assert_eq!(o.code, 0, "{}", o.stderr);
        let shown = o.stdout.trim_end_matches('\n');
        assert_eq!(shown.chars().count(), limit, "at --max-chars {limit}");
        assert!(whole.starts_with(shown), "at --max-chars {limit}");
    }

    // The same text through --output, byte for byte.
    let file = home.join("utf8.txt");
    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{message}"}}"#))
        .args(["--output", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), whole);
    assert_eq!(
        o.stdout.trim(),
        format!(
            "wrote {} chars to {}",
            whole.chars().count(),
            file.display()
        )
    );
}

#[test]
fn a_cut_result_under_json_is_an_object_saying_so() {
    let s = start(Mode::Stateless);
    let home = temp_home("max-chars-json");
    let long = "y".repeat(10_000);

    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .args(["--json", "--max-chars", "120"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).expect("a cut result is still JSON");
    assert_eq!(v["truncated"]["chars"], 120);
    assert!(v["truncated"]["totalChars"].as_u64().unwrap() > 10_000);
    assert!(v["truncated"]["hint"]
        .as_str()
        .unwrap()
        .contains("--output FILE"));
    assert_eq!(v["isError"], false);
    assert_eq!(v["head"].as_str().unwrap().chars().count(), 120);
    assert!(
        v.get("content").is_none(),
        "a result is whole or it is replaced, never half rewritten: {}",
        o.stdout
    );
    assert_eq!(o.stderr, "", "the object says it on stdout instead");

    // Under the bound, the object is exactly what it always was.
    let o = run(dial(&home, &s.url).arg(r#"{"message":"short"}"#).args([
        "--json",
        "--max-chars",
        "4000",
    ]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], echoed("short"));
    assert!(v.get("truncated").is_none());
}

#[test]
fn output_writes_the_whole_result_and_prints_one_line() {
    let s = start(Mode::Stateless);
    let home = temp_home("output");
    let long = "z".repeat(10_000);
    let whole = echoed(&long);

    let file = home.join("result.txt");
    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .args(["-o", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        format!("wrote 10,006 chars to {}", file.display())
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), whole);

    // Under --json the file holds the result object and stdout holds a receipt.
    let json_file = home.join("result.json");
    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .args(["--json", "-o", json_file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(receipt["output"], json_file.display().to_string());
    assert_eq!(receipt["isError"], false);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&json_file).unwrap())
        .expect("the file holds the result object");
    assert_eq!(written["content"][0]["text"], whole);
    assert_eq!(
        receipt["chars"].as_u64().unwrap() as usize,
        std::fs::read_to_string(&json_file).unwrap().chars().count()
    );
}

#[test]
fn a_failed_tool_still_writes_its_file_and_still_exits_one() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-failed");

    let file = home.join("failed.txt");
    let o = run(mcpdial(&home)
        .args(["call", &s.url, "fail", "{}"])
        .args(["-o", file.to_str().unwrap()]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "it failed");
    assert!(o.stdout.contains("wrote 9 chars to"), "{}", o.stdout);
    assert!(
        o.stderr.contains("(tool reported an error)"),
        "{}",
        o.stderr
    );

    let json_file = home.join("failed.json");
    let o = run(mcpdial(&home)
        .args(["--json", "call", &s.url, "fail", "{}"])
        .args(["-o", json_file.to_str().unwrap()]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    let receipt: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(receipt["isError"], true, "the receipt carries the verdict");
    assert!(json_file.exists());
}

#[test]
fn output_takes_a_blob_off_a_terminals_hands_byte_for_byte() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-blob");

    let file = home.join("logo.png");
    let o = run(mcpdial(&home)
        .args(["read", &s.url, "file:///logo.png"])
        .args(["-o", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(std::fs::read(&file).unwrap(), PNG_MAGIC);
    assert_eq!(
        o.stdout.trim(),
        format!("wrote 8 bytes to {}", file.display()),
        "bytes are not characters and are not counted as any"
    );

    // A text resource is characters again, and can be cut like any other text.
    let o = run(mcpdial(&home)
        .args(["read", &s.url, "file:///readme.md"])
        .args(["--max-chars", "9"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "# fake-mc");
    assert!(o.stderr.contains("output truncated: 9 of 21 chars shown"));
}

#[test]
fn a_path_that_could_not_be_written_is_refused_before_anything_is_sent() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-refused");
    let taken = home.join("taken.txt");
    std::fs::write(&taken, "mine").unwrap();
    let before = s.requests.lock().unwrap().len();

    let refusals = [
        (taken.clone(), "already exists"),
        (home.clone(), "is a directory"),
        (home.join("nowhere/deep/result.txt"), "is not a directory"),
    ];
    for (path, said) in refusals {
        let o = run(dial(&home, &s.url)
            .arg(r#"{"message":"hi"}"#)
            .args(["-o", path.to_str().unwrap()]));
        assert_eq!(o.code, 2, "{} {}", path.display(), o.stderr);
        assert!(o.stderr.contains(said), "{}: {}", path.display(), o.stderr);
        assert_eq!(o.stdout, "", "{}", path.display());
    }
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine");
    assert_eq!(
        s.requests.lock().unwrap().len(),
        before,
        "a path mcpdial cannot write is refused before the server is dialed"
    );

    // The same refusal under --json is an error object, not prose.
    let o = run(dial(&home, &s.url).arg(r#"{"message":"hi"}"#).args([
        "--json",
        "-o",
        taken.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 2);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
}

#[test]
fn the_bound_can_come_from_the_environment_and_must_be_a_number() {
    let s = start(Mode::Stateless);
    let home = temp_home("max-chars-env");
    let long = "w".repeat(500);

    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .env("MCPDIAL_MAX_CHARS", "40"));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim_end_matches('\n').chars().count(), 40);

    // The flag beats it, and a value that is not a number is a usage error.
    let o = run(dial(&home, &s.url)
        .arg(format!(r#"{{"message":"{long}"}}"#))
        .env("MCPDIAL_MAX_CHARS", "40")
        .args(["--max-chars", "12"]));
    assert_eq!(o.stdout.trim_end_matches('\n').chars().count(), 12);
    let o = run(dial(&home, &s.url)
        .arg(r#"{"message":"hi"}"#)
        .env("MCPDIAL_MAX_CHARS", "lots"));
    assert_eq!(o.code, 2);
    assert!(
        o.stderr.contains("MCPDIAL_MAX_CHARS must be a number"),
        "{}",
        o.stderr
    );
}

#[test]
fn prompt_and_raw_send_their_results_the_same_way() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-prompt-raw");

    let file = home.join("prompt.txt");
    let o = run(mcpdial(&home)
        .args(["prompt", &s.url, "summarize", r#"{"text":"a memo"}"#])
        .args(["-o", file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("wrote "), "{}", o.stdout);
    assert!(std::fs::read_to_string(&file).unwrap().contains("a memo"));

    let o = run(mcpdial(&home)
        .args(["raw", &s.url, "tools/list"])
        .args(["--max-chars", "60"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["truncated"]["chars"], 60);
    assert!(o.stderr.contains("output truncated: 60 of"), "{}", o.stderr);
}

#[test]
fn a_command_with_no_result_to_send_says_the_flags_do_not_apply() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-elsewhere");

    for flag in [["--max-chars", "10"], ["--output", "x.txt"]] {
        let o = run(mcpdial(&home).args(["tools", &s.url]).args(flag));
        assert_eq!(o.code, 2, "{}", o.stderr);
        assert!(
            o.stderr
                .contains("apply to call, prompt, read, raw and shell"),
            "{}",
            o.stderr
        );
    }
}
