use clap::{Parser, Subcommand};
use mcpdial::client::{self, describe_params, Options, Probe, Status};
use mcpdial::{oauth, Credential, Error, ServerConfig, Store, USER_AGENT};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

const EXIT_ERROR: u8 = 1; // the server said no: JSON-RPC error, HTTP error, or tool isError
const EXIT_USAGE: u8 = 2; // bad arguments or config; nothing was sent

/// Dial any MCP server from the shell. No SDK, no host app, no connector.
///
/// TARGET is a saved server name, an http(s):// URL, or stdio:<command>.
#[derive(Parser)]
#[command(
    name = "mcpdial",
    version,
    about,
    after_help = "\
examples:
  mcpdial add wiki --http https://mcp.deepwiki.com/mcp
  mcpdial add fs --stdio \"npx -y @modelcontextprotocol/server-filesystem /tmp\"
  mcpdial ls                      # every saved server with its live status
  mcpdial tools                   # every tool on every server
  mcpdial login work              # one-time browser step; the token is saved
  mcpdial call wiki read_wiki_structure '{\"repoName\":\"modelcontextprotocol/servers\"}'
  mcpdial call https://mcp.deepwiki.com/mcp read_wiki_structure '{\"repoName\":\"x/y\"}'
  mcpdial tools 'stdio:npx -y @modelcontextprotocol/server-everything stdio'

calling from a program or an agent: pass --json everywhere and run `mcpdial guide`."
)]
struct Cli {
    /// Seconds to wait for a reply
    #[arg(long, global = true, default_value_t = 60.0, value_name = "SECS")]
    timeout: f64,

    /// Emit JSON instead of a readable summary
    #[arg(long, global = true)]
    json: bool,

    /// Trace every message on stderr (and pass a stdio server's stderr through)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Override the User-Agent (HTTP only)
    #[arg(long, global = true, default_value = USER_AGENT, hide_default_value = true)]
    user_agent: String,

    /// Extra HTTP header, repeatable. With `add`, saved to the server.
    #[arg(
        short = 'H',
        long = "header",
        global = true,
        value_name = "'Name: value'"
    )]
    headers: Vec<String>,

    /// Env var holding a bearer token; beats any saved credential. With `add`, saved.
    #[arg(long, global = true, value_name = "VAR")]
    token_env: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save a server under a name
    Add {
        name: String,
        /// Streamable HTTP endpoint
        #[arg(
            long,
            value_name = "URL",
            conflicts_with = "stdio",
            required_unless_present = "stdio"
        )]
        http: Option<String>,
        /// Command that speaks MCP on stdio
        #[arg(long, value_name = "CMD")]
        stdio: Option<String>,
        /// Environment variable for the stdio process, repeatable
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Working directory for the stdio process
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
    },
    /// Import servers from a host's config (Claude Code, Claude Desktop, Cursor, ...)
    Import {
        /// A JSON file with an `mcpServers` object. Omit to scan the usual locations.
        file: Option<std::path::PathBuf>,
        /// Overwrite servers that already exist under the same name
        #[arg(long)]
        force: bool,
    },
    /// Keep one session open and run commands from stdin (state persists between calls)
    Shell { target: String },
    /// Forget a server and any credential saved for it
    Rm { name: String },
    /// List saved servers with their live connection status
    Ls {
        /// Do not connect; just show the configuration
        #[arg(long)]
        no_probe: bool,
    },
    /// Show the tools a server offers (every server when no target is given)
    Tools {
        target: Option<String>,
        /// Show full descriptions and parameters
        #[arg(short, long)]
        long: bool,
    },
    /// Initialize and show server identity and capabilities
    Info { target: String },
    /// Call a tool
    Call {
        target: String,
        tool: String,
        /// JSON object of arguments: inline, @file, or - for stdin
        #[arg(default_value = "{}")]
        arguments: String,
    },
    /// Show one tool's name, description, and input schema
    Schema { target: String, tool: String },
    /// Send any JSON-RPC method
    Raw {
        target: String,
        method: String,
        /// JSON object of params: inline, @file, or - for stdin
        #[arg(default_value = "{}")]
        params: String,
    },
    /// Print the usage guide written for programs and agents that call mcpdial
    Guide,
    /// Authorize in the browser once and save the token (HTTP servers)
    Login {
        target: String,
        /// Space-separated scopes (default: whatever the server advertises)
        #[arg(long)]
        scope: Option<String>,
        /// Fixed loopback port for the redirect (default: any free port)
        #[arg(long)]
        port: Option<u16>,
        /// Use a pre-registered client id instead of dynamic registration
        #[arg(long)]
        client_id: Option<String>,
        /// Loopback host in the redirect URI: 127.0.0.1 (default, with a localhost
        /// fallback if the server refuses it) or localhost
        #[arg(long, value_name = "HOST", value_parser = ["127.0.0.1", "localhost"])]
        redirect_host: Option<String>,
        /// Print the URL but do not try to open a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Delete the saved credential for a server
    Logout { target: String },
    /// Manage saved tokens without the browser
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Save a token read from stdin, or from --env VAR. Never from an argument.
    Set {
        name: String,
        #[arg(long, value_name = "VAR")]
        env: Option<String>,
    },
    /// Describe the saved credential without revealing it
    Show { name: String },
    /// Delete the saved credential
    Rm { name: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            if json {
                eprintln!("{}", error_json(&e));
            } else {
                eprintln!("error: {e}");
            }
            ExitCode::from(match e {
                Error::Usage(_) | Error::Config(_) => EXIT_USAGE,
                _ => EXIT_ERROR,
            })
        }
    }
}

/// One JSON object per error, so a program can branch on `kind` without parsing prose.
fn error_json(e: &Error) -> Value {
    let mut v = json!({ "message": e.to_string() });
    match e {
        Error::Rpc { code, data, .. } => {
            v["kind"] = json!("rpc");
            v["code"] = json!(code);
            if let Some(d) = data {
                v["data"] = d.clone();
            }
        }
        Error::Http {
            status,
            www_authenticate,
            ..
        } => {
            v["kind"] = json!("http");
            v["status"] = json!(status);
            if let Some(w) = www_authenticate {
                v["www_authenticate"] = json!(w);
            }
        }
        Error::Transport(_) => v["kind"] = json!("transport"),
        Error::Auth(_) => v["kind"] = json!("auth"),
        Error::Config(_) => v["kind"] = json!("config"),
        Error::Usage(_) => v["kind"] = json!("usage"),
    }
    json!({ "error": v })
}

/// A JSON object given inline, as `@path` to read a file, or `-` to read stdin.
fn read_json_arg(text: &str, what: &str) -> Result<Value, Error> {
    let owned;
    let text = if text == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::usage(format!("reading {what} from stdin: {e}")))?;
        owned = buf;
        &owned
    } else if let Some(path) = text.strip_prefix('@') {
        owned = std::fs::read_to_string(path)
            .map_err(|e| Error::usage(format!("reading {what} from {path}: {e}")))?;
        &owned
    } else {
        text
    };
    parse_object(text, what)
}

fn parse_object(text: &str, what: &str) -> Result<Value, Error> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Error::usage(format!("{what} were not valid JSON: {e}")))?;
    if !v.is_object() {
        return Err(Error::usage(format!("{what} must be a JSON object")));
    }
    Ok(v)
}

fn parse_headers(items: &[String]) -> Result<Vec<(String, String)>, Error> {
    items
        .iter()
        .map(|item| match item.split_once(':') {
            Some((name, value)) if !name.trim().is_empty() => {
                Ok((name.trim().to_string(), value.trim().to_string()))
            }
            _ => Err(Error::usage(format!(
                "--header must look like 'Name: value', got {item:?}"
            ))),
        })
        .collect()
}

fn run(cli: Cli) -> Result<u8, Error> {
    let store = Store::from_env()?;
    let opts = Options {
        timeout: Duration::from_secs_f64(cli.timeout.max(0.0)),
        user_agent: cli.user_agent.clone(),
        extra_headers: parse_headers(&cli.headers)?,
        token_env: cli.token_env.clone(),
        verbose: cli.verbose,
    };

    match cli.cmd {
        Cmd::Add {
            name,
            http,
            stdio,
            env,
            cwd,
        } => {
            let mut cfg = match (http, stdio) {
                (Some(url), None) => ServerConfig::http(url),
                (None, Some(cmd)) => ServerConfig::stdio(cmd),
                _ => {
                    return Err(Error::usage(
                        "pass exactly one of --http URL or --stdio CMD",
                    ))
                }
            };
            if cfg.stdio.is_some() && (!opts.extra_headers.is_empty() || opts.token_env.is_some()) {
                return Err(Error::usage(
                    "--header and --token-env only apply to --http servers",
                ));
            }
            if cfg.http.is_some() && (!env.is_empty() || cwd.is_some()) {
                return Err(Error::usage(
                    "--env and --cwd only apply to --stdio servers",
                ));
            }
            for item in &env {
                match item.split_once('=') {
                    Some((k, v)) if !k.trim().is_empty() => {
                        cfg.env.insert(k.trim().to_string(), v.to_string());
                    }
                    _ => {
                        return Err(Error::usage(format!(
                            "--env must look like KEY=VALUE, got {item:?}"
                        )))
                    }
                }
            }
            cfg.cwd = cwd;
            cfg.headers = opts.extra_headers.iter().cloned().collect();
            cfg.token_env = opts.token_env.clone();
            store.add_server(&name, cfg.clone())?;
            eprintln!("saved {name} ({} {})", cfg.kind(), cfg.location());
            Ok(0)
        }

        Cmd::Import { file, force } => {
            let files: Vec<std::path::PathBuf> = match file {
                Some(f) => vec![f],
                None => import_candidates()
                    .into_iter()
                    .filter(|p| p.exists())
                    .collect(),
            };
            if files.is_empty() {
                return Err(Error::usage(
                    "no config files found; pass a path to a JSON file with an mcpServers object",
                ));
            }
            let existing = store.servers()?;
            let mut added = 0;
            for path in &files {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                let doc: Value = serde_json::from_str(&text)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                let found = mcpdial::import_config::extract(&doc);
                if found.is_empty() {
                    eprintln!("{}: no servers", path.display());
                    continue;
                }
                for f in found {
                    if existing.contains_key(&f.name) && !force {
                        eprintln!(
                            "  skip {:<16} already saved (use --force to overwrite)",
                            f.name
                        );
                        continue;
                    }
                    match store.add_server(&f.name, f.config.clone()) {
                        Ok(()) => {
                            added += 1;
                            eprintln!(
                                "  add  {:<16} {} {}  [{} in {}]",
                                f.name,
                                f.config.kind(),
                                f.config.location(),
                                f.scope,
                                path.display()
                            );
                            if let Some(n) = f.note {
                                eprintln!("       note: {n}");
                            }
                        }
                        Err(e) => eprintln!("  skip {:<16} {e}", f.name),
                    }
                }
            }
            eprintln!("imported {added} server(s)");
            Ok(0)
        }

        Cmd::Shell { target } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let si = &conn.server_info["serverInfo"];
            let interactive = std::io::stdin().is_terminal();
            if interactive {
                eprintln!(
                    "connected to {} {}. Commands: tools, call TOOL [json], raw METHOD [json], info, quit",
                    si["name"].as_str().unwrap_or("?"),
                    si["version"].as_str().unwrap_or("")
                );
            }
            let stdin = std::io::stdin();
            let mut line = String::new();
            let mut failures = 0u32;
            loop {
                if interactive {
                    eprint!("{}> ", r.name);
                    std::io::stderr().flush().ok();
                }
                line.clear();
                let n = stdin
                    .read_line(&mut line)
                    .map_err(|e| Error::usage(e.to_string()))?;
                if n == 0 {
                    break;
                }
                let text = line.trim();
                if text.is_empty() || text.starts_with('#') {
                    continue;
                }
                let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
                let rest = rest.trim();
                let outcome: Result<(), Error> = match word {
                    "quit" | "exit" => break,
                    "help" => {
                        eprintln!("tools | call TOOL [json] | raw METHOD [json] | info | quit");
                        Ok(())
                    }
                    "info" => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&conn.server_info).unwrap()
                        );
                        Ok(())
                    }
                    "tools" => conn.session.list_tools().map(|tools| {
                        if cli.json {
                            println!("{}", json!({ "tools": tools }));
                        } else {
                            println!("{} tool(s):", tools.len());
                            print_tools(&tools, rest == "--long" || rest == "-l");
                        }
                    }),
                    "call" => {
                        let (tool, args) =
                            rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                        if tool.is_empty() {
                            Err(Error::usage("call needs a tool name"))
                        } else {
                            parse_object(args.trim(), "arguments")
                                .and_then(|a| conn.session.call_tool(tool, a))
                                .map(|result| {
                                    if cli.json {
                                        println!("{result}");
                                    } else {
                                        let out = mcpdial::render_content(&result);
                                        if !out.is_empty() {
                                            println!("{out}");
                                        }
                                        if result["isError"].as_bool().unwrap_or(false) {
                                            eprintln!("(tool reported an error)");
                                        }
                                    }
                                })
                        }
                    }
                    "raw" => {
                        let (method, params) =
                            rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                        if method.is_empty() {
                            Err(Error::usage("raw needs a method"))
                        } else {
                            parse_object(params.trim(), "params")
                                .and_then(|p| conn.session.request(method, Some(p)))
                                .map(|result| {
                                    println!(
                                        "{}",
                                        if cli.json {
                                            result.to_string()
                                        } else {
                                            serde_json::to_string_pretty(&result).unwrap()
                                        }
                                    )
                                })
                        }
                    }
                    other => Err(Error::usage(format!("unknown command {other:?}; try help"))),
                };
                if let Err(e) = outcome {
                    failures += 1;
                    if cli.json {
                        println!("{}", error_json(&e));
                    } else {
                        eprintln!("error: {e}");
                    }
                }
            }
            Ok(if failures > 0 && !interactive {
                EXIT_ERROR
            } else {
                0
            })
        }

        Cmd::Schema { target, tool } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let tools = conn.session.list_tools()?;
            let Some(t) = tools.iter().find(|t| t["name"] == tool) else {
                let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
                return Err(Error::Rpc {
                    code: -32602,
                    message: format!("Tool {tool} not found; available: {}", names.join(", ")),
                    data: None,
                });
            };
            println!("{}", serde_json::to_string_pretty(t).unwrap());
            Ok(0)
        }

        Cmd::Guide => {
            print!("{}", include_str!("../docs/AGENTS.md"));
            Ok(0)
        }

        Cmd::Rm { name } => {
            if store.remove_server(&name)? {
                eprintln!("removed {name}");
                Ok(0)
            } else {
                Err(Error::usage(format!("no server named {name:?}")))
            }
        }

        Cmd::Ls { no_probe } => {
            if no_probe {
                let servers = store.servers()?;
                let creds = store.credentials()?;
                if cli.json {
                    let rows: Vec<Value> = servers
                        .iter()
                        .map(|(n, c)| {
                            json!({
                                "name": n, "kind": c.kind(), "location": c.location(),
                                "headers": c.headers, "token_env": c.token_env,
                                "credential": creds.get(n).map(|c| c.has_token()).unwrap_or(false),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows).unwrap());
                } else {
                    let rows: Vec<Vec<String>> = servers
                        .iter()
                        .map(|(n, c)| {
                            let auth = if c.token_env.is_some() {
                                format!("${}", c.token_env.as_deref().unwrap())
                            } else if creds.get(n).is_some_and(|c| c.has_token()) {
                                "saved".into()
                            } else {
                                "-".into()
                            };
                            vec![n.clone(), c.kind().into(), auth, c.location().into()]
                        })
                        .collect();
                    print_table(&["NAME", "TYPE", "AUTH", "LOCATION"], &rows);
                }
                return Ok(0);
            }
            let probes = client::probe_all(&store, &opts, true)?;
            if cli.json {
                let rows: Vec<Value> = probes
                    .iter()
                    .map(|p| {
                        let mut v = serde_json::to_value(p).unwrap();
                        if let Some(t) = &p.tools {
                            v["tools"] = json!(t.len());
                        }
                        v
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else if probes.is_empty() {
                eprintln!("no servers saved yet. Try: mcpdial add wiki --http https://mcp.deepwiki.com/mcp");
            } else {
                let rows: Vec<Vec<String>> = probes
                    .iter()
                    .map(|p| {
                        vec![
                            p.name.clone(),
                            p.kind.into(),
                            p.status.label(),
                            auth_label(p),
                            p.server
                                .clone()
                                .or_else(|| p.status.detail().map(truncate))
                                .unwrap_or_else(|| "-".into()),
                            p.tools
                                .as_ref()
                                .map(|t| t.len().to_string())
                                .unwrap_or_else(|| "-".into()),
                        ]
                    })
                    .collect();
                print_table(
                    &["NAME", "TYPE", "STATUS", "AUTH", "SERVER", "TOOLS"],
                    &rows,
                );
            }
            Ok(0)
        }

        Cmd::Tools { target: None, long } => {
            let probes = client::probe_all(&store, &opts, true)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&probes).unwrap());
                return Ok(0);
            }
            if probes.is_empty() {
                eprintln!("no servers saved yet. Try: mcpdial add wiki --http https://mcp.deepwiki.com/mcp");
                return Ok(0);
            }
            for (i, p) in probes.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                match &p.tools {
                    Some(tools) => {
                        println!(
                            "## {}  {}  ({} tools)",
                            p.name,
                            p.server.as_deref().unwrap_or(""),
                            tools.len()
                        );
                        print_tools(tools, long);
                    }
                    None => println!(
                        "## {}  {}{}",
                        p.name,
                        p.status.label(),
                        p.status
                            .detail()
                            .map(|d| format!(": {}", truncate(d)))
                            .unwrap_or_default()
                    ),
                }
            }
            Ok(0)
        }

        Cmd::Tools {
            target: Some(target),
            long,
        } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let tools = conn.session.list_tools()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "tools": tools })).unwrap()
                );
            } else {
                println!("{} tool(s):\n", tools.len());
                print_tools(&tools, long);
            }
            Ok(0)
        }

        Cmd::Info { target } => {
            let r = client::resolve(&store, &target)?;
            let conn = client::connect(&store, &r, &opts)?;
            let init = &conn.server_info;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(init).unwrap());
            } else {
                let si = &init["serverInfo"];
                println!(
                    "{} {}",
                    si["name"].as_str().unwrap_or("?"),
                    si["version"].as_str().unwrap_or("")
                );
                println!(
                    "protocol {}",
                    init["protocolVersion"].as_str().unwrap_or("?")
                );
                let caps: Vec<&str> = init["capabilities"]
                    .as_object()
                    .map(|o| o.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                println!(
                    "capabilities: {}",
                    if caps.is_empty() {
                        "(none)".into()
                    } else {
                        caps.join(", ")
                    }
                );
                if let Some(instr) = init["instructions"].as_str() {
                    println!("\n{}", instr.trim());
                }
            }
            Ok(0)
        }

        Cmd::Call {
            target,
            tool,
            arguments,
        } => {
            let arguments = read_json_arg(&arguments, "arguments")?;
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let result = conn.session.call_tool(&tool, arguments)?;
            let is_error = result["isError"].as_bool().unwrap_or(false);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
            } else {
                let text = mcpdial::render_content(&result);
                if !text.is_empty() {
                    println!("{text}");
                }
            }
            Ok(if is_error { EXIT_ERROR } else { 0 })
        }

        Cmd::Raw {
            target,
            method,
            params,
        } => {
            let params = read_json_arg(&params, "params")?;
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let result = conn.session.request(&method, Some(params))?;
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
            Ok(0)
        }

        Cmd::Login {
            target,
            scope,
            port,
            client_id,
            redirect_host,
            no_browser,
        } => {
            let r = client::resolve(&store, &target)?;
            let Some(url) = r.config.http.clone() else {
                return Err(Error::usage(
                    "login only applies to HTTP servers; stdio servers need no token",
                ));
            };
            let existing = store.credential(&r.name)?;
            let http = oauth::Http::new(opts.timeout, Some(opts.user_agent.clone()));
            let login_opts = oauth::LoginOptions {
                scope,
                port,
                client_id,
                redirect_host,
                open_browser: !no_browser,
                timeout: Duration::from_secs(300),
            };
            let cred = oauth::login(&http, &url, existing.as_ref(), &login_opts, |line| {
                eprintln!("{line}")
            })?;
            store.save_credential(&r.name, cred.clone())?;
            eprintln!(
                "saved token for {} ({}{})",
                r.name,
                expiry_label(&cred),
                if cred.refresh_token.is_some() {
                    ", refreshable"
                } else {
                    ""
                }
            );
            Ok(0)
        }

        Cmd::Logout { target } | Cmd::Token(TokenCmd::Rm { name: target }) => {
            let name = client::resolve(&store, &target)
                .map(|r| r.name)
                .unwrap_or(target);
            if store.remove_credential(&name)? {
                eprintln!("removed credential for {name}");
            } else {
                eprintln!("no credential saved for {name}");
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Set { name, env }) => {
            let key = client::resolve(&store, &name)
                .map(|r| r.name)
                .unwrap_or(name);
            let token = match env {
                Some(var) => std::env::var(&var)
                    .ok()
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| Error::usage(format!("${var} is unset or empty")))?,
                None => {
                    let mut stdin = std::io::stdin();
                    if stdin.is_terminal() {
                        eprint!("paste the token and press enter: ");
                        std::io::stderr().flush().ok();
                    }
                    let mut buf = String::new();
                    stdin
                        .read_to_string(&mut buf)
                        .map_err(|e| Error::usage(e.to_string()))?;
                    let t = buf.trim().to_string();
                    if t.is_empty() {
                        return Err(Error::usage("no token on stdin"));
                    }
                    t
                }
            };
            let mut cred = store.credential(&key)?.unwrap_or_default();
            cred.access_token = Some(token);
            cred.expires_at = None;
            cred.source = Some("manual".into());
            store.save_credential(&key, cred)?;
            eprintln!("saved token for {key}");
            Ok(0)
        }

        Cmd::Token(TokenCmd::Show { name }) => {
            let key = client::resolve(&store, &name)
                .map(|r| r.name)
                .unwrap_or(name);
            let Some(cred) = store.credential(&key)? else {
                eprintln!("no credential saved for {key}");
                return Ok(EXIT_ERROR);
            };
            if cli.json {
                // Metadata only. The secrets never leave the file through this path.
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "name": key,
                        "has_access_token": cred.has_token(),
                        "has_refresh_token": cred.refresh_token.is_some(),
                        "expires_at": cred.expires_at,
                        "expired": cred.is_expired(),
                        "scope": cred.scope,
                        "source": cred.source,
                        "client_id": cred.client_id,
                        "token_endpoint": cred.token_endpoint,
                    }))
                    .unwrap()
                );
            } else {
                println!("{key}");
                println!(
                    "  access token:  {}",
                    if cred.has_token() { "present" } else { "none" }
                );
                println!("  expiry:        {}", expiry_label(&cred));
                println!(
                    "  refresh token: {}",
                    if cred.refresh_token.is_some() {
                        "present"
                    } else {
                        "none"
                    }
                );
                println!("  source:        {}", cred.source.as_deref().unwrap_or("?"));
                if let Some(s) = &cred.scope {
                    println!("  scope:         {s}");
                }
                if let Some(c) = &cred.client_id {
                    println!("  client id:     {c}");
                }
                if let Some(t) = &cred.token_endpoint {
                    println!("  token url:     {t}");
                }
            }
            Ok(0)
        }
    }
}

/// Where hosts keep their `mcpServers` config, most specific first.
fn import_candidates() -> Vec<std::path::PathBuf> {
    let mut out = vec![std::path::PathBuf::from(".mcp.json")];
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        out.push(home.join(".claude.json"));
        out.push(home.join(".cursor/mcp.json"));
        out.push(home.join(".codeium/windsurf/mcp_config.json"));
        if cfg!(target_os = "macos") {
            out.push(home.join("Library/Application Support/Claude/claude_desktop_config.json"));
        } else if cfg!(target_os = "windows") {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                out.push(
                    std::path::PathBuf::from(appdata).join("Claude/claude_desktop_config.json"),
                );
            }
        } else {
            out.push(home.join(".config/Claude/claude_desktop_config.json"));
        }
    }
    out
}

fn auth_label(p: &Probe) -> String {
    match (p.auth, &p.status) {
        (client::AuthUsed::Env, _) => "env".into(),
        (client::AuthUsed::Saved, _) => "saved".into(),
        (client::AuthUsed::None, Status::AuthRequired) => "needed".into(),
        (client::AuthUsed::None, Status::TokenRejected) => "rejected".into(),
        (client::AuthUsed::None, _) => "-".into(),
    }
}

fn expiry_label(cred: &Credential) -> String {
    match cred.expires_at {
        None => "no expiry recorded".into(),
        Some(t) => {
            let now = mcpdial::config::now();
            if t <= now {
                "expired".into()
            } else {
                let secs = t - now;
                if secs >= 86_400 {
                    format!("expires in {}d", secs / 86_400)
                } else if secs >= 3600 {
                    format!("expires in {}h", secs / 3600)
                } else {
                    format!("expires in {}m", (secs / 60).max(1))
                }
            }
        }
    }
}

fn truncate(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() > 60 {
        format!("{}...", first.chars().take(57).collect::<String>())
    } else {
        first.to_string()
    }
}

fn print_tools(tools: &[Value], long: bool) {
    for t in tools {
        let name = t["name"].as_str().unwrap_or("?");
        let desc = t["description"].as_str().unwrap_or("").trim();
        if long {
            println!("{name}");
            for line in desc.lines() {
                println!("    {}", line.trim_end());
            }
            let params = describe_params(t);
            if !params.is_empty() {
                println!("  parameters:");
                for p in params {
                    println!("    {p}");
                }
            }
            println!();
        } else {
            let first = desc.lines().next().unwrap_or("");
            println!("  {name:<28} {}", truncate_at(first, 90));
        }
    }
}

fn truncate_at(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n - 3).collect::<String>())
    } else {
        s.to_string()
    }
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == cols {
                out.push_str(c);
            } else {
                out.push_str(&format!("{:<w$}  ", c, w = widths[i]));
            }
        }
        out.trim_end().to_string()
    };
    println!("{}", line(headers.to_vec()));
    for row in rows {
        println!("{}", line(row.iter().map(String::as_str).collect()));
    }
}
