//! The surface a program reads: `--json`, exit codes, `guide`, completions.

use crate::common::{echo_command, mcpdial, run, start, temp_home, Mode};
use crate::oauth::drive_login_out;
use serde_json::{json, Value};
use std::process::Stdio;

#[test]
fn agent_surface_json_errors_file_args_schema_and_guide() {
    let s = start(Mode::Stateless);
    let auth = start(Mode::Auth { tokens: vec![] });
    let home = temp_home("agent");

    // Errors are one JSON object on stderr under --json, with a kind to branch on.
    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "nope"]));
    assert_eq!(o.code, 1);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "rpc");
    assert_eq!(e["error"]["code"], -32602);
    assert!(o.stdout.is_empty());

    let o = run(mcpdial(&home).args(["--json", "info", &auth.url]));
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "http");
    assert_eq!(e["error"]["status"], 401);
    assert!(e["error"]["www_authenticate"]
        .as_str()
        .unwrap()
        .contains("resource_metadata"));

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "echo", "{bad"]));
    assert_eq!(o.code, 2);
    assert_eq!(
        serde_json::from_str::<Value>(o.stderr.trim()).unwrap()["error"]["kind"],
        "usage"
    );

    // Arguments from a file and from stdin.
    let f = home.join("args.json");
    std::fs::write(&f, r#"{"message":"from a file"}"#).unwrap();
    let at = format!("@{}", f.display());
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", &at]));
    assert_eq!(o.stdout.trim(), "Echo: from a file", "{}", o.stderr);
    let mut child = mcpdial(&home)
        .args(["--json", "call", &s.url, "echo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"message":"from stdin"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["content"][0]["text"], "Echo: from stdin");
    let o = run(mcpdial(&home).args(["call", &s.url, "echo", "@/nonexistent.json"]));
    assert_eq!(o.code, 2);

    // One tool's schema, and a helpful miss.
    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["name"], "add");
    assert_eq!(v["inputSchema"]["required"][0], "a");
    // A name this server does not have is the caller's mistake, not the server's:
    // `tools/list` succeeded and nothing was sent for the tool itself.
    let o = run(mcpdial(&home).args(["schema", &s.url, "nah"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("available: echo, add"), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "schema", &s.url, "nah"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "usage", "{}", o.stderr);
    assert!(err["error"].get("code").is_none(), "{}", o.stderr);
    let o = run(mcpdial(&home).args(["--json", "schema", &s.url, "ad"]));
    let err: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(err["error"]["hint"], "did you mean add?", "{}", o.stderr);

    // The guide is embedded in the binary.
    let o = run(mcpdial(&home).args(["guide"]));
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("# mcpdial for agents"));
    assert!(o.stdout.contains("Exit codes"));

    // Shell in JSON mode keeps errors on stdout, in order.
    let mut child = mcpdial(&home)
        .args(["--json", "shell", &s.url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call echo {\"message\":\"one\"}\ncall nope\ncall echo {\"message\":\"two\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines[0]["content"][0]["text"], "Echo: one");
    assert_eq!(lines[1]["error"]["kind"], "rpc");
    assert_eq!(lines[2]["content"][0]["text"], "Echo: two");
    assert_eq!(out.status.code(), Some(1));

    // Under --json every line on either stream is an object: the schema hint
    // under a failed result, a missing credential, and each mutation's receipt.
    let objects = |text: &str| -> Vec<Value> {
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l:?}")))
            .collect()
    };
    let local = format!("stdio:{}", echo_command());
    let o = run(mcpdial(&home).args(["--json", "call", &local, "strict", "{}"]));
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&o.stdout).unwrap()["isError"],
        true
    );
    let err = objects(&o.stderr);
    assert_eq!(err.len(), 1, "{}", o.stderr);
    assert!(
        err[0]["hint"]
            .as_str()
            .unwrap()
            .starts_with("usage: mcpdial call"),
        "{}",
        o.stderr
    );

    let o = run(mcpdial(&home).args(["--json", "token", "show", "nobody"]));
    assert_eq!(o.code, 2);
    assert!(o.stdout.is_empty());
    let err = objects(&o.stderr);
    assert_eq!(err[0]["error"]["kind"], "config");
    assert_eq!(err[0]["error"]["message"], "no credential saved for nobody");

    let o = run(mcpdial(&home).args(["--json", "add", "fake", "--http", &s.url]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let out = objects(&o.stdout);
    assert_eq!(out.len(), 1, "{}", o.stdout);
    assert_eq!(out[0]["saved"]["name"], "fake");
    assert_eq!(out[0]["saved"]["kind"], "http");
    assert_eq!(out[0]["saved"]["location"], s.url);
    assert_eq!(out[0]["saved"]["status"]["state"], "connected");

    let o = run(mcpdial(&home)
        .env("TOK", "t")
        .args(["--json", "token", "set", "fake", "--env", "TOK"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    assert_eq!(objects(&o.stdout), [json!({"saved_credential": "fake"})]);
    let o = run(mcpdial(&home).args(["--json", "token", "rm", "fake"]));
    assert_eq!(objects(&o.stdout), [json!({"removed_credential": "fake"})]);
    let o = run(mcpdial(&home).args(["--json", "logout", "fake"]));
    assert_eq!(objects(&o.stdout), [json!({"removed_credential": null})]);
    assert!(o.stderr.is_empty(), "{}", o.stderr);

    let cfg = home.join("import.json");
    std::fs::write(
        &cfg,
        json!({"mcpServers": {
            "fake": {"type": "http", "url": s.url},
            "other": {"type": "http", "url": s.url},
        }})
        .to_string(),
    )
    .unwrap();
    let o = run(mcpdial(&home).args(["--json", "import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    assert_eq!(
        objects(&o.stdout),
        [json!({"imported": ["other"], "skipped": ["fake"]})]
    );
    let o = run(mcpdial(&home).args(["--json", "rm", "other"]));
    assert_eq!(objects(&o.stdout), [json!({"removed": "other"})]);
    assert!(o.stderr.is_empty(), "{}", o.stderr);

    // login's receipt has the same shape; its progress lines stay prose.
    let (_, stdout) = drive_login_out(&home, &auth.url, &["--json"]);
    let out = objects(&stdout);
    assert_eq!(out.len(), 1, "{stdout}");
    assert_eq!(out[0]["login"]["name"], auth.url);
    assert!(out[0]["login"]["expires_at"].is_u64(), "{stdout}");
    assert_eq!(out[0]["login"]["refreshable"], true);

    // In the shell, info is one line like every other command, and the hint
    // under a failed result is an object on stderr.
    let mut child = mcpdial(&home)
        .args(["--json", "shell", &local])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"info\ncall strict {}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines = objects(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines.len(), 2, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(lines[0]["serverInfo"]["name"], "echo-server");
    assert_eq!(lines[1]["isError"], true);
    let err = objects(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(err.len(), 1, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(err[0]["hint"]
        .as_str()
        .unwrap()
        .starts_with("usage: call strict"));
}

#[test]
fn completion_scripts_for_every_shell() {
    let home = temp_home("completions");

    for (shell, registration_line) in [
        ("bash", "complete -F _mcpdial"),
        ("zsh", "#compdef mcpdial"),
        ("fish", "complete -c mcpdial"),
        ("elvish", "edit:completion:arg-completer[mcpdial]"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let o = run(mcpdial(&home).args(["completions", shell]));
        assert_eq!(o.code, 0, "{shell}: {}", o.stderr);
        assert!(!o.stdout.is_empty(), "{shell}: empty script");
        assert!(
            o.stdout.contains(registration_line),
            "{shell}: no {registration_line:?}"
        );
        assert!(o.stdout.contains("token-env"), "{shell}: no global flags");
        assert!(
            o.stdout.contains("no-probe"),
            "{shell}: no per-command flags"
        );
    }

    let o = run(mcpdial(&home).args(["completions", "csh"]));
    assert_eq!(o.code, 2);
    assert!(o.stdout.is_empty());

    // Kept out of the command list, but named once among the examples, so
    // that `--help` alone is enough to find it.
    let o = run(mcpdial(&home).args(["--help"]));
    assert_eq!(o.code, 0);
    let (before_examples, examples) = o.stdout.split_once("examples:").unwrap();
    assert!(
        !before_examples.contains("completions"),
        "{before_examples}"
    );
    assert!(examples.contains("mcpdial completions SHELL"), "{examples}");
}

/// `completions` builds the command tree a second time, under the tree clap
/// already built to parse the arguments, so it is the deepest stack the binary
/// ever reaches. clap_derive expands that tree into one function, and
/// unoptimized its frame is most of the 1 MiB Windows reserves for a main
/// thread, so the command tree growing by a few arguments is enough to overflow
/// it there and nowhere else. `.cargo/config.toml` links Windows binaries with
/// the 8 MiB Unix reserves instead; this is the check that it took.
#[test]
#[cfg(windows)]
fn the_windows_binary_reserves_a_unix_sized_stack() {
    const WANTED: u64 = 8 * 1024 * 1024;
    const PE32_PLUS: u16 = 0x20b;
    const COFF_HEADER_LEN: usize = 24;
    const STACK_RESERVE_IN_OPTIONAL_HEADER: usize = 0x48;

    let image = std::fs::read(env!("CARGO_BIN_EXE_mcpdial")).unwrap();
    let at_u16 = |at: usize| u16::from_le_bytes(image[at..at + 2].try_into().unwrap());
    let at_u32 = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    let at_u64 = |at: usize| u64::from_le_bytes(image[at..at + 8].try_into().unwrap());

    let pe_header = at_u32(0x3c) as usize;
    assert!(
        image[pe_header..pe_header + 4] == *b"PE\0\0",
        "not a PE image"
    );
    let optional_header = pe_header + COFF_HEADER_LEN;
    assert_eq!(
        at_u16(optional_header),
        PE32_PLUS,
        "the stack reserve sits at another offset in a 32-bit image"
    );

    let reserved = at_u64(optional_header + STACK_RESERVE_IN_OPTIONAL_HEADER);
    assert!(
        reserved >= WANTED,
        "the binary reserves {reserved} bytes of stack, short of the {WANTED} \
         .cargo/config.toml asks for; a debug build overflows a 1 MiB stack \
         before it has finished parsing an argument"
    );
}

#[test]
fn a_structured_only_result_still_prints() {
    let s = start(Mode::Stateless);
    let home = temp_home("structured");

    let o = run(mcpdial(&home).args(["call", &s.url, "reading"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["celsius"], 20, "{}", o.stdout);

    let o = run(mcpdial(&home).args(["--json", "call", &s.url, "reading"]));
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["structuredContent"]["celsius"], 20);
}

#[test]
fn content_blocks_win_over_structured_content() {
    let s = start(Mode::Stateless);
    let home = temp_home("both-shapes");

    let o = run(mcpdial(&home).args(["call", &s.url, "add", r#"{"a":1,"b":2}"#]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "The sum of 1 and 2 is 3.");
}

#[test]
fn schema_shows_an_output_schema_and_names_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("output-schema");

    let o = run(mcpdial(&home).args(["schema", &s.url, "add"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["outputSchema"]["required"][0], "sum");
    assert!(o.stderr.contains("structuredContent"), "{}", o.stderr);

    let o = run(mcpdial(&home).args(["schema", &s.url, "echo"]));
    assert!(o.stderr.is_empty(), "{}", o.stderr);
}
