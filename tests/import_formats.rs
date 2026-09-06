//! `import` reads VS Code's `servers` with `inputs` and `envFile`, Codex's
//! `config.toml` and OpenCode's `mcp`, scans where each host keeps its file, and
//! `--from` keeps one host's.

mod common;

use common::{mcpdial, run, temp_home};
use serde_json::{json, Value};

fn saved(home: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap()
}

#[test]
fn vscode_inputs_become_placeholders_and_a_note() {
    let home = temp_home("import-vscode");
    let workspace = home.join("project");
    std::fs::create_dir_all(workspace.join(".vscode")).unwrap();
    std::fs::write(
        workspace.join(".env"),
        "API_KEY=hunter2\n# note\nREGION=us\n",
    )
    .unwrap();
    let cfg = workspace.join(".vscode/mcp.json");
    std::fs::write(
        &cfg,
        json!({
            "inputs": [
                {"id": "github-token", "type": "promptString", "description": "GitHub PAT", "password": true}
            ],
            "servers": {
                "github": {
                    "type": "http",
                    "url": "https://api.example.com/mcp",
                    "headers": {"Authorization": "Bearer ${input:github-token}"}
                },
                "local": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "server"],
                    "env": {"TOKEN": "${input:github-token}"},
                    "envFile": "${workspaceFolder}/.env"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 2 server(s)"), "{}", o.stderr);
    assert!(o.stderr.contains("[servers in"), "{}", o.stderr);
    assert!(
        o.stderr.contains(
            "note: set before dialing: MCPDIAL_INPUT_GITHUB_TOKEN (input github-token, secret: GitHub PAT)"
        ),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr.contains("set API_KEY, REGION before dialing"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("hunter2"), "{}", o.stderr);

    let file = saved(&home);
    assert_eq!(
        file["servers"]["github"]["headers"]["Authorization"],
        "Bearer ${MCPDIAL_INPUT_GITHUB_TOKEN}"
    );
    assert_eq!(
        file["servers"]["local"]["env"],
        json!({"API_KEY": "${API_KEY}", "REGION": "${REGION}", "TOKEN": "${MCPDIAL_INPUT_GITHUB_TOKEN}"})
    );
    assert!(!file.to_string().contains("hunter2"), "{file}");

    // The placeholder is filled in from the environment at dial time and refused
    // without it, like any other.
    let o = run(mcpdial(&home)
        .env_remove("MCPDIAL_INPUT_GITHUB_TOKEN")
        .args(["info", "github"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("MCPDIAL_INPUT_GITHUB_TOKEN"),
        "{}",
        o.stderr
    );

    // Under --json the notes ride in the receipt, keyed by server.
    let o = run(mcpdial(&home).args(["--json", "import", cfg.to_str().unwrap(), "--force"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.is_empty(), "{}", o.stderr);
    let receipt: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(receipt["imported"], json!(["github", "local"]));
    assert_eq!(receipt["skipped"], json!([]));
    assert_eq!(receipt["notes"]["github"].as_array().unwrap().len(), 1);
    assert_eq!(receipt["notes"]["local"].as_array().unwrap().len(), 2);
    assert!(
        receipt["notes"]["local"][1]
            .as_str()
            .unwrap()
            .starts_with("envFile "),
        "{receipt}"
    );
}

#[test]
fn codex_toml_file() {
    let home = temp_home("import-codex");
    let cfg = home.join("config.toml");
    std::fs::write(
        &cfg,
        r#"model = "o3"

[mcp_servers.docs]
command = "npx"
args = ["-y", "docs-server", "/srv/my docs"]

[mcp_servers.docs.env]
LOG_LEVEL = "debug"

[mcp_servers.api]
url = "https://api.example.com/mcp"
bearer_token_env_var = "API_TOKEN"
http_headers = { X-Tenant = "acme" }
"#,
    )
    .unwrap();

    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 2 server(s)"), "{}", o.stderr);
    assert!(o.stderr.contains("[mcp_servers in"), "{}", o.stderr);
    let file = saved(&home);
    assert_eq!(
        file["servers"]["docs"],
        json!({"stdio": "npx -y docs-server '/srv/my docs'", "env": {"LOG_LEVEL": "debug"}})
    );
    assert_eq!(
        file["servers"]["api"],
        json!({"http": "https://api.example.com/mcp", "token_env": "API_TOKEN", "headers": {"X-Tenant": "acme"}})
    );

    // A TOML mistake is a config error naming the file and the line.
    std::fs::write(&cfg, "[mcp_servers.x]\ncommand = \"\"\"a\"\"\"\n").unwrap();
    let o = run(mcpdial(&home).args(["import", cfg.to_str().unwrap()]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("config.toml: line 2"), "{}", o.stderr);
}

#[test]
fn scan_finds_every_host_and_from_keeps_one() {
    let home = temp_home("import-scan");
    let user_home = home.join("user");
    let project = home.join("project");
    std::fs::create_dir_all(user_home.join(".codex")).unwrap();
    std::fs::create_dir_all(user_home.join(".config/opencode")).unwrap();
    std::fs::create_dir_all(project.join(".vscode")).unwrap();
    std::fs::write(
        user_home.join(".codex/config.toml"),
        "[mcp_servers.from_codex]\ncommand = \"codex-server\"\n",
    )
    .unwrap();
    std::fs::write(
        user_home.join(".config/opencode/opencode.json"),
        json!({"mcp": {
            "from_opencode": {"type": "local", "command": ["bun", "x", "server"], "environment": {"A": "1"}},
            "from_opencode_remote": {"type": "remote", "url": "https://mcp.example.com/mcp", "headers": {"X-A": "1"}}
        }})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        project.join(".vscode/mcp.json"),
        json!({"servers": {"from_vscode": {"command": "code-server"}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        project.join(".mcp.json"),
        json!({"mcpServers": {"from_claude": {"command": "claude-server"}}}).to_string(),
    )
    .unwrap();
    let scan = |args: &[&str]| {
        let mut c = mcpdial(&home);
        c.env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .env_remove("APPDATA")
            .current_dir(&project)
            .arg("import")
            .args(args);
        run(&mut c)
    };

    let o = scan(&["--from", "codex"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 1 server(s)"), "{}", o.stderr);
    assert_eq!(
        saved(&home)["servers"],
        json!({"from_codex": {"stdio": "codex-server"}})
    );

    let o = scan(&["--from", "opencode", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(o.stdout.trim()).unwrap(),
        json!({"imported": ["from_opencode", "from_opencode_remote"], "skipped": []})
    );
    let file = saved(&home);
    assert_eq!(
        file["servers"]["from_opencode"],
        json!({"stdio": "bun x server", "env": {"A": "1"}})
    );
    assert_eq!(
        file["servers"]["from_opencode_remote"],
        json!({"http": "https://mcp.example.com/mcp", "headers": {"X-A": "1"}})
    );

    let o = scan(&[]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stderr.contains("imported 2 server(s)"), "{}", o.stderr);
    let names: Vec<String> = saved(&home)["servers"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        names,
        [
            "from_claude",
            "from_codex",
            "from_opencode",
            "from_opencode_remote",
            "from_vscode"
        ]
    );

    let o = scan(&["--from", "cursor"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("no config files found"), "{}", o.stderr);

    let o = scan(&["--from", "emacs"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr.contains("unknown host emacs") && o.stderr.contains("vscode"),
        "{}",
        o.stderr
    );

    let o = scan(&["--from", "codex", "config.toml"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("cannot be used with"), "{}", o.stderr);
}

/// `export` is the same knowledge pointed the other way: a saved server written
/// back out in a host's shape, with no credential in it.
fn add_three(home: &std::path::Path) {
    let add = |args: &[&str]| {
        let o = run(mcpdial(home).arg("add").args(args).arg("--no-probe"));
        assert_eq!(o.code, 0, "{}", o.stderr);
    };
    add(&[
        "fs",
        "--stdio",
        "npx -y server-filesystem '/tmp/my dir'",
        "--env",
        "DEBUG=1",
        "--cwd",
        "/p",
        "--deny",
        "delete_*",
    ]);
    add(&[
        "wiki",
        "--http",
        "https://wiki.example.com/mcp",
        "-H",
        "X-A: 1",
    ]);
    add(&[
        "tok",
        "--http",
        "https://api.example.com/mcp",
        "--token-env",
        "API_TOKEN",
    ]);
    std::fs::write(
        home.join("credentials.json"),
        json!({"credentials": {"wiki": {"access_token": "super-secret-value"}}}).to_string(),
    )
    .unwrap();
}

#[test]
fn export_round_trips_through_import_and_leaves_credentials_behind() {
    let home = temp_home("export-round-trip");
    add_three(&home);

    let o = run(mcpdial(&home).args(["export"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("super-secret-value"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains("wiki: exported without its saved token; the host will need its own login"),
        "{}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains("fs: its allow and deny lists are not exported"),
        "{}",
        o.stderr
    );
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        doc["mcpServers"]["fs"],
        json!({"command": "npx", "args": ["-y", "server-filesystem", "/tmp/my dir"],
               "env": {"DEBUG": "1"}, "cwd": "/p"}),
        "the quoted argument is one element of args"
    );
    assert_eq!(
        doc["mcpServers"]["tok"]["headers"]["Authorization"], "Bearer ${API_TOKEN}",
        "the variable travels, never its value"
    );

    // What was written is read back as the same servers, quoting and all.
    let host_file = home.join("exported.json");
    std::fs::write(&host_file, &o.stdout).unwrap();
    let fresh = temp_home("export-round-trip-back");
    let o = run(mcpdial(&fresh).args(["import", host_file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        saved(&fresh)["servers"]["fs"],
        json!({"stdio": "npx -y server-filesystem '/tmp/my dir'",
               "env": {"DEBUG": "1"}, "cwd": "/p"}),
        "import, export, import: the same server, less the deny list the note named"
    );

    // Under --json a note on stderr is an object like every other line.
    let o = run(mcpdial(&home).args(["--json", "export", "wiki"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let note: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert!(
        note["note"]
            .as_str()
            .unwrap()
            .starts_with("wiki: exported without"),
        "{}",
        o.stderr
    );

    // A name that is not saved is a usage error, and nothing is printed.
    let o = run(mcpdial(&home).args(["export", "fs", "nope"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stdout.is_empty(), "{}", o.stdout);
    assert!(
        o.stderr.contains("no server named \"nope\""),
        "{}",
        o.stderr
    );
}

#[test]
fn export_writes_the_shape_each_host_reads() {
    let home = temp_home("export-formats");
    add_three(&home);

    let o = run(mcpdial(&home).args(["export", "fs", "tok", "--format", "vscode"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(doc["servers"]["fs"]["type"], "stdio");
    assert_eq!(doc["servers"]["tok"]["type"], "http");
    assert_eq!(
        doc["servers"]["tok"]["headers"]["Authorization"],
        "Bearer ${env:API_TOKEN}"
    );

    let o = run(mcpdial(&home).args(["export", "fs", "tok", "--format", "codex"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("bearer_token_env_var = \"API_TOKEN\""),
        "{}",
        o.stdout
    );
    assert!(!o.stderr.contains("VS Code"), "{}", o.stderr);

    // Codex reads it back as the servers it was written from.
    let host_file = home.join("config.toml");
    std::fs::write(&host_file, &o.stdout).unwrap();
    let fresh = temp_home("export-formats-back");
    let o = run(mcpdial(&fresh).args(["import", host_file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        saved(&fresh)["servers"]["fs"],
        json!({"stdio": "npx -y server-filesystem '/tmp/my dir'",
               "env": {"DEBUG": "1"}, "cwd": "/p"})
    );
    assert_eq!(
        saved(&fresh)["servers"]["tok"],
        saved(&home)["servers"]["tok"],
        "a token_env is Codex's own field"
    );
}

#[test]
fn merge_replaces_the_exported_entries_and_keeps_the_rest() {
    let home = temp_home("export-merge");
    add_three(&home);

    let host_file = home.join("host.json");
    std::fs::write(
        &host_file,
        json!({"otherKey": {"keep": true},
               "mcpServers": {"theirs": {"command": "theirs"}, "fs": {"command": "stale"}}})
        .to_string(),
    )
    .unwrap();
    let o = run(mcpdial(&home).args(["export", "fs", "--merge", host_file.to_str().unwrap()]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let doc: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(doc["otherKey"], json!({"keep": true}), "untouched");
    assert_eq!(doc["mcpServers"]["theirs"], json!({"command": "theirs"}));
    assert_eq!(doc["mcpServers"]["fs"]["command"], "npx", "replaced");
    assert!(
        std::fs::read_to_string(&host_file)
            .unwrap()
            .contains("stale"),
        "the file itself is never written"
    );

    let toml_file = home.join("config.toml");
    std::fs::write(
        &toml_file,
        "model = \"o3\"\n\n# keep me\n[profiles.fast]\nmodel = \"o4-mini\"\n\n\
         [mcp_servers.fs]\ncommand = \"stale\"\n\n[mcp_servers.fs.env]\nOLD = \"1\"\n\n\
         [mcp_servers.theirs]\ncommand = \"theirs\"\n",
    )
    .unwrap();
    let o = run(mcpdial(&home).args([
        "export",
        "fs",
        "--format",
        "codex",
        "--merge",
        toml_file.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.starts_with("model = \"o3\"\n\n# keep me\n"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout
            .contains("[mcp_servers.theirs]\ncommand = \"theirs\""),
        "{}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("stale") && !o.stdout.contains("OLD"),
        "the replaced table is gone: {}",
        o.stdout
    );

    // A file mcpdial cannot read line for line is refused, not guessed at.
    std::fs::write(&toml_file, "[[history]]\nx = 1\n").unwrap();
    let o = run(mcpdial(&home).args([
        "export",
        "fs",
        "--format",
        "codex",
        "--merge",
        toml_file.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("arrays of tables"), "{}", o.stderr);
}
