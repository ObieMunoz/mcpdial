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
