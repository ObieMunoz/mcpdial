//! The agent contract: what a program gets from mcpdial, byte for byte.
//!
//! Every command in the README's command list is run against the fake HTTP
//! server and `examples/echo_server.rs`, once under a pipe and once under
//! `--json`, and its exit code, stdout and stderr are compared with the file
//! of the same name under `tests/snapshots/`. Ports, temp paths, pids and
//! token expiries are normalised first, so the files are the same on every
//! machine and every OS.
//!
//! A snapshot that no longer matches fails the test. That is the point: the
//! `Plain` presenter is frozen, and a change to what an agent sees has to be a
//! deliberate act. To make one, change the code, then regenerate the files and
//! read the diff before committing it:
//!
//!     MCPDIAL_UPDATE_SNAPSHOTS=1 cargo test --test contract
//!
//! and say in the pull request that the contract changed, and why; its title
//! is what lands in the changelog. A PR about the terminal presentation that
//! needs this is wrong by definition.
//!
//! Two commands are held more loosely, because their stdout is generated from
//! something else and would put every change to that thing in a snapshot:
//! `guide` is compared with `docs/AGENTS.md` directly, and `completions` (a
//! script clap generates from the argument parser, which every new flag
//! changes) is checked for the registration line alone. Clap's own usage
//! errors are left out for the same reason, and `serve` because it is a
//! server: it runs until it is killed, and what it prints comes from the
//! library rather than the presenter.

mod common;

use common::{
    catalog_entries, echo_command, echo_server, mcpdial, start, FakeServer, Mode, CONFIDENTIAL_ID,
    CONFIDENTIAL_SECRET,
};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const UPDATE_ENV: &str = "MCPDIAL_UPDATE_SNAPSHOTS";

/// One command of the contract: how it is run under a pipe, and how under
/// `--json`. The two differ only where a command changes state, so that the
/// second run still has something to do.
struct Case {
    name: &'static str,
    plain: Vec<String>,
    json: Vec<String>,
    stdin: Option<String>,
    env: Vec<(String, String)>,
    /// Which of the two runs to make; a daemon's `start` can only be run
    /// once per `stop`, so those two are split into a plain and a json case.
    piped: bool,
    with_json: bool,
}

impl Case {
    fn both(name: &'static str, args: &[&str]) -> Self {
        Self::each(name, args, args)
    }

    fn each(name: &'static str, plain: &[&str], json: &[&str]) -> Self {
        Self {
            name,
            plain: plain.iter().map(|a| a.to_string()).collect(),
            json: json.iter().map(|a| a.to_string()).collect(),
            stdin: None,
            env: Vec::new(),
            piped: true,
            with_json: true,
        }
    }

    fn only_piped(mut self) -> Self {
        self.with_json = false;
        self
    }

    fn only_json(mut self) -> Self {
        self.piped = false;
        self
    }

    /// The `--json` run of a command that saves servers overwrites the plain
    /// run's, so the receipt is the same.
    fn with_json_force(mut self) -> Self {
        self.json.push("--force".to_string());
        self
    }

    fn stdin(mut self, text: &str) -> Self {
        self.stdin = Some(text.to_string());
        self
    }

    fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }
}

/// Everything a run can print that differs from one machine to the next, and
/// what it is replaced with.
struct Names {
    home: PathBuf,
    echo: PathBuf,
    bases: Vec<(String, String)>,
}

impl Names {
    fn normalise(&self, text: &str) -> String {
        let mut out = text.replace("\r\n", "\n");
        for (base, name) in &self.bases {
            out = out.replace(base, name);
        }
        out = replace_path(&out, &self.home, "HOME");
        out = replace_path(&out, &self.echo, "ECHO_SERVER");
        // A Windows path is single-quoted wherever it goes through word
        // splitting, for its backslashes; a unix one only where a test quoted it.
        out = out.replace("'ECHO_SERVER'", "ECHO_SERVER");
        out = replace_number(&out, "pid ", "PID");
        out = replace_number(&out, "\"pid\":", "PID");
        out = replace_number(&out, "\"expires_at\":", "TIME");
        out
    }
}

/// `path` as printed, and as JSON escapes it, both become `name`; a separator
/// that follows either becomes `/`, so a Windows path reads as the unix one.
fn replace_path(text: &str, path: &Path, name: &str) -> String {
    let shown = path.display().to_string();
    let escaped = json!(shown).to_string();
    let escaped = &escaped[1..escaped.len() - 1];
    let mut out = text.replace(&shown, name);
    if escaped != shown {
        out = out.replace(escaped, name);
    }
    let mut fixed = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find(name) {
        let after = at + name.len();
        fixed.push_str(&rest[..after]);
        rest = &rest[after..];
        // The rest of the path: separators become `/`, up to the first thing
        // that cannot be part of a file name we chose.
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ')' | ']'))
            .unwrap_or(rest.len());
        fixed.push_str(&rest[..end].replace("\\\\", "/").replace('\\', "/"));
        rest = &rest[end..];
    }
    fixed.push_str(rest);
    fixed
}

/// The digits after every `prefix` (and the space pretty-printed JSON puts
/// there) become `name`.
fn replace_number(text: &str, prefix: &str, name: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let after = at + prefix.len();
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        out.push_str(&rest[..spaces]);
        rest = &rest[spaces..];
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits > 0 {
            out.push_str(name);
        }
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

/// The whole of a run as one text, which is what a snapshot file holds.
fn render(command: &str, code: i32, stdout: &str, stderr: &str) -> String {
    format!("$ {command}\nexit {code}\n--- stdout\n{stdout}--- stderr\n{stderr}")
}

/// A config directory short enough that `run/NAME.sock` fits a Unix socket
/// path, which macOS caps at 104 bytes. Two `cargo test` invocations never
/// share a pid, so that and the test's own tag keep them apart.
fn contract_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("md-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Contract {
    home: PathBuf,
    names: Names,
    fixture: PathBuf,
    base: String,
}

impl Contract {
    fn new(servers: &[&FakeServer]) -> Self {
        let home = contract_home("contract");
        let fixture = home.join("catalog-fixture.json");
        std::fs::write(&fixture, catalog_entries(&servers[0].base).to_string()).unwrap();
        let bases = servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let name = if i == 0 {
                    "http://127.0.0.1:PORT".to_string()
                } else {
                    format!("http://127.0.0.1:PORT{}", i + 1)
                };
                (s.base.clone(), name)
            })
            .collect();
        Self {
            names: Names {
                home: home.clone(),
                echo: echo_server(),
                bases,
            },
            base: servers[0].base.clone(),
            home,
            fixture,
        }
    }

    fn command(&self, args: &[String], case: &Case) -> Command {
        let mut cmd = mcpdial(&self.home);
        cmd.args(args)
            .env("MCPDIAL_CATALOG", &self.fixture)
            .env("MCPDIAL_REGISTRY", &self.base)
            .env_remove("MCPDIAL_PLAIN")
            .env_remove("MCPDIAL_NO_DAEMON")
            .env_remove("TERM")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &case.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// One run, rendered and normalised: the text a snapshot holds.
    fn run(&self, case: &Case, json: bool) -> String {
        let mut args = if json {
            case.json.clone()
        } else {
            case.plain.clone()
        };
        if json {
            args.insert(0, "--json".to_string());
        }
        let mut child = self.command(&args, case).spawn().expect("spawn mcpdial");
        {
            let mut stdin = child.stdin.take().unwrap();
            if let Some(text) = &case.stdin {
                stdin.write_all(text.as_bytes()).unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        let code = out.status.code().unwrap_or(-1);
        let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        match case.name {
            "guide" => {
                assert_eq!(
                    stdout,
                    include_str!("../docs/AGENTS.md"),
                    "guide is the file"
                );
                stdout = "(docs/AGENTS.md, byte for byte)\n".to_string();
            }
            "completions" => {
                assert!(stdout.contains("complete -F"), "{stdout}");
                stdout = "(the bash completion script)\n".to_string();
            }
            _ => {}
        }
        let command = format!("mcpdial {}", args.join(" "));
        self.names
            .normalise(&render(&command, code, &stdout, &stderr))
    }
}

fn snapshot_path(case: &Case, json: bool) -> PathBuf {
    let suffix = if json { ".json.snap" } else { ".snap" };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("{}{suffix}", case.name))
}

/// The line-by-line difference, for a failure to show.
fn diff(expected: &str, actual: &str) -> String {
    let mut out = String::new();
    let (e, a): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    for i in 0..e.len().max(a.len()) {
        match (e.get(i), a.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => {
                if let Some(x) = x {
                    out.push_str(&format!("  - {x}\n"));
                }
                if let Some(y) = y {
                    out.push_str(&format!("  + {y}\n"));
                }
            }
        }
    }
    out
}

/// Compares a run with its snapshot, or rewrites the snapshot when asked to.
/// Returns what went wrong, so every drift is reported at once.
fn check(contract: &Contract, case: &Case, json: bool) -> Option<String> {
    let actual = contract.run(case, json);
    let path = snapshot_path(case, json);
    if std::env::var_os(UPDATE_ENV).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return None;
    }
    let Ok(expected) = std::fs::read_to_string(&path) else {
        return Some(format!(
            "{} has no snapshot yet; {UPDATE_ENV}=1 writes {}",
            case.name,
            path.display()
        ));
    };
    let expected = expected.replace("\r\n", "\n");
    if expected == actual {
        return None;
    }
    Some(format!(
        "{} differs from {}:\n{}",
        case.name,
        path.display(),
        diff(&expected, &actual)
    ))
}

/// The commands, in an order where each finds the state the ones before it
/// left. Servers saved from the registry come last, because `tools` with no
/// target would try to run their `npx` and report a spawn error in the
/// operating system's own words.
fn cases(contract: &Contract, url: &str, auth_url: &str) -> Vec<Case> {
    let echo = echo_command();
    let home = contract.home.display().to_string();
    let host_config = contract.home.join("host-config.json");
    std::fs::write(
        &host_config,
        json!({"mcpServers": {
            "imported": {"type": "http", "url": url, "headers": {"X-From": "import"}},
            "local": {"command": echo_server().display().to_string(), "args": ["--flag"]},
        }})
        .to_string(),
    )
    .unwrap();
    let host_config = host_config.display().to_string();
    let empty_config = contract.home.join("empty-config.json");
    std::fs::write(&empty_config, "{\"mcpServers\": {}}").unwrap();
    let empty_config = empty_config.display().to_string();
    let saved = format!("{home}/saved");
    let saved_json = format!("{home}/saved-json");
    let args_file = contract.home.join("args.json");
    std::fs::write(&args_file, "{\"message\": \"from a file\"}").unwrap();
    let args_file = format!("@{}", args_file.display());

    let mut cases = vec![
        // Saving servers.
        Case::each(
            "add-http",
            &["add", "web", "--http", url, "--no-probe"],
            &["add", "web", "--http", url, "--no-probe", "--force"],
        ),
        Case::each(
            "add-http-timeout",
            &["add", "slow", "--http", url, "--timeout", "5", "--no-probe"],
            &[
                "add",
                "slow",
                "--http",
                url,
                "--timeout",
                "5",
                "--no-probe",
                "--force",
            ],
        ),
        Case::each(
            "add-stdio",
            &["add", "echo", "--stdio", &echo, "--no-probe"],
            &["add", "echo", "--stdio", &echo, "--no-probe", "--force"],
        ),
        Case::each(
            "add-lists",
            &[
                "add",
                "fs",
                "--stdio",
                &echo,
                "--allow",
                "echo",
                "--allow",
                "co*",
                "--deny",
                "fail",
                "--no-probe",
            ],
            &[
                "add",
                "fs",
                "--stdio",
                &echo,
                "--allow",
                "echo",
                "--allow",
                "co*",
                "--deny",
                "fail",
                "--no-probe",
                "--force",
            ],
        ),
        Case::each(
            "add-catalog",
            &["add", "cat", "--catalog", "fake", "--no-probe"],
            &["add", "cat", "--catalog", "fake", "--no-probe", "--force"],
        ),
        Case::both(
            "add-duplicate",
            &["add", "web", "--http", url, "--no-probe"],
        ),
        Case::both(
            "add-bad-url",
            &["add", "bad", "--http", "notaurl", "--no-probe"],
        ),
        Case::both(
            "add-bad-header",
            &["add", "bad", "--http", url, "-H", "nocolon", "--no-probe"],
        ),
        Case::both(
            "add-missing-catalog",
            &["add", "bad", "--catalog", "nope", "--no-probe"],
        ),
        // The lists.
        Case::both("set-show", &["set", "fs"]),
        Case::both(
            "set-change",
            &["set", "fs", "--deny", "fail", "--deny", "strict"],
        ),
        Case::both("set-clear", &["set", "fs", "--clear-allow"]),
        Case::both("set-missing", &["set", "nobody"]),
        // Looking at what is saved.
        Case::both("ls", &["ls", "--no-probe"]),
        Case::both("catalog", &["catalog"]),
        Case::both("browse", &["browse"]),
        Case::both("import", &["import", &host_config]).with_json_force(),
        Case::both("import-empty", &["import", &empty_config]),
        // What a server offers.
        Case::both("tools", &["tools", "web"]),
        Case::both("tools-long", &["tools", "web", "--long"]),
        Case::both("tools-stdio", &["tools", "echo"]),
        Case::both("tools-all", &["tools", "fs", "--all"]),
        Case::both("tools-every", &["tools"]),
        Case::both("tools-unknown-server", &["tools", "nobody"]),
        Case::both("info", &["info", "web"]),
        Case::both("info-stdio", &["info", "echo"]),
        Case::both("schema", &["schema", "web", "add"]),
        Case::both("schema-missing", &["schema", "web", "nah"]),
        Case::both("resources", &["resources", "web"]),
        Case::both("resources-long", &["resources", "web", "--long"]),
        Case::both("resources-none", &["resources", "echo"]),
        Case::both("prompts", &["prompts", "web"]),
        Case::both("prompts-long", &["prompts", "web", "--long"]),
        Case::both("prompts-none", &["prompts", "echo"]),
        // Calling.
        Case::both("call", &["call", "web", "add", r#"{"a":40,"b":2}"#]),
        Case::both("call-pairs", &["call", "web", "add", "a=40", "b=2"]),
        Case::both(
            "call-pairs-json",
            &["call", "echo", "echo", r#"message:="typed""#],
        ),
        Case::both("call-pairs-mixed", &["call", "web", "add", "a=1", "{}"]),
        Case::both(
            "call-stdio",
            &["call", "echo", "echo", r#"{"message":"hi"}"#],
        ),
        Case::both("call-file", &["call", "echo", "echo", &args_file]),
        Case::both("call-stdin", &["call", "echo", "echo", "-"])
            .stdin("{\"message\": \"from stdin\"}\n"),
        Case::both("call-failed", &["call", "web", "fail"]),
        Case::both("call-unknown-tool", &["call", "web", "nope"]),
        Case::both("call-near-tool", &["call", "web", "ecoh"]),
        Case::both("call-bad-arguments", &["call", "echo", "echo", "{}"]),
        Case::both("call-failed-arguments", &["call", "echo", "strict", "{}"]),
        Case::both("call-unquoted", &["call", "web", "echo", "{message:hi}"]),
        Case::both("call-not-object", &["call", "web", "echo", "[1]"]),
        Case::both("call-denied", &["call", "fs", "fail"]),
        Case::both(
            "call-url",
            &["call", url, "echo", r#"{"message":"ad hoc"}"#],
        ),
        Case::both(
            "call-plain-flag",
            &["--plain", "call", "web", "echo", r#"{"message":"plain"}"#],
        ),
        Case::both("raw", &["raw", "web", "tools/list"]),
        Case::both("raw-unknown-method", &["raw", "web", "nope/method"]),
        Case::both("read", &["read", "web", "file:///readme.md"]),
        Case::each(
            "read-save-dir",
            &["--save-dir", &saved, "read", "web", "file:///logo.png"],
            &["--save-dir", &saved_json, "read", "web", "file:///logo.png"],
        ),
        Case::both("read-missing", &["read", "web", "file:///nope"]),
        Case::each(
            "call-save-dir",
            &["--save-dir", &saved, "call", "echo", "shot"],
            &["--save-dir", &saved_json, "call", "echo", "shot"],
        ),
        Case::both("call-media", &["call", "echo", "shot"]),
        Case::both("save-dir-misuse", &["--save-dir", &saved, "ls"]),
        Case::both(
            "prompt",
            &["prompt", "web", "summarize", r#"{"text":"a memo"}"#],
        ),
        Case::both("prompt-missing", &["prompt", "web", "nope"]),
        Case::both(
            "prompt-unquoted",
            &["prompt", "web", "summarize", "{text:memo}"],
        ),
        Case::both("prompt-pairs", &["prompt", "web", "summarize", "text=a memo"]),
        Case::both("shell", &["shell", "web"]).stdin(
            "tools\ncall add {\"a\":1,\"b\":2}\ncall add a=5 b=6\nnope\ncall nope\ncall add\nresources\n\
             prompts\nprompt greet\nread file:///readme.md\ninfo\nhelp\nhelp add\n\
             schema add\nraw tools/list\nadd\n# a comment\n\nquit\n",
        ),
        Case::both("shell-denied", &["shell", "fs"])
            .stdin("call fail {}\ncall echo {\"message\":\"ok\"}\n"),
        // Tokens, against a server that wants one.
        Case::each(
            "add-auth",
            &["add", "work", "--http", auth_url, "--no-probe"],
            &["add", "work", "--http", auth_url, "--no-probe", "--force"],
        ),
        Case::both(
            "login-client-credentials",
            &[
                "login",
                "work",
                "--grant",
                "client-credentials",
                "--client-id",
                CONFIDENTIAL_ID,
                "--client-secret-env",
                "CLIENT_SECRET",
            ],
        )
        .env("CLIENT_SECRET", CONFIDENTIAL_SECRET),
        Case::both("token-show", &["token", "show", "work"]),
        Case::both(
            "call-with-token",
            &["call", "work", "echo", r#"{"message":"authorized"}"#],
        ),
        Case::both("logout", &["logout", "work"]),
        Case::both("token-set", &["token", "set", "work", "--env", "TOKEN"])
            .env("TOKEN", "tok-manual"),
        Case::both("token-show-manual", &["token", "show", "work"]),
        Case::both("token-rm", &["token", "rm", "work"]).only_piped(),
        Case::both("token-set-stdin", &["token", "set", "work"]).stdin("tok-stdin\n"),
        Case::both("token-rm", &["token", "rm", "work"]).only_json(),
        Case::both("token-show-missing", &["token", "show", "work"]),
        Case::both("login-stdio", &["login", "echo"]),
        Case::both("guide", &["guide"]),
        Case::both("completions", &["completions", "bash"]),
        // From the registry: saved without being run, and never probed.
        Case::each(
            "add-registry",
            &[
                "add",
                "files",
                "--registry",
                "io.github.acme/files",
                "--arg",
                "/srv",
            ],
            &[
                "add",
                "files",
                "--registry",
                "io.github.acme/files",
                "--arg",
                "/srv",
                "--force",
            ],
        ),
        Case::both(
            "add-registry-missing-arg",
            &["add", "bad", "--registry", "io.github.acme/files"],
        ),
        Case::both(
            "add-registry-not-a-name",
            &["add", "bad", "--registry", "files"],
        ),
        Case::each(
            "add-catalog-registry",
            &["add", "box", "--catalog", "box", "--no-probe"],
            &["add", "box", "--catalog", "box", "--no-probe", "--force"],
        ),
        Case::both("search", &["search", "acme"]),
        Case::both("search-limit", &["search", "acme", "--limit", "1"]),
        Case::both("search-offline", &["search", "fake", "--offline"]),
        Case::both("search-none", &["search", "zzz"]),
        Case::both("search-empty", &["search"]),
        Case::both("ls-registry", &["ls", "--no-probe"]),
        Case::each("rm", &["rm", "files"], &["rm", "box"]),
        Case::both("rm-missing", &["rm", "nobody"]),
    ];
    if cfg!(unix) {
        // A daemon is unix only; on Windows `start` is exit 2 in the OS's words.
        let start = ["start", "echo", "--idle", "60"];
        cases.extend([
            Case::both("start", &start).only_piped(),
            Case::both("start-again", &start),
            Case::both("call-daemon", &["call", "echo", "count"]),
            Case::both("ls-daemon", &["ls", "--no-probe"]),
            Case::both("stop", &["stop", "echo"]).only_piped(),
            Case::both("stop-missing", &["stop", "echo"]),
            Case::both("start", &start).only_json(),
            Case::both("stop", &["stop", "echo"]).only_json(),
        ]);
    }
    cases
}

#[test]
fn every_command_prints_the_bytes_its_snapshot_holds() {
    let fake = start(Mode::Stateless);
    let auth = start(Mode::Confidential {
        auth_method: "client_secret_post".to_string(),
    });
    let contract = Contract::new(&[&fake, &auth]);
    let mut failures = Vec::new();
    for case in cases(&contract, &fake.url, &auth.url) {
        if case.piped {
            failures.extend(check(&contract, &case, false));
        }
        if case.with_json {
            failures.extend(check(&contract, &case, true));
        }
    }
    assert!(
        failures.is_empty(),
        "the agent contract changed:\n\n{}\nIf that is intended, {UPDATE_ENV}=1 cargo test --test contract rewrites the snapshots; say so in the PR.",
        failures.join("\n")
    );
}

/// `script` runs a command inside a pseudo-terminal, so stdout is a terminal
/// as far as the command can tell. Both streams arrive on that terminal,
/// merged, with `\r\n` line endings. BSD `script` takes the command as
/// arguments; util-linux wants one string after `-c`, and `-e` to hand the
/// command's exit status on.
fn under_pty(home: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, Vec<u8>) {
    let bsd = cfg!(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ));
    let mut cmd = Command::new("script");
    cmd.arg("-q");
    if bsd {
        cmd.arg("/dev/null")
            .arg(env!("CARGO_BIN_EXE_mcpdial"))
            .args(args);
    } else {
        let line: Vec<String> = std::iter::once(env!("CARGO_BIN_EXE_mcpdial"))
            .chain(args.iter().copied())
            .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
            .collect();
        cmd.args(["-e", "-c", &line.join(" "), "/dev/null"]);
    }
    cmd.env("MCPDIAL_HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("MCPDIAL_PLAIN")
        .env_remove("MCPDIAL_JSON")
        .env_remove("MCPDIAL_TIMEOUT")
        .env_remove("MCPDIAL_USER_AGENT")
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn script");
    // Held open until the command has exited: an end of file on the terminal's
    // input would be echoed as `^D`, as a typed one is.
    let stdin = child.stdin.take();
    let out = child.wait_with_output().unwrap();
    drop(stdin);
    (out.status.code().unwrap_or(-1), out.stdout)
}

/// What the terminal showed, as the bytes the command wrote: without the
/// carriage returns the terminal adds, or the echo of an end of file.
fn typed(pty: &[u8]) -> Vec<u8> {
    let pty = pty.strip_prefix(b"^D\x08\x08").unwrap_or(pty);
    pty.iter().copied().filter(|b| *b != b'\r').collect()
}

fn snapshot_stdout(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.snap"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n");
    let (_, rest) = text.split_once("--- stdout\n").unwrap();
    let (stdout, _) = rest.split_once("--- stderr\n").unwrap();
    stdout.to_string()
}

/// `MCPDIAL_PLAIN=1` at a terminal is the pipe, byte for byte; so is `--plain`
/// and so is `TERM=dumb`. Without any of them a terminal gets `Rich`, which is
/// only observable so far where it refuses to put a binary resource on the
/// screen, and only in a build with the `rich` feature. The `script` that
/// makes the terminal is a unix tool, so Windows skips this.
#[test]
fn a_terminal_asked_for_plain_output_gets_the_piped_bytes() {
    if !cfg!(unix) {
        eprintln!("skipped: needs `script` to make a pseudo-terminal");
        return;
    }
    let fake = start(Mode::Stateless);
    let home = contract_home("pty");
    let saved = Command::new(env!("CARGO_BIN_EXE_mcpdial"))
        .env("MCPDIAL_HOME", &home)
        .args(["add", "web", "--http", &fake.url, "--no-probe"])
        .output()
        .unwrap();
    assert!(saved.status.success());

    let (code, pty) = under_pty(&home, &["tools", "web"], &[("MCPDIAL_PLAIN", "1")]);
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&typed(&pty)),
        snapshot_stdout("tools"),
        "MCPDIAL_PLAIN=1 at a terminal is the piped snapshot"
    );

    let piped = Command::new(env!("CARGO_BIN_EXE_mcpdial"))
        .env("MCPDIAL_HOME", &home)
        .args(["read", "web", "file:///logo.png"])
        .output()
        .unwrap();
    assert!(
        piped.status.success(),
        "{}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let bytes: Vec<u8> = piped
        .stdout
        .iter()
        .copied()
        .filter(|b| *b != b'\r')
        .collect();
    let read = ["read", "web", "file:///logo.png"];
    for (label, args, env) in [
        (
            "MCPDIAL_PLAIN=1",
            read.to_vec(),
            vec![("MCPDIAL_PLAIN", "1")],
        ),
        (
            "--plain",
            ["--plain"].iter().chain(&read).copied().collect(),
            vec![],
        ),
        ("TERM=dumb", read.to_vec(), vec![("TERM", "dumb")]),
    ] {
        let (code, pty) = under_pty(&home, &args, &env);
        assert_eq!(code, 0, "{label}: {}", String::from_utf8_lossy(&pty));
        assert_eq!(
            typed(&pty),
            bytes,
            "{label} at a terminal is the piped bytes"
        );
    }

    let (code, pty) = under_pty(&home, &read, &[]);
    let shown = String::from_utf8_lossy(&typed(&pty)).into_owned();
    if cfg!(feature = "rich") {
        assert_eq!(code, 2, "{shown}");
        assert!(
            shown.contains("error: this resource is binary and stdout is a terminal"),
            "{shown}"
        );
        assert!(
            shown.contains("mcpdial read web file:///logo.png > file"),
            "{shown}"
        );
    } else {
        assert_eq!(code, 0, "{shown}");
        assert_eq!(
            typed(&pty),
            bytes,
            "without the rich feature a terminal is a pipe"
        );
    }
}
