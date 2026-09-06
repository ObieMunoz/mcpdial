use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use complete::ShellHelper;
use mcpdial::catalog;
use mcpdial::client::{self, describe_params, Listing, Options, Status};
use mcpdial::config::Source;
use mcpdial::protocol::{is_not_found, INVALID_PARAMS, METHOD_NOT_FOUND};
use mcpdial::registry::{Pick, Registry, Resolved};
use mcpdial::serve;
use mcpdial::session::{
    extension_for, render_content, render_messages, render_resource, resource_bodies, save_media,
    Media, MediaSink,
};
use mcpdial::transport::trace::Trace;
use mcpdial::{
    daemon, oauth, Credential, Elicit, Error, KnownVersion, Level, ServerConfig, Store, USER_AGENT,
};
use notices::Notices;
use output::{As, Output, Payload};
use present::style::ColorMode;
use present::{truncate_at, Presenter};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

mod args;
mod brief;
mod browse;
mod complete;
mod env_defaults;
mod grep;
mod notices;
mod output;
mod present;
mod snapshot;

const EXIT_ERROR: u8 = 1; // the server said no: JSON-RPC error, HTTP error, or tool isError
const EXIT_USAGE: u8 = 2; // bad arguments or config; nothing was sent
const EXIT_DRIFT: u8 = 3; // --check: the server no longer matches the snapshot

/// The positionals more than one subcommand takes, worded once so they cannot
/// drift apart.
const TARGET_HELP: &str = "A saved name, an http(s):// URL, or stdio:<command>";
const SAVED_NAME_HELP: &str = "A saved server (`mcpdial ls` lists them)";
const CREDENTIAL_TARGET_HELP: &str = "A saved name, or the http(s):// URL a token was saved for";

/// Dial any MCP server from the shell. No SDK, no host app, no connector.
///
/// TARGET is a saved server name, an http(s):// URL, or stdio:<command>.
#[derive(Parser)]
#[command(
    name = "mcpdial",
    version,
    about,
    next_help_heading = "Global options",
    after_help = "\
examples:
  mcpdial add wiki --http https://mcp.deepwiki.com/mcp
  mcpdial add fs --stdio \"npx -y @modelcontextprotocol/server-filesystem /tmp\"
  mcpdial catalog                 # a reviewed list of servers, by category
  mcpdial add ctx7 --catalog context7                      # one of them
  mcpdial search browser automation                        # the whole registry, ranked
  mcpdial add ctx7 --registry io.github.upstash/context7   # from the MCP registry
  mcpdial ls                      # every saved server with its status and its age
  mcpdial tools                   # every tool on every server
  mcpdial login work              # one-time browser step; the token is saved
  mcpdial call wiki read_wiki_structure '{\"repoName\":\"modelcontextprotocol/servers\"}'
  mcpdial call https://mcp.deepwiki.com/mcp read_wiki_structure '{\"repoName\":\"x/y\"}'
  mcpdial tools 'stdio:npx -y @modelcontextprotocol/server-everything stdio'
  mcpdial completions SHELL       # bash, zsh, fish, elvish, powershell

calling from a program or an agent: pass --json everywhere and run `mcpdial guide`."
)]
struct Cli {
    /// Seconds to wait for a reply; else the server's saved timeout, else 60. With `add`, saved.
    /// (MCPDIAL_TIMEOUT=SECS does the same)
    #[arg(long, global = true, value_name = "SECS")]
    timeout: Option<f64>,

    /// Emit JSON instead of a readable summary (MCPDIAL_JSON=1 does the same)
    #[arg(long, global = true)]
    json: bool,

    /// Print what a pipe would get, even at a terminal (MCPDIAL_PLAIN=1 does the same)
    #[arg(long, global = true)]
    plain: bool,

    /// Print long output straight to the terminal instead of through a pager
    /// (MCPDIAL_PAGER= does the same everywhere)
    #[arg(long, global = true)]
    no_pager: bool,

    /// Print a server's text as it was written instead of rendering its
    /// markdown, for copying it out of the terminal
    #[arg(long, global = true)]
    raw: bool,

    /// Colour the terminal output: `always` (for `less -R`), `never`, or `auto`
    /// (at a terminal, unless NO_COLOR is set)
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    color: ColorMode,

    /// Trace every message on stderr (and pass a stdio server's stderr through)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Print a line on stderr for every progress notification a server sends
    /// during a call, whether or not stderr is a terminal
    #[arg(long, global = true)]
    progress: bool,

    /// Show a server's own log notifications from this level up (default
    /// warning), and ask a server that supports logging to send no less
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<Level>,

    /// Append every message and transport event to FILE as JSON Lines, secrets
    /// redacted (MCPDIAL_TRACE=FILE does the same everywhere)
    #[arg(long, global = true, value_name = "FILE")]
    trace: Option<PathBuf>,

    /// Override the User-Agent (HTTP only) (MCPDIAL_USER_AGENT does the same)
    #[arg(long, global = true)]
    user_agent: Option<String>,

    /// Extra HTTP header, repeatable. With `add`, saved to the server.
    #[arg(
        short = 'H',
        long = "header",
        global = true,
        value_name = "'Name: value'"
    )]
    headers: Vec<String>,

    /// Env var holding a bearer token; beats any saved credential. With `add`, saved.
    /// A `${VAR}` in any `-H` header value does the same for that header.
    #[arg(long, global = true, value_name = "VAR")]
    token_env: Option<String>,

    /// MCP protocol revision to speak, instead of working out which the server does.
    /// With `add`, saved.
    #[arg(long, global = true, value_name = "VERSION")]
    protocol_version: Option<KnownVersion>,
    /// Fail on the first transient HTTP failure instead of sending the request once more
    #[arg(long, global = true)]
    no_retry: bool,

    /// Write image, audio and blob blocks to DIR/<tool>-<n>.<ext> instead of
    /// printing them (call, prompt, read, shell)
    #[arg(long, global = true, value_name = "DIR")]
    save_dir: Option<PathBuf>,

    /// Show at most N characters of a result; under --json a `truncated` object
    /// replaces it (call, prompt, read, raw, shell; MCPDIAL_MAX_CHARS=N does the same)
    #[arg(long, global = true, value_name = "N")]
    max_chars: Option<usize>,

    /// Write the whole result to FILE, which must not exist, and print one line
    /// saying so (call, prompt, read, raw, shell)
    #[arg(short = 'o', long, global = true, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Dial a stdio server afresh even when `start` left one running
    /// (MCPDIAL_NO_DAEMON=1 does the same everywhere)
    #[arg(long, global = true)]
    no_daemon: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save a server under a name
    Add {
        /// The name to save it under, and to dial it by from then on
        name: String,
        /// Streamable HTTP endpoint
        #[arg(
            long,
            value_name = "URL",
            conflicts_with_all = ["stdio", "registry", "catalog"],
            required_unless_present_any = ["stdio", "registry", "catalog"]
        )]
        http: Option<String>,
        /// An entry of the curated catalog, by its id (`mcpdial catalog` lists them)
        #[arg(long, value_name = "ID", conflicts_with_all = ["stdio", "registry"])]
        catalog: Option<String>,
        /// Command that speaks MCP on stdio
        #[arg(long, value_name = "CMD", conflicts_with = "registry")]
        stdio: Option<String>,
        /// A server in the MCP registry, by its registry name (io.github.owner/server)
        #[arg(long, value_name = "NAME")]
        registry: Option<String>,
        /// Run this package type from the registry entry rather than the first offered
        #[arg(
            long,
            value_name = "TYPE",
            value_parser = ["npm", "pypi", "oci"],
            requires = "registry",
            conflicts_with = "remote"
        )]
        package: Option<String>,
        /// Use the registry entry's remote endpoint rather than a package
        #[arg(long, requires = "registry")]
        remote: bool,
        /// A value the registry entry leaves to you, repeatable, in the order listed
        #[arg(long, value_name = "VALUE", requires = "registry")]
        arg: Vec<String>,
        /// Environment variable for the stdio process, repeatable
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Working directory for the stdio process
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Offer only tools matching this glob (`*`, `?`), repeatable
        #[arg(long, value_name = "PATTERN")]
        allow: Vec<String>,
        /// Hide and refuse tools matching this glob, repeatable; beats --allow
        #[arg(long, value_name = "PATTERN")]
        deny: Vec<String>,
        /// Replace a server already saved under this name
        #[arg(long)]
        force: bool,
        /// Save without dialing the server for its status
        #[arg(long)]
        no_probe: bool,
    },
    /// Change a saved server's tool allow and deny lists, or show them
    Set {
        #[arg(help = SAVED_NAME_HELP)]
        name: String,
        /// Replace the allow list with these globs (`*`, `?`), repeatable
        #[arg(long, value_name = "PATTERN", conflicts_with = "clear_allow")]
        allow: Vec<String>,
        /// Replace the deny list with these globs, repeatable; beats --allow
        #[arg(long, value_name = "PATTERN", conflicts_with = "clear_deny")]
        deny: Vec<String>,
        /// Remove the allow list, so every tool not denied is offered
        #[arg(long)]
        clear_allow: bool,
        /// Remove the deny list
        #[arg(long)]
        clear_deny: bool,
    },
    /// Search the MCP registry, ranked, over a local copy of its whole list
    Search {
        /// Words that must all appear in an entry's name, title or description
        query: Vec<String>,
        /// How many matches to show
        #[arg(long, default_value_t = 20, value_name = "N")]
        limit: usize,
        /// Fetch the whole list again, even if the local copy is recent
        #[arg(long, conflicts_with = "offline")]
        refresh: bool,
        /// Search the local copy as it is, without touching the network
        #[arg(long)]
        offline: bool,
    },
    /// Import servers from a host's config (Claude, Cursor, Windsurf, VS Code, Codex, OpenCode)
    Import {
        /// A host's config: JSON with an `mcpServers`, `servers` (VS Code) or `mcp`
        /// (OpenCode) object, or Codex's `config.toml`. Omit to scan the usual locations.
        file: Option<std::path::PathBuf>,
        /// Scan one host's locations only: vscode, codex, opencode, claude, cursor or windsurf
        #[arg(long, value_name = "HOST", conflicts_with = "file")]
        from: Option<mcpdial::import_config::Host>,
        /// Overwrite servers that already exist under the same name
        #[arg(long)]
        force: bool,
    },
    /// Write saved servers out in a host's own shape, to stdout for you to redirect
    Export {
        /// Servers to export; every saved server when none is named
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Shape to write: mcpservers (Claude, Cursor, Windsurf), vscode or codex
        #[arg(long, default_value = "mcpservers", value_name = "FORMAT")]
        format: mcpdial::export_config::Format,
        /// Print this host file with the exported servers merged in; it is never written
        #[arg(long, value_name = "FILE")]
        merge: Option<std::path::PathBuf>,
    },
    /// List the curated catalog of servers, grouped by category
    Catalog {
        /// Use the copy built into the binary instead of refreshing it
        #[arg(long)]
        offline: bool,
    },
    /// Tick catalog servers to save and dial; the ones already saved start ticked
    Browse {
        /// The whole MCP registry instead of the catalog (fzf filters it)
        #[arg(long)]
        all: bool,
        /// Use the copies on disk without refreshing them
        #[arg(long)]
        offline: bool,
        /// What fzf's preview pane shows for one entry
        #[arg(long, value_name = "ID", hide = true)]
        preview: Option<String>,
    },
    /// Keep one session open and run commands from stdin (state persists between calls)
    Shell {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Print a url-mode elicitation's address instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Keep a stdio server running in the background; later commands share its session
    Start {
        /// A saved stdio server to hold open
        name: String,
        /// Exit after this many seconds with no caller (default: never)
        #[arg(long, value_name = "SECS")]
        idle: Option<f64>,
    },
    /// End a server kept running by `start`
    Stop {
        /// The saved stdio server whose background session to end
        name: String,
    },
    /// Run a server's daemon in this process (what `start` runs in the background)
    #[command(hide = true)]
    Daemon {
        name: String,
        #[arg(long, value_name = "SECS")]
        idle: Option<f64>,
    },
    /// Forget a server and any credential saved for it
    Rm {
        #[arg(help = SAVED_NAME_HELP)]
        name: String,
    },
    /// List saved servers with their connection status
    Ls {
        /// Do not connect; just show the configuration
        #[arg(long)]
        no_probe: bool,
        /// Dial every server again instead of reusing a status from the last few minutes
        #[arg(long, conflicts_with = "no_probe")]
        refresh: bool,
    },
    /// Show the tools a server offers (every server when no target is given)
    Tools {
        /// A saved name, an http(s):// URL, or stdio:<command>; every saved server when omitted
        target: Option<String>,
        /// Show full descriptions and parameters
        #[arg(short, long)]
        long: bool,
        /// Include the tools the server's allow and deny lists hide, marked (denied)
        #[arg(long, requires = "target")]
        all: bool,
        /// Write the tools in full to FILE, which must not exist, instead of listing them
        #[arg(long, value_name = "FILE", requires = "target", conflicts_with_all = ["long", "all"])]
        snapshot: Option<PathBuf>,
        /// Report how the tools differ from a snapshot; exit 3 when a caller would break
        #[arg(long, value_name = "FILE", requires = "target",
              conflicts_with_all = ["snapshot", "long", "all"])]
        check: Option<PathBuf>,
        /// With --check: hold every snapshotted tool to the object the snapshot holds
        #[arg(long, requires = "check")]
        strict: bool,
    },
    /// Search tools, resources, prompts and instructions across saved servers
    Grep(grep::Flags),
    /// Initialize and show server identity and capabilities
    Info {
        #[arg(help = TARGET_HELP)]
        target: String,
    },
    /// Call a tool
    Call {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// A tool name from `mcpdial tools TARGET`
        tool: String,
        /// One JSON object (inline, @file, or - for stdin), or key=value pairs
        arguments: Vec<String>,
        /// Answers for anything the server elicits mid-call: a JSON object or @file
        #[arg(long, value_name = "JSON")]
        elicit: Option<String>,
        /// Print a url-mode elicitation's address instead of opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Refuse to call when TOOL has drifted from this snapshot; exit 3 with the differences
        #[arg(long, value_name = "FILE")]
        check: Option<PathBuf>,
        /// With --check: hold TOOL to the object the snapshot holds
        #[arg(long, requires = "check")]
        strict: bool,
    },
    /// Show one tool's name, description, and input and output schemas
    Schema {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// A tool name from `mcpdial tools TARGET`
        tool: String,
    },
    /// Show the resources a server offers, and its URI templates
    Resources {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Show full descriptions and mime types
        #[arg(short, long)]
        long: bool,
    },
    /// Read one resource. Text goes to stdout; binary needs a redirect or --save-dir
    Read {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// A resource URI from `mcpdial resources TARGET`, or a template expanded yourself
        uri: String,
    },
    /// Show the prompts a server offers
    Prompts {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Show full descriptions and arguments
        #[arg(short, long)]
        long: bool,
    },
    /// Render a prompt into the messages it expands to
    Prompt {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// A prompt name from `mcpdial prompts TARGET`
        name: String,
        /// One JSON object (inline, @file, or - for stdin), or key=value pairs
        arguments: Vec<String>,
        /// Answers for anything the server elicits mid-call: a JSON object or @file
        #[arg(long, value_name = "JSON")]
        elicit: Option<String>,
        /// Print a url-mode elicitation's address instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Send any JSON-RPC method; a saved server's allow and deny lists do not apply
    Raw {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// A JSON-RPC method name, such as tools/list
        method: String,
        /// JSON object of params: inline, @file, or - for stdin
        #[arg(default_value = "{}")]
        params: String,
    },
    /// Expose a saved server to a client that must not see its credentials
    Serve {
        #[arg(help = TARGET_HELP)]
        target: String,
        /// Address to listen on; port 0 picks a free one and reports it
        #[arg(
            long,
            value_name = "ADDR",
            default_value = "127.0.0.1:0",
            conflicts_with = "stdio"
        )]
        listen: String,
        /// Speak MCP on this process's own stdin and stdout instead of listening
        #[arg(long)]
        stdio: bool,
        /// Allow --listen on an address other than loopback
        #[arg(long, conflicts_with = "stdio")]
        listen_any: bool,
        /// Env var holding the bearer token clients must present
        #[arg(long, value_name = "VAR", conflicts_with = "stdio")]
        bearer_env: Option<String>,
        /// Expose only tools matching this glob, on top of the server's own lists
        #[arg(long, value_name = "PATTERN")]
        allow: Vec<String>,
        /// Never expose tools matching this glob, repeatable; beats --allow
        #[arg(long, value_name = "PATTERN")]
        deny: Vec<String>,
    },
    /// Print the usage guide written for programs and agents that call mcpdial
    Guide,
    /// Print a shell completion script for bash, zsh, fish, elvish or powershell
    #[command(hide = true)]
    Completions {
        /// The shell to write the script for
        shell: Shell,
    },
    /// Authorize in the browser once and save the token (HTTP servers)
    Login {
        /// A saved name, or an http(s):// URL
        target: String,
        /// authorization-code (a browser, once) or client-credentials (a confidential
        /// client's --client-id and secret, no human)
        #[arg(
            long,
            value_name = "GRANT",
            default_value = "authorization-code",
            value_parser = ["authorization-code", "client-credentials"]
        )]
        grant: String,
        /// Space-separated scopes (default: whatever the server advertises)
        #[arg(long)]
        scope: Option<String>,
        /// Fixed loopback port for the redirect (default: any free port)
        #[arg(long)]
        port: Option<u16>,
        /// Use a pre-registered client id instead of a client metadata document or
        /// dynamic registration
        #[arg(long)]
        client_id: Option<String>,
        /// Present this client ID metadata document as the client id, instead of the
        /// one the project publishes, whether or not the server advertises support
        #[arg(long, value_name = "URL", conflicts_with_all = ["client_id", "no_client_metadata"])]
        client_metadata_url: Option<String>,
        /// Register dynamically even when the server accepts client metadata documents
        #[arg(long)]
        no_client_metadata: bool,
        /// Read that client's secret from stdin. Never from an argument.
        #[arg(long, requires = "client_id")]
        client_secret: bool,
        /// Read that client's secret from $VAR instead of stdin
        #[arg(
            long,
            value_name = "VAR",
            requires = "client_id",
            conflicts_with = "client_secret"
        )]
        client_secret_env: Option<String>,
        /// Loopback host in the redirect URI: 127.0.0.1 (default, with a localhost
        /// fallback if the server refuses it) or localhost
        #[arg(long, value_name = "HOST", value_parser = ["127.0.0.1", "localhost"])]
        redirect_host: Option<String>,
        /// Print the URL but do not try to open a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Delete the saved credential for a server
    Logout {
        #[arg(help = CREDENTIAL_TARGET_HELP)]
        target: String,
    },
    /// Manage saved tokens without the browser
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Save a token read from stdin, or from --env VAR. Never from an argument.
    Set {
        #[arg(help = CREDENTIAL_TARGET_HELP)]
        name: String,
        #[arg(long, value_name = "VAR")]
        env: Option<String>,
    },
    /// Describe the saved credential without revealing it
    Show {
        #[arg(help = CREDENTIAL_TARGET_HELP)]
        name: String,
    },
    /// Delete the saved credential
    Rm {
        #[arg(help = CREDENTIAL_TARGET_HELP)]
        name: String,
    },
}

fn main() -> ExitCode {
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // A bare `mcpdial` at a terminal with nothing saved yet opens the
        // checklist; anything else gets clap's help, as ever.
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let mut browse = Cli::parse_from(["mcpdial", "browse"]);
            // MCPDIAL_JSON in the environment is a program asking, not a person.
            let _ = env_defaults::apply(&mut browse);
            if browse::first_run(&browse) {
                browse
            } else {
                e.exit()
            }
        }
        Err(e) => e.exit(),
    };
    let defaults = env_defaults::apply(&mut cli);
    let ui = <dyn Presenter>::choose(&cli);
    let json = cli.json;
    match defaults
        .map_err(Failure::from)
        .and_then(|()| run(ui.as_ref(), cli))
    {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            if json {
                ui.err_line(&f.to_json().to_string());
            } else {
                f.report(ui.as_ref());
            }
            ExitCode::from(match f.error {
                Error::Usage(_) | Error::Config(_) => EXIT_USAGE,
                _ => EXIT_ERROR,
            })
        }
    }
}

/// A command that failed, plus an optional hint that spells out what was
/// expected instead. The hint is a second block of prose for a human and an
/// `error.hint` string under `--json`, so neither has to guess a tool's shape.
struct Failure {
    error: Error,
    hint: Option<String>,
    /// The tool the error is about, as `error.tool` under `--json`.
    tool: Option<String>,
}

impl Failure {
    fn hinted(error: Error, hint: impl Into<String>) -> Self {
        Self {
            error,
            hint: Some(hint.into()),
            tool: None,
        }
    }

    fn report(&self, ui: &dyn Presenter) {
        ui.error(&self.error.to_string(), self.hint.as_deref());
    }

    fn to_json(&self) -> Value {
        let mut v = error_json(&self.error);
        if let Some(hint) = &self.hint {
            v["error"]["hint"] = json!(hint);
        }
        if let Some(tool) = &self.tool {
            v["error"]["tool"] = json!(tool);
        }
        v
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            hint: None,
            tool: None,
        }
    }
}

/// The refusal a saved server's allow and deny lists make before anything is
/// sent for `tool`; `Ok` when they permit it.
fn refuse_denied(cfg: &ServerConfig, name: &str, tool: &str) -> Result<(), Failure> {
    cfg.refuse_denied(name, tool).map_err(|error| Failure {
        error,
        hint: None,
        tool: Some(tool.to_string()),
    })
}

fn print_json(ui: &dyn Presenter, v: &impl serde::Serialize) {
    ui.json(&serde_json::to_string_pretty(v).expect("a JSON value is serializable"));
}

fn print_value(ui: &dyn Presenter, v: &Value, compact: bool) {
    ui.json(&output::document(v, compact));
}

/// A hint on stderr: prose for a human, `{"hint": ...}` under `--json`, where
/// every line on either stream has to be an object.
fn print_hint(ui: &dyn Presenter, hint: &str, json: bool) {
    if json {
        ui.err_line(&json!({ "hint": hint }).to_string());
    } else {
        ui.err_line(hint);
    }
}

/// A tool result on stdout, and a marker on stderr when the tool reported an
/// error: a failure whose text is a plain sentence otherwise reads as success
/// to anyone not checking `$?`. Under `--json` the object carries `isError`
/// itself. Returns whether the tool reported an error.
fn print_tool_result(
    ui: &dyn Presenter,
    out: &Output,
    result: &Value,
    text: &str,
    json: bool,
    one_line: bool,
) -> Result<bool, Failure> {
    let failed = result["isError"].as_bool().unwrap_or(false);
    let payload = if json {
        Payload::Json {
            value: result,
            one_line,
        }
    } else {
        Payload::Text(text)
    };
    let sent = out.deliver(payload, failed)?;
    output::show(ui, sent, shape(json), json, || {
        if json {
            print_value(ui, result, one_line);
        } else if !text.is_empty() {
            ui.text(text);
        }
    });
    if !json && failed {
        ui.err_line("(tool reported an error)");
    }
    Ok(failed)
}

/// A result goes out as a document under `--json` and as the server's own text
/// otherwise, cut or whole.
fn shape(json: bool) -> As {
    if json {
        As::Json
    } else {
        As::Text
    }
}

/// One request with somebody listening to what the server says on the way, and
/// the updating line finished before whatever prints next, however it went.
fn watched<'a, T>(
    notices: &mut Notices<'a>,
    request: impl FnOnce(&mut Notices<'a>) -> Result<T, Error>,
) -> Result<T, Error> {
    let outcome = request(notices);
    notices.finish();
    outcome
}

/// What this invocation can answer when a server elicits mid-call.
///
/// Nothing here may wait on a person who is not there. Without a terminal on
/// both stdin and stderr, or under `--json`, where the output is being parsed
/// rather than read, a form elicitation is declined the moment it arrives and
/// the call carries on.
fn elicitation(
    answers: Option<&str>,
    no_browser: bool,
    json: bool,
    shell: bool,
) -> Result<Elicit, Error> {
    let answers = answers
        .map(|text| read_json_arg(text, "elicit answers"))
        .transpose()?
        .and_then(|v| v.as_object().cloned());
    let at_a_terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    Ok(Elicit {
        answers,
        ask: at_a_terminal && !json,
        later: shell,
        browser: !no_browser,
        json,
    })
}

fn dial(store: &Store, opts: &Options, target: &str) -> Result<client::Connection, Failure> {
    let r = client::resolve(store, target)?;
    Ok(client::connect(store, &r, opts)?)
}

fn info_hint(target: &str) -> String {
    format!("`mcpdial info {}`", shell_word(target))
}

/// Where to look for the resource or prompt the server did have.
fn resources_hint(target: &str) -> String {
    format!("`mcpdial resources {}`", shell_word(target))
}

fn prompts_hint(target: &str) -> String {
    format!("`mcpdial prompts {} --long`", shell_word(target))
}

/// The config a registry entry describes, with every value it needs in hand.
/// Nothing is run: the command line is built, not tried.
fn from_registry(
    opts: &Options,
    entry: &str,
    pick: &Pick,
    args: &[String],
) -> Result<Resolved, Failure> {
    const NAME_HINT: &str =
        "a registry name is the entry's own `name`, like io.github.owner/server";
    if !entry.contains('/') {
        return Err(Failure::hinted(
            Error::usage(format!("{entry:?} is not a registry name")),
            NAME_HINT,
        ));
    }
    let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
    let server = registry.latest(entry)?.ok_or_else(|| {
        Failure::hinted(
            Error::usage(format!("the registry has no server named {entry}")),
            NAME_HINT,
        )
    })?;
    let resolved = mcpdial::registry::convert(&server, pick, args)?;
    if !resolved.missing.is_empty() {
        return Err(Failure::hinted(
            Error::usage(format!(
                "{entry} needs {} value(s) this command was not given",
                resolved.missing.len()
            )),
            format!(
                "pass each with --arg VALUE, in this order:\n  {}",
                resolved.missing.join("\n  ")
            ),
        ));
    }
    Ok(resolved)
}

/// The config a catalog entry describes, from the freshest catalog at hand. A
/// registry entry goes through the registry exactly as `--registry` would.
fn from_catalog(
    ui: &dyn Presenter,
    store: &Store,
    opts: &Options,
    id: &str,
) -> Result<Resolved, Failure> {
    let loaded = catalog::load(
        store,
        &catalog::Source::from_env(),
        false,
        opts.timeout_or_default(),
        &opts.user_agent,
    )?;
    if opts.verbose {
        ui.err_line(&format!("catalog: {}", loaded.origin));
    }
    let Some(entry) = catalog::find(&loaded.entries, id) else {
        let ids = loaded.entries.iter().map(|e| e.id.as_str());
        return Err(Failure::hinted(
            Error::usage(format!("no catalog entry named {id:?}")),
            match closest(id, ids) {
                Some(near) => format!("did you mean {near}? `mcpdial catalog` lists them all."),
                None => "`mcpdial catalog` lists every entry with its id.".to_string(),
            },
        ));
    };
    let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
    let resolved = catalog::resolve(entry, &registry)?;
    if !resolved.missing.is_empty() {
        return Err(Failure::hinted(
            Error::config(format!(
                "{id} needs {} value(s) the catalog does not carry",
                resolved.missing.len()
            )),
            format!(
                "add it from the registry instead, passing each with --arg VALUE in this order:\n  mcpdial add NAME --registry {} --arg ...\n  {}",
                entry.registry.as_deref().unwrap_or("?"),
                resolved.missing.join("\n  ")
            ),
        ));
    }
    Ok(resolved)
}

/// The name a credential is filed under, and whether the target names anything
/// mcpdial could dial: a saved server, a URL, or a `stdio:` command line.
fn credential_key(store: &Store, target: String) -> (String, bool) {
    match client::resolve(store, &target) {
        Ok(r) => (r.name, true),
        Err(_) => (target, false),
    }
}

fn name_and_args(rest: &str) -> (&str, &str) {
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
    match args.trim() {
        "" => (name, "{}"),
        args => (name, args),
    }
}

const NO_SERVERS: &str =
    "no servers saved yet. Try: mcpdial add wiki --http https://mcp.deepwiki.com/mcp";

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

/// A secret from `$VAR`, or from stdin. Never from an argument, where `ps` and the
/// shell history would both keep a copy.
fn read_secret(ui: &dyn Presenter, env: Option<&str>, what: &str) -> Result<String, Error> {
    if let Some(var) = env {
        return std::env::var(var)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::usage(format!("${var} is unset or empty")));
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        ui.err(&format!("paste the {what} and press enter: "));
    }
    let mut buf = String::new();
    stdin
        .read_to_string(&mut buf)
        .map_err(|e| Error::usage(e.to_string()))?;
    let secret = buf.trim().to_string();
    if secret.is_empty() {
        return Err(Error::usage(format!("no {what} on stdin")));
    }
    Ok(secret)
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

/// A JSON object, or a usage error that quotes what arrived instead. Anything
/// unquoted from a shell (a URL, a bare word, a pasted markdown link) lands here.
fn parse_object(text: &str, what: &str) -> Result<Value, Error> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(v) => Err(Error::usage(format!(
            "{what} must be a JSON object like {{\"key\": \"value\"}}, not {}",
            json_kind(&v)
        ))),
        Err(e) => Err(Error::usage(match requote_object(text) {
            Some(fixed) => format!(
                "{what} must be a JSON object like {{\"key\": \"value\"}}; {:?} is one with its double quotes missing (did you mean {fixed}?)",
                truncate_at(text, 60)
            ),
            None => format!(
                "{what} must be a JSON object like {{\"key\": \"value\"}}; {:?} is not JSON ({e})",
                truncate_at(text, 60)
            ),
        })),
    }
}

/// `{url:https://x}` back to `{"url": "https://x"}`: the object a shell was most
/// likely handed before it removed the double quotes. Flat objects only, since
/// the quotes were the only thing telling a comma in a value from a separator.
fn requote_object(text: &str) -> Option<String> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    if inner.contains(['{', '[', '"']) {
        return None;
    }
    let fields = inner
        .split(',')
        .map(|pair| {
            let (key, value) = pair.split_once(':')?;
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() || value.is_empty() {
                return None;
            }
            let value = serde_json::from_str::<Value>(value)
                .unwrap_or_else(|_| Value::String(value.to_string()));
            Some(format!("{}: {value}", json!(key)))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("{{{}}}", fields.join(", ")))
}

/// The hint after an argument object failed to parse on the command line: the
/// same command with its quotes back when the shell removed them, else `lookup`,
/// which says where the expected shape is.
fn json_arg_hint(arguments: &str, command: &str, lookup: String) -> String {
    match requote_object(arguments) {
        Some(fixed) => format!(
            "the shell removed the double quotes; single quotes keep them:\n  {command} {}",
            shell_word(&fixed)
        ),
        None => lookup,
    }
}

/// What a JSON value is, for an error message.
fn json_kind(v: &Value) -> &'static str {
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
fn edit_distance(a: &str, b: &str) -> usize {
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
fn closest<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let word = word.to_lowercase();
    let limit = if word.chars().count() <= 4 { 1 } else { 2 };
    candidates
        .map(|c| (edit_distance(&word, &c.to_lowercase()), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn find_tool<'a>(tools: &'a [Value], name: &str) -> Option<&'a Value> {
    tools.iter().find(|t| t["name"] == name)
}

fn field_values(items: &[Value], key: &str) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| i[key].as_str())
        .map(String::from)
        .collect()
}

/// The shape a tool expects: a call that would be well-formed, then one line per
/// parameter. `prefix` is whatever comes before the tool name in that call, and
/// `quote` wraps the argument object, which a shell needs quoted and the REPL does not.
fn tool_usage(tool: &Value, prefix: &str, quote: &str) -> String {
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
fn shell_word(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || "\"'$`\\*?~<>|&;()[]{}#!".contains(c)) {
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        s.to_string()
    }
}

/// What to say about a tool name this server does not have.
fn suggest_tool(tools: &[Value], name: &str, tools_cmd: &str) -> Option<String> {
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
fn call_hint(
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
fn server_refused(e: &Error) -> bool {
    matches!(e, Error::Rpc { .. })
}

/// A server error that the tool's schema would have prevented. Any other error
/// is the tool's own failure, and printing a schema under it is just noise.
///
/// `-32602` is asked to carry more meanings with every revision - 2026-07-28 gave
/// it a missing resource too - so it only settles the question here because the one
/// caller is a `tools/call` that named a tool the server has. A name it does not
/// have is answered by `call_hint` before this is consulted.
fn is_argument_error(e: &Error) -> bool {
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
fn reads_as_argument_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("-32602") || t.contains("invalid arguments") || t.contains("validation error")
}

/// A server that never implemented `resources/*` or `prompts/*` answers `-32601`,
/// which reads as a mistake in the request. Its `initialize` or `server/discover`
/// result already listed what it does implement, so point there instead of at the
/// bare code.
fn missing_capability(e: Error, capability: &str, info_cmd: &str) -> Failure {
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
fn missing_item(e: Error, capability: &str, list_cmd: &str, info_cmd: &str) -> Failure {
    match &e {
        Error::Rpc { code, .. } if is_not_found(*code) => Failure::hinted(
            e,
            format!("{list_cmd} lists the {capability} this server does have."),
        ),
        _ => missing_capability(e, capability, info_cmd),
    }
}

fn advertises(server_info: &Value, capability: &str) -> bool {
    server_info["capabilities"].get(capability).is_some()
}

/// Where a media block's bytes go: a numbered file under `--save-dir`, or nowhere,
/// leaving a placeholder on stdout.
struct MediaFiles<'a> {
    dir: Option<&'a Path>,
    /// The tool, prompt or resource the bytes came from, as a file name.
    stem: String,
}

impl MediaFiles<'_> {
    fn saves(&self) -> bool {
        self.dir.is_some()
    }

    /// The first free `<stem>-<n>.<ext>` in the directory, so a second call keeps
    /// the first one's file rather than writing over it.
    fn place(&self, media: &Media) -> Result<Option<PathBuf>, Error> {
        let Some(dir) = self.dir else {
            return Ok(None);
        };
        let ext = extension_for(&media.mime_type);
        let failed =
            |path: &Path, e: std::io::Error| Error::transport(format!("{}: {e}", path.display()));
        for n in 1.. {
            let path = dir.join(format!("{}-{n}.{ext}", self.stem));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(&media.bytes).map_err(|e| failed(&path, e))?;
                    return Ok(Some(path));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(failed(&path, e)),
            }
        }
        unreachable!("the counter does not run out")
    }
}

/// The sink of a reader with nowhere to put bytes.
fn placeholders(_: &Media) -> Result<Option<PathBuf>, Error> {
    Ok(None)
}

/// A media block's bytes filed where `--save-dir` says, and offered to the
/// presenter, which draws them at a terminal that can show an image and does
/// nothing anywhere else.
fn filed(ui: &dyn Presenter, files: &MediaFiles, media: &Media) -> Result<Option<PathBuf>, Error> {
    let path = files.place(media)?;
    ui.draw(media, path.as_deref());
    Ok(path)
}

/// A tool or prompt name as a file name: anything a shell or a filesystem would
/// argue with becomes `_`.
fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.trim_matches('.').is_empty() {
        "media".to_string()
    } else {
        stem
    }
}

/// A resource's file name from its URI: the last path segment, less its extension.
fn resource_stem(uri: &str) -> String {
    let last = uri
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let stem = match last.rsplit_once('.') {
        Some((before, _)) if !before.is_empty() => before,
        _ => last,
    };
    if stem.is_empty() {
        "resource".to_string()
    } else {
        file_stem(stem)
    }
}

type Render = fn(&Value, &mut MediaSink) -> Result<String, Error>;

/// The text `render` makes of a `tools/call` or `prompts/get` result, with its
/// media handed to `files`. Under `--json` the object itself is what gets printed,
/// so the media moves into it as `path` and `bytes` instead, and the text is only
/// for `call`'s argument-error check.
fn rendered(
    ui: &dyn Presenter,
    result: &mut Value,
    json: bool,
    files: &MediaFiles,
    render: Render,
) -> Result<String, Failure> {
    let mut sink = |m: &Media| filed(ui, files, m);
    if json {
        if files.saves() {
            save_media(result, &mut sink)?;
        }
        // The object goes out as the server sent it; a blob it mangled is the
        // reader's problem, not a reason to fail the command.
        return Ok(render(result, &mut placeholders).unwrap_or_default());
    }
    Ok(render(result, &mut sink)?)
}

/// A `prompts/get` result on stdout: the object under `--json`, the messages
/// otherwise.
fn emit(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    compact: bool,
    files: &MediaFiles,
    render: Render,
) -> Result<(), Failure> {
    let text = rendered(ui, result, json, files, render)?;
    let payload = if json {
        Payload::Json {
            value: result,
            one_line: compact,
        }
    } else {
        Payload::Text(&text)
    };
    let sent = out.deliver(payload, false)?;
    output::show(ui, sent, shape(json), json, || {
        if json {
            print_value(ui, result, compact);
        } else if !text.is_empty() {
            ui.text(&text);
        }
    });
    Ok(())
}

/// A `resources/read` result on stdout: the object under `--json`; with a
/// `--save-dir`, its text and a line per blob filed there; otherwise the bytes
/// themselves, which is the presenter's business.
fn emit_resource(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    compact: bool,
    files: &MediaFiles,
    redirect: &str,
) -> Result<(), Failure> {
    let mut sink = |m: &Media| filed(ui, files, m);
    if json {
        if files.saves() {
            save_media(result, &mut sink)?;
        }
        let sent = out.deliver(
            Payload::Json {
                value: result,
                one_line: compact,
            },
            false,
        )?;
        output::show(ui, sent, As::Json, json, || {
            print_value(ui, result, compact)
        });
    } else if files.saves() {
        let text = render_resource(result, &mut sink)?;
        let sent = out.deliver(Payload::Text(&text), false)?;
        output::show(ui, sent, As::Raw, json, || ui.out(&text));
    } else {
        // Bodies leave byte for byte, so the presenter that refuses binary at a
        // terminal keeps its say - unless `--output` has somewhere to put them.
        let bodies = resource_bodies(result)?;
        let sent = out.deliver_resource(&bodies)?;
        let mut refused = Ok(());
        output::show(ui, sent, As::Raw, json, || {
            refused = ui.resource(&bodies, redirect);
        });
        refused?;
    }
    Ok(())
}

/// Where `add` was told a server lives, checked before anything is written: an
/// http(s) URL with a host, or a command line with at least one word. A mistake
/// here would otherwise surface on the next `ls`, as an unreachable server.
fn validate_location(cfg: &ServerConfig) -> Result<(), Error> {
    // A `${VAR}` is filled in when the server is dialed; what can be checked
    // now is the shape around it.
    let filled = cfg.expanded(|_| Some("var".to_string()))?;
    if let (Some(url), Some(given)) = (&filled.http, &cfg.http) {
        let has_scheme = url.starts_with("http://") || url.starts_with("https://");
        let has_host = url
            .parse::<ureq::http::Uri>()
            .is_ok_and(|u| u.host().is_some_and(|h| !h.is_empty()));
        if !has_scheme || !has_host {
            return Err(Error::usage(format!(
                "--http needs an http:// or https:// URL, got {given:?}"
            )));
        }
    }
    if let Some(cmd) = &filled.stdio {
        mcpdial::transport::stdio::split_command(cmd)?;
    }
    Ok(())
}

/// The `--timeout` that `add` saves: a number of seconds a wait can be bounded
/// by, so the file never holds one that dialing would refuse.
fn validate_timeout(secs: f64) -> Result<f64, Error> {
    if secs.is_finite() && secs >= 0.0 {
        Ok(secs)
    } else {
        Err(Error::usage(format!(
            "--timeout must be a non-negative number of seconds, got {secs}"
        )))
    }
}

/// `--allow` and `--deny` patterns as saved: trimmed, and none of them empty,
/// since an empty pattern matches nothing and would only puzzle a later reader.
fn validate_patterns(patterns: Vec<String>, flag: &str) -> Result<Vec<String>, Error> {
    patterns
        .into_iter()
        .map(|p| match p.trim() {
            "" => Err(Error::usage(format!(
                "{flag} needs a tool name or a glob like 'read_*', got \"\""
            ))),
            p => Ok(p.to_string()),
        })
        .collect()
}

/// The allow and deny lists a server has, under the keys they are saved as.
fn tool_lists(cfg: &ServerConfig) -> Vec<(&'static str, Vec<String>)> {
    [("allow", &cfg.allow), ("deny", &cfg.deny)]
        .into_iter()
        .filter(|(_, patterns)| !patterns.is_empty())
        .map(|(list, patterns)| (list, patterns.clone()))
        .collect()
}

/// One line per list that is set, for the receipt a human reads.
fn tool_lists_lines(lists: &[(&str, Vec<String>)]) -> Vec<String> {
    lists
        .iter()
        .map(|(list, patterns)| format!("{list}: {}", patterns.join(", ")))
        .collect()
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

const SHELL_COMMANDS: &[&str] = &[
    "tools",
    "schema",
    "call",
    "resources",
    "read",
    "prompts",
    "prompt",
    "raw",
    "elicit",
    "info",
    "help",
    "quit",
    "exit",
];

const SHELL_SUMMARY: &str = "commands: tools, schema TOOL, call TOOL {\"arg\": \"value\"}, \
     resources, read URI, prompts, prompt NAME, raw METHOD, elicit {\"x\": 1}, info, help, \
     quit (or exit)";

const SHELL_HELP: &str = r#"commands (one per line; # starts a comment):
  tools [--long]             every tool this server offers
  schema TOOL                one tool's full JSON input schema
  help [TOOL]                this list, or one tool's parameters
  call TOOL {"arg": "value"} call a tool; arguments are one JSON object, default {}
  call TOOL arg=value        the same, as pairs the tool's schema types
  resources [--long]         every resource, then every URI template
  read URI                   one resource's contents
  prompts [--long]           every prompt this server offers
  prompt NAME {"arg": "..."} expand a prompt into its messages
  raw METHOD {"json": ...}   send any JSON-RPC method; allow and deny lists do not apply
  elicit {"arg": "value"}    answers for whatever the server elicits from here on
  info                       the initialize result
  quit (or exit)             close the session

At a terminal: Up and Down walk the history, Tab completes commands, tool and
prompt names and resource URIs, and ^C abandons the line being typed."#;

/// The answer to a tool name this server does not have.
fn no_such_tool(tools: &[Value], name: &str) -> Failure {
    Failure {
        error: Error::usage(format!("no tool named {name:?}")),
        hint: suggest_tool(tools, name, "`tools`"),
        tool: None,
    }
}

/// [`call_hint`] for a `call` line typed at the shell, where the arguments are
/// written bare and the tool list comes from the session already open.
fn shell_call_hint(
    cache: &mut Option<Vec<Value>>,
    conn: &mut client::Connection,
    tool: &str,
    argument_error: bool,
) -> Option<String> {
    call_hint(
        shell_tools(cache, conn),
        tool,
        argument_error,
        "call",
        "",
        "`tools`",
    )
}

/// Where shell input comes from. A terminal gets line editing, history and
/// completion; anything else is read a line at a time exactly as before, which
/// is what scripts and pipes depend on.
enum Input {
    Tty {
        editor: Box<rustyline::Editor<ShellHelper, rustyline::history::DefaultHistory>>,
        history: std::path::PathBuf,
        prompt: String,
        interrupts: u8,
    },
    Pipe {
        stdin: std::io::Stdin,
        /// Printed before each line when a human is typing into a redirected stdout.
        prompt: Option<String>,
    },
}

impl Input {
    /// A terminal reader when stdin and stdout are both a tty, so that a
    /// redirected stdout keeps today's behaviour: prompts on stderr, nothing else.
    fn open(
        store: &Store,
        r: &client::Resolved,
        label: &str,
        interactive: bool,
    ) -> Result<Self, Error> {
        let prompt = format!("{label}> ");
        if !interactive || !std::io::stdout().is_terminal() {
            return Ok(Input::Pipe {
                stdin: std::io::stdin(),
                prompt: interactive.then_some(prompt),
            });
        }
        let config = rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .auto_add_history(false)
            .build();
        let mut editor = rustyline::Editor::with_config(config)
            .map_err(|e| Error::usage(format!("cannot start line editing: {e}")))?;
        editor.set_helper(Some(ShellHelper::default()));
        let history = store.history_path(r.saved.then_some(r.name.as_str()));
        editor.load_history(&history).ok();
        Ok(Input::Tty {
            editor: Box::new(editor),
            history,
            prompt,
            interrupts: 0,
        })
    }

    /// The next line, or `None` when the session should end.
    fn next(&mut self, ui: &dyn Presenter) -> Result<Option<String>, Error> {
        match self {
            Input::Pipe { stdin, prompt } => {
                if let Some(prompt) = prompt {
                    ui.err(prompt);
                }
                let mut line = String::new();
                match stdin
                    .read_line(&mut line)
                    .map_err(|e| Error::usage(e.to_string()))?
                {
                    0 => Ok(None),
                    _ => Ok(Some(line)),
                }
            }
            Input::Tty {
                editor,
                prompt,
                interrupts,
                ..
            } => loop {
                match editor.readline(prompt) {
                    Ok(line) => {
                        *interrupts = 0;
                        if !line.trim().is_empty() {
                            editor.add_history_entry(line.as_str()).ok();
                        }
                        return Ok(Some(line));
                    }
                    // One ^C abandons the line being typed; a second one leaves,
                    // which is what ^C did before there was a line to abandon.
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        *interrupts += 1;
                        if *interrupts > 1 {
                            return Ok(None);
                        }
                        ui.err_line("(^C again, or `quit`, to exit)");
                    }
                    Err(rustyline::error::ReadlineError::Eof) => return Ok(None),
                    Err(e) => return Err(Error::usage(e.to_string())),
                }
            },
        }
    }

    /// Offer what the session has learned to Tab completion.
    fn set_completions(&mut self, tools: &[Value], resources: &[Value], prompts: &[Value]) {
        if let Input::Tty { editor, .. } = self {
            if let Some(helper) = editor.helper_mut() {
                helper.tools = tools.to_vec();
                helper.resources = field_values(resources, "uri");
                helper.prompts = field_values(prompts, "name");
            }
        }
    }

    fn save_history(&mut self) {
        if let Input::Tty {
            editor, history, ..
        } = self
        {
            if let Some(dir) = history.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            editor.save_history(history).ok();
        }
    }
}

/// The tool list, kept for explaining mistakes: fetched at most once per session,
/// and best effort, so a server that will not list its tools just gets no hint.
fn shell_tools<'a>(
    cache: &'a mut Option<Vec<Value>>,
    conn: &mut client::Connection,
) -> &'a [Value] {
    cache
        .get_or_insert_with(|| conn.list_tools().unwrap_or_default())
        .as_slice()
}

fn run(ui: &dyn Presenter, cli: Cli) -> Result<u8, Failure> {
    let store = Store::from_env()?;
    let mut opts = Options {
        timeout: cli
            .timeout
            .map(|secs| Duration::from_secs_f64(secs.max(0.0))),
        user_agent: cli
            .user_agent
            .clone()
            .unwrap_or_else(|| USER_AGENT.to_string()),
        extra_headers: parse_headers(&cli.headers)?,
        token_env: cli.token_env.clone(),
        protocol_version: cli.protocol_version,
        verbose: cli.verbose,
        no_daemon: cli.no_daemon || daemon::disabled_by_env(),
        retry: !cli.no_retry,
        log_level: cli.log_level,
        trace: Trace::from_flag_or_env(cli.trace.as_deref())?,
        ..Options::default()
    };
    // Built before the command is matched, because matching moves it out of
    // `cli`; it borrows nothing from there.
    let mut notices = Notices::new(ui, &cli);
    if let Some(dir) = &cli.save_dir {
        let renders_media = matches!(
            cli.cmd,
            Cmd::Call { .. } | Cmd::Prompt { .. } | Cmd::Read { .. } | Cmd::Shell { .. }
        );
        if !renders_media {
            return Err(Error::usage("--save-dir applies to call, prompt, read and shell").into());
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::usage(format!("--save-dir {}: {e}", dir.display())))?;
    }
    let save_dir = cli.save_dir.as_deref();
    let out = Output::choose(&cli)?;
    let plain = present::wants_plain(&cli);

    match cli.cmd {
        Cmd::Add {
            name,
            http,
            catalog,
            stdio,
            registry,
            package,
            remote,
            arg,
            env,
            cwd,
            allow,
            deny,
            force,
            no_probe,
        } => {
            // A registry entry is saved without being run, as documented: its
            // command usually needs values the user has yet to supply.
            let dial = !no_probe && registry.is_none();
            let timeout = cli.timeout.map(validate_timeout).transpose()?;
            let mut notes = Vec::new();
            let mut cfg = match (http, stdio, registry) {
                (None, None, None) if catalog.is_some() => {
                    let id = catalog.as_deref().unwrap_or("");
                    let resolved = from_catalog(ui, &store, &opts, id)?;
                    let elsewhere = mcpdial::config::saved_from_catalog(&store.servers()?, id)
                        .filter(|saved| *saved != name && !force)
                        .map(String::from);
                    if let Some(saved) = elsewhere {
                        return Err(Error::usage(format!(
                            "catalog entry {id} is already saved as {saved}; pass --force to save it again"
                        ))
                        .into());
                    }
                    notes = resolved.notes;
                    resolved.config
                }
                (Some(url), None, None) => ServerConfig::http(url),
                (None, Some(cmd), None) => ServerConfig::stdio(cmd),
                (None, None, Some(entry)) => {
                    let pick = if remote {
                        Pick::Remote
                    } else if let Some(kind) = package {
                        Pick::Package(kind)
                    } else {
                        Pick::Any
                    };
                    let resolved = from_registry(&opts, &entry, &pick, &arg)?;
                    notes = resolved.notes;
                    resolved.config
                }
                _ => {
                    return Err(Error::usage(
                        "pass exactly one of --http URL, --stdio CMD or --registry NAME",
                    )
                    .into())
                }
            };
            if cfg.stdio.is_some() && (!opts.extra_headers.is_empty() || opts.token_env.is_some()) {
                return Err(
                    Error::usage("--header and --token-env only apply to --http servers").into(),
                );
            }
            if cfg.http.is_some() && (!env.is_empty() || cwd.is_some()) {
                return Err(Error::usage("--env and --cwd only apply to --stdio servers").into());
            }
            for item in &env {
                match item.split_once('=') {
                    Some((k, v)) if !k.trim().is_empty() => {
                        cfg.env.insert(k.trim().to_string(), v.to_string());
                    }
                    _ => {
                        return Err(Error::usage(format!(
                            "--env must look like KEY=VALUE, got {item:?}"
                        ))
                        .into())
                    }
                }
            }
            if cwd.is_some() {
                cfg.cwd = cwd;
            }
            cfg.headers.extend(opts.extra_headers.iter().cloned());
            cfg.token_env = opts.token_env.clone();
            cfg.protocol_version = opts.protocol_version.map(|v| v.to_string());
            cfg.timeout = timeout;
            cfg.allow = validate_patterns(allow, "--allow")?;
            cfg.deny = validate_patterns(deny, "--deny")?;
            validate_location(&cfg)?;
            let replaced = store.server(&name)?;
            if let Some(old) = replaced.as_ref().filter(|_| !force) {
                return Err(Error::usage(format!(
                    "{name} is already saved ({} {}); pass --force to replace it",
                    old.kind(),
                    old.location()
                ))
                .into());
            }
            let summary = format!("{} {}", cfg.kind(), cfg.location());
            let mut saved = json!({ "name": name, "kind": cfg.kind(), "location": cfg.location() });
            let lists = tool_lists(&cfg);
            store.add_server(&name, cfg)?;
            if force {
                store.forget_probe(&name)?;
            }
            let row = dial
                .then(|| client::listing_one(&store, &opts, &name))
                .transpose()?;
            if cli.json {
                if let Some(row) = &row {
                    saved = serde_json::to_value(row).expect("a listing is serializable");
                }
                if !notes.is_empty() {
                    saved["notes"] = json!(notes);
                }
                for (list, patterns) in &lists {
                    saved[list] = json!(patterns);
                }
                print_value(ui, &json!({ "saved": saved }), true);
            } else {
                ui.err_line(&match &replaced {
                    Some(old) => format!(
                        "saved {name} ({summary}), replacing {} {}",
                        old.kind(),
                        old.location()
                    ),
                    None => format!("saved {name} ({summary})"),
                });
                for line in tool_lists_lines(&lists) {
                    ui.err_line(&format!("  {line}"));
                }
                for note in &notes {
                    ui.note(note);
                }
                if let Some(row) = &row {
                    ui.table(&LISTING_HEADERS, &[listing_row(row)]);
                }
            }
            Ok(0)
        }

        Cmd::Set {
            name,
            allow,
            deny,
            clear_allow,
            clear_deny,
        } => {
            let Some(mut cfg) = store.server(&name)? else {
                return Err(Error::usage(format!("no server named {name:?}")).into());
            };
            let changing = !allow.is_empty() || !deny.is_empty() || clear_allow || clear_deny;
            if !changing {
                if cli.json {
                    print_json(
                        ui,
                        &json!({ "name": name, "allow": cfg.allow, "deny": cfg.deny }),
                    );
                } else {
                    let or = |patterns: &[String], none: &str| {
                        if patterns.is_empty() {
                            none.to_string()
                        } else {
                            patterns.join(", ")
                        }
                    };
                    ui.line(&name);
                    ui.line(&format!(
                        "  allow: {}",
                        or(&cfg.allow, "(every tool not denied)")
                    ));
                    ui.line(&format!("  deny:  {}", or(&cfg.deny, "(none)")));
                }
                return Ok(0);
            }
            if clear_allow {
                cfg.allow.clear();
            }
            if clear_deny {
                cfg.deny.clear();
            }
            if !allow.is_empty() {
                cfg.allow = validate_patterns(allow, "--allow")?;
            }
            if !deny.is_empty() {
                cfg.deny = validate_patterns(deny, "--deny")?;
            }
            let summary = format!("{} {}", cfg.kind(), cfg.location());
            let saved = json!({
                "name": name, "kind": cfg.kind(), "location": cfg.location(),
                "allow": cfg.allow, "deny": cfg.deny,
            });
            let lines = tool_lists_lines(&tool_lists(&cfg));
            store.add_server(&name, cfg)?;
            if cli.json {
                print_value(ui, &json!({ "saved": saved }), true);
            } else {
                ui.err_line(&format!("saved {name} ({summary})"));
                for line in lines {
                    ui.err_line(&format!("  {line}"));
                }
            }
            Ok(0)
        }

        Cmd::Search {
            query,
            limit,
            refresh,
            offline,
        } => {
            let query = query.join(" ");
            if query.trim().is_empty() {
                return Err(Failure::hinted(
                    Error::usage("search needs a query"),
                    "words that must all appear, like: mcpdial search browser automation",
                ));
            }
            let sync = match (refresh, offline) {
                (true, _) => mcpdial::registry::Sync::Refresh,
                (_, true) => mcpdial::registry::Sync::Offline,
                _ => mcpdial::registry::Sync::Auto,
            };
            let registry = Registry::from_env(opts.timeout_or_default(), &opts.user_agent);
            let mut progress =
                |n: usize| ui.progress(&format!("fetching the registry: {n} servers"));
            let indexed = mcpdial::registry::index(&store, &registry, sync, &mut progress);
            ui.progress_end();
            let (index, note) = indexed?;
            if let Some(note) = note {
                ui.note(&note);
            }
            let loaded = catalog::load(
                &store,
                &catalog::Source::from_env(),
                offline,
                opts.timeout_or_default(),
                &opts.user_agent,
            )?;
            if opts.verbose {
                ui.err_line(&format!("catalog: {}", loaded.origin));
            }
            let catalog: Vec<String> = loaded
                .entries
                .iter()
                .filter_map(|e| e.registry.clone())
                .collect();
            let hits = mcpdial::search::rank(&query, &index.servers, &catalog);
            if hits.is_empty() {
                if cli.json {
                    print_json(ui, &hits);
                } else {
                    ui.err_line(&format!("no registry entry matches {query:?}"));
                }
                return Ok(EXIT_ERROR);
            }
            let shown: Vec<&Value> = hits.iter().copied().take(limit).collect();
            if cli.json {
                print_json(ui, &shown);
                return Ok(0);
            }
            let rows: Vec<Vec<String>> = shown
                .iter()
                .map(|e| {
                    let server = &e["server"];
                    let name = server["name"].as_str().unwrap_or("?");
                    let transports = match mcpdial::registry::transports(server) {
                        t if t.is_empty() => "-".to_string(),
                        t => t.join(", "),
                    };
                    let source = if catalog.iter().any(|c| c == name) {
                        "catalog"
                    } else {
                        "registry"
                    };
                    vec![
                        name.to_string(),
                        transports,
                        source.to_string(),
                        truncate(server["description"].as_str().unwrap_or("")),
                    ]
                })
                .collect();
            ui.table(&["NAME", "TRANSPORTS", "SOURCE", "DESCRIPTION"], &rows);
            if hits.len() > shown.len() {
                ui.err_line(&format!(
                    "{} of {} matches; --limit N shows more",
                    shown.len(),
                    hits.len()
                ));
            }
            Ok(0)
        }

        Cmd::Import { file, from, force } => {
            let files: Vec<std::path::PathBuf> = match file {
                Some(f) => vec![f],
                None => mcpdial::import_config::candidates(from)
                    .into_iter()
                    .filter(|p| p.exists())
                    .collect(),
            };
            if files.is_empty() {
                return Err(Error::usage(
                    "no config files found; pass a path to a host's config file",
                )
                .into());
            }
            let existing = store.servers()?;
            let mut imported: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // The running commentary is for a human; a program gets one object at the end.
            let say = |line: String| {
                if !cli.json {
                    ui.err_line(&line);
                }
            };
            for path in &files {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                let found = mcpdial::import_config::read(path, &text)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                if found.is_empty() {
                    say(format!("{}: no servers", path.display()));
                    continue;
                }
                for f in found {
                    if existing.contains_key(&f.name) && !force {
                        say(format!(
                            "  skip {:<16} already saved (use --force to overwrite)",
                            f.name
                        ));
                        skipped.push(f.name);
                        continue;
                    }
                    let summary = format!("{} {}", f.config.kind(), f.config.location());
                    match store.add_server(&f.name, f.config) {
                        Ok(()) => {
                            say(format!(
                                "  add  {:<16} {summary}  [{} in {}]",
                                f.name,
                                f.scope,
                                path.display()
                            ));
                            for n in &f.notes {
                                say(format!("       note: {n}"));
                            }
                            if !f.notes.is_empty() {
                                notes.insert(f.name.clone(), f.notes);
                            }
                            imported.push(f.name);
                        }
                        Err(e) => {
                            say(format!("  skip {:<16} {e}", f.name));
                            skipped.push(f.name);
                        }
                    }
                }
            }
            if cli.json {
                let mut receipt = json!({ "imported": imported, "skipped": skipped });
                if !notes.is_empty() {
                    receipt["notes"] = json!(notes);
                }
                print_value(ui, &receipt, true);
            } else {
                ui.err_line(&format!("imported {} server(s)", imported.len()));
            }
            Ok(0)
        }

        Cmd::Export {
            names,
            format,
            merge,
        } => {
            let out = mcpdial::export_config::export(&store, &names, format, merge.as_deref())?;
            ui.out(&out.document);
            for note in out.notes {
                if cli.json {
                    ui.err_line(&json!({ "note": note }).to_string());
                } else {
                    ui.err_line(&note);
                }
            }
            Ok(0)
        }

        Cmd::Shell { target, no_browser } => {
            opts.elicit = elicitation(None, no_browser, cli.json, true)?;
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let si = &conn.server_info["serverInfo"];
            let interactive = std::io::stdin().is_terminal();
            if interactive {
                ui.err_line(&format!(
                    "connected to {} {}",
                    si["name"].as_str().unwrap_or("?"),
                    si["version"].as_str().unwrap_or("")
                ));
                ui.err_line(SHELL_SUMMARY);
            }
            // A saved server is prompted by its own name. An ad-hoc target is a
            // whole URL or command line, which makes a prompt that wraps the
            // terminal, so use what the server calls itself instead.
            let label = if r.saved {
                r.name.clone()
            } else {
                si["name"]
                    .as_str()
                    .map_or_else(|| truncate_at(&r.name, 24), String::from)
            };
            let mut input = Input::open(&store, &r, &label, interactive)?;
            let mut failures = 0u32;
            // tools/list, fetched at most once, so a mistake can be answered with
            // the shape the server actually wants.
            let mut cache: Option<Vec<Value>> = None;
            let mut resources: Option<Vec<Value>> = None;
            let mut prompts: Option<Vec<Value>> = None;
            if matches!(input, Input::Tty { .. }) {
                // One eager fetch: it gives Tab something to complete and warms
                // the same cache the hints read.
                let tools = shell_tools(&mut cache, &mut conn).to_vec();
                if advertises(&conn.server_info, "resources") {
                    resources = conn.session.list_resources().ok();
                }
                if advertises(&conn.server_info, "prompts") {
                    prompts = conn.session.list_prompts().ok();
                }
                input.set_completions(
                    &tools,
                    resources.as_deref().unwrap_or_default(),
                    prompts.as_deref().unwrap_or_default(),
                );
            }
            let info_cmd = "`info`";
            while let Some(raw) = input.next(ui)? {
                let text = raw.trim();
                if text.is_empty() || text.starts_with('#') {
                    continue;
                }
                let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
                let rest = rest.trim();
                let long = rest == "--long" || rest == "-l";
                let outcome: Result<(), Failure> = match word {
                    "quit" | "exit" => break,
                    "help" if rest.is_empty() => {
                        ui.err_line(SHELL_HELP);
                        Ok(())
                    }
                    "help" => match refuse_denied(&r.config, &r.name, rest) {
                        Err(denied) => Err(denied),
                        Ok(()) => {
                            let tools = shell_tools(&mut cache, &mut conn);
                            match find_tool(tools, rest) {
                                Some(t) => {
                                    ui.err_line(&tool_usage(t, "call", ""));
                                    Ok(())
                                }
                                None => Err(no_such_tool(tools, rest)),
                            }
                        }
                    },
                    "schema" if rest.is_empty() => Err(Failure::hinted(
                        Error::usage("schema needs a tool name"),
                        "usage: schema TOOL   (`tools` lists what this server offers)",
                    )),
                    "schema" => match refuse_denied(&r.config, &r.name, rest) {
                        Err(denied) => Err(denied),
                        Ok(()) => {
                            let tools = shell_tools(&mut cache, &mut conn);
                            match find_tool(tools, rest) {
                                Some(t) => {
                                    ui.paged(|| print_value(ui, t, cli.json));
                                    Ok(())
                                }
                                None => Err(no_such_tool(tools, rest)),
                            }
                        }
                    },
                    "info" => {
                        ui.paged(|| print_value(ui, &conn.server_info, cli.json));
                        Ok(())
                    }
                    "tools" => conn.list_tools().map_err(Failure::from).map(|tools| {
                        if cli.json {
                            print_value(ui, &json!({ "tools": brief::tools(&tools, long) }), true);
                        } else {
                            listed(ui, long, || {
                                ui.line(&format!("{} tool(s):", tools.len()));
                                print_tools(ui, &tools, long);
                            });
                        }
                        cache = Some(tools);
                    }),
                    "resources" => match conn.session.list_resources() {
                        Err(e) => Err(missing_capability(e, "resources", info_cmd)),
                        Ok(found) => {
                            let templates =
                                conn.session.list_resource_templates().unwrap_or_default();
                            if cli.json {
                                ui.line(&format!(
                                    "{}",
                                    json!({
                                        "resources": brief::resources(&found, long),
                                        "resourceTemplates": brief::resources(&templates, long),
                                    })
                                ));
                            } else {
                                ui.line(&format!("{} resource(s):", found.len()));
                                ui.resources(&found, long);
                                if !templates.is_empty() {
                                    ui.line(&format!("\n{} template(s):", templates.len()));
                                    ui.resources(&templates, long);
                                }
                            }
                            resources = Some(found);
                            Ok(())
                        }
                    },
                    "read" if rest.is_empty() => Err(Failure::hinted(
                        Error::usage("read needs a resource URI"),
                        "usage: read URI   (`resources` lists what this server offers)",
                    )),
                    "read" => match watched(&mut notices, |w| {
                        conn.session.read_resource_watching(rest, w)
                    }) {
                        Err(e) => Err(missing_item(e, "resources", "`resources`", info_cmd)),
                        Ok(mut result) => {
                            let redirect = format!("mcpdial read {} {}", shell_word(&target), rest);
                            let files = MediaFiles {
                                dir: save_dir,
                                stem: resource_stem(rest),
                            };
                            ui.paged(|| {
                                emit_resource(
                                    ui,
                                    &out,
                                    &mut result,
                                    cli.json,
                                    true,
                                    &files,
                                    &redirect,
                                )
                            })
                        }
                    },
                    "prompts" => match conn.session.list_prompts() {
                        Err(e) => Err(missing_capability(e, "prompts", info_cmd)),
                        Ok(found) => {
                            if cli.json {
                                print_value(
                                    ui,
                                    &json!({ "prompts": brief::prompts(&found, long) }),
                                    true,
                                );
                            } else {
                                ui.line(&format!("{} prompt(s):", found.len()));
                                print_prompts(ui, &found, long);
                            }
                            prompts = Some(found);
                            Ok(())
                        }
                    },
                    "prompt" => {
                        let (name, args) = name_and_args(rest);
                        if name.is_empty() {
                            Err(Failure::hinted(
                                Error::usage("prompt needs a name"),
                                "usage: prompt NAME {\"arg\": \"value\"}   (`prompts` lists what this server offers)",
                            ))
                        } else {
                            args::shell_arguments(args, || Value::Null)
                                .and_then(|a| {
                                    watched(&mut notices, |w| {
                                        conn.session.get_prompt_watching(name, a, w)
                                    })
                                })
                                .map_err(|e| {
                                    missing_item(e, "prompts", "`prompts --long`", info_cmd)
                                })
                                .and_then(|mut result| {
                                    let files = MediaFiles {
                                        dir: save_dir,
                                        stem: file_stem(name),
                                    };
                                    ui.paged(|| {
                                        emit(
                                            ui,
                                            &out,
                                            &mut result,
                                            cli.json,
                                            true,
                                            &files,
                                            render_messages,
                                        )
                                    })
                                })
                        }
                    }
                    "call" => {
                        let (tool, args) = name_and_args(rest);
                        if tool.is_empty() {
                            Err(Failure::hinted(
                                Error::usage("call needs a tool name"),
                                "usage: call TOOL {\"arg\": \"value\"}   (`tools` lists what this server offers)",
                            ))
                        } else if let Err(denied) = refuse_denied(&r.config, &r.name, tool) {
                            Err(denied)
                        } else {
                            // Arguments that do not parse and arguments the server
                            // rejects mean the same thing to whoever typed the line:
                            // show them what this tool takes.
                            let parsed = args::shell_arguments(args, || {
                                find_tool(shell_tools(&mut cache, &mut conn), tool)
                                    .map_or(Value::Null, |t| t["inputSchema"].clone())
                            });
                            match parsed {
                                Err(e) => Err(Failure {
                                    error: e,
                                    hint: shell_call_hint(&mut cache, &mut conn, tool, true),
                                    tool: None,
                                }),
                                Ok(a) => match watched(&mut notices, |w| {
                                    conn.session.call_tool_watching(tool, a, w)
                                }) {
                                    Ok(mut result) => {
                                        let files = MediaFiles {
                                            dir: save_dir,
                                            stem: file_stem(tool),
                                        };
                                        rendered(ui, &mut result, cli.json, &files, render_content)
                                            .and_then(|text| {
                                                let failed = ui.paged(|| {
                                                    print_tool_result(
                                                        ui, &out, &result, &text, cli.json, true,
                                                    )
                                                })?;
                                                if failed {
                                                    let argument_error =
                                                        reads_as_argument_error(&text);
                                                    if let Some(hint) = shell_call_hint(
                                                        &mut cache,
                                                        &mut conn,
                                                        tool,
                                                        argument_error,
                                                    ) {
                                                        print_hint(ui, &hint, cli.json);
                                                    }
                                                }
                                                Ok(())
                                            })
                                    }
                                    Err(e) => {
                                        let hint = server_refused(&e)
                                            .then(|| {
                                                shell_call_hint(
                                                    &mut cache,
                                                    &mut conn,
                                                    tool,
                                                    is_argument_error(&e),
                                                )
                                            })
                                            .flatten();
                                        Err(Failure {
                                            error: e,
                                            hint,
                                            tool: None,
                                        })
                                    }
                                },
                            }
                        }
                    }
                    "elicit" if rest.is_empty() => Err(Failure::hinted(
                        Error::usage("elicit needs a JSON object of answers"),
                        "usage: elicit {\"confirm\": true}   (used for every elicitation from here on)",
                    )),
                    "elicit" => parse_object(rest, "answers")
                        .map_err(Failure::from)
                        .map(|values| {
                            let values = values.as_object().cloned().unwrap_or_default();
                            conn.elicit.set(values.clone());
                            if cli.json {
                                ui.json(&json!({ "elicit": values }).to_string());
                            } else {
                                ui.line(&format!("{} answer(s) ready to elicit with", values.len()));
                            }
                        }),
                    "raw" => {
                        let (method, params) = name_and_args(rest);
                        if method.is_empty() {
                            Err(Failure::hinted(
                                Error::usage("raw needs a method"),
                                "usage: raw METHOD {\"json\": \"params\"}   e.g. raw tools/list",
                            ))
                        } else {
                            parse_object(params, "params")
                                .and_then(|p| conn.session.request(method, Some(p)))
                                .map_err(Failure::from)
                                .map(|result| ui.paged(|| print_value(ui, &result, cli.json)))
                        }
                    }
                    // Only reachable when line editing is off, since a terminal
                    // reader consumes these itself.
                    other if other.starts_with('\u{1b}') => Err(Failure::hinted(
                        Error::usage("that was an escape sequence, not a command"),
                        "arrow keys and line editing need a terminal on both stdin and stdout",
                    )),
                    other => {
                        let tools = shell_tools(&mut cache, &mut conn);
                        let unknown = || Error::usage(format!("unknown command {other:?}"));
                        let near_tool =
                            closest(other, tools.iter().filter_map(|t| t["name"].as_str()))
                                .and_then(|near| find_tool(tools, near));
                        Err(if let Some(t) = find_tool(tools, other) {
                            // The commonest mistake: typing a tool name on its own.
                            Failure::hinted(
                                Error::usage(format!("{other} is a tool, not a command")),
                                tool_usage(t, "call", ""),
                            )
                        } else if let Some(near) = closest(other, SHELL_COMMANDS.iter().copied()) {
                            Failure::hinted(unknown(), format!("did you mean {near}?"))
                        } else if let Some(t) = near_tool {
                            Failure::hinted(
                                unknown(),
                                format!(
                                    "did you mean the tool {}?\n{}",
                                    t["name"].as_str().unwrap_or("?"),
                                    tool_usage(t, "call", "")
                                ),
                            )
                        } else {
                            Failure::hinted(unknown(), SHELL_SUMMARY)
                        })
                    }
                };
                input.set_completions(
                    cache.as_deref().unwrap_or_default(),
                    resources.as_deref().unwrap_or_default(),
                    prompts.as_deref().unwrap_or_default(),
                );
                if let Err(f) = outcome {
                    failures += 1;
                    if cli.json {
                        print_value(ui, &f.to_json(), true);
                    } else {
                        f.report(ui);
                    }
                }
            }
            input.save_history();
            Ok(if failures > 0 && !interactive {
                EXIT_ERROR
            } else {
                0
            })
        }

        Cmd::Start { name, idle } => {
            let idle = idle_duration(idle)?;
            let pid = daemon::start(&store, &name, idle, &opts)?;
            if cli.json {
                print_json(
                    ui,
                    &json!({
                        "name": name, "pid": pid,
                        "socket": daemon::socket_path(&store, &name),
                    }),
                );
            } else {
                ui.err_line(&format!("started {name} (pid {pid})"));
            }
            Ok(0)
        }

        Cmd::Stop { name } => {
            daemon::stop(&store, &name, &opts)?;
            ui.err_line(&format!("stopped {name}"));
            Ok(0)
        }

        // Only `start` runs this, with nothing but a pipe back to it on stdout:
        // an error before the socket is open is reported there, for `start` to
        // print as its own.
        Cmd::Daemon { name, idle } => {
            let idle = idle_duration(idle)?;
            match daemon::serve(&store, &name, &opts, idle) {
                Ok(()) => Ok(0),
                Err(e) => {
                    print_value(ui, &error_json(&e), true);
                    Ok(EXIT_ERROR)
                }
            }
        }

        Cmd::Schema { target, tool } => {
            let r = client::resolve(&store, &target)?;
            refuse_denied(&r.config, &r.name, &tool)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let tools = conn.list_tools()?;
            // The `tools/list` this name was looked up in succeeded, and nothing
            // was ever sent for the tool itself: the mistake is the caller's, and
            // saying so as the shell's own `schema` does costs no invented -32602.
            let Some(t) = find_tool(&tools, &tool) else {
                let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
                return Err(Failure {
                    error: Error::usage(format!(
                        "no tool named {tool:?}; available: {}",
                        names.join(", ")
                    )),
                    hint: closest(&tool, names.iter().copied())
                        .map(|near| format!("did you mean {near}?")),
                    tool: None,
                });
            };
            // Beside the note below, and on stderr for the same reason: stdout
            // is the tool object, whole, whichever mode asked for it.
            let hints = client::hint_tags(t);
            if !hints.is_empty() {
                ui.err_line(&format!("{tool}{hints}"));
            }
            ui.paged(|| print_json(ui, t));
            if t.get("outputSchema").is_some() {
                ui.err_line("this tool declares an outputSchema: results carry structuredContent");
            }
            Ok(0)
        }

        Cmd::Resources { target, long } => {
            let mut conn = dial(&store, &opts, &target)?;
            let resources = conn
                .session
                .list_resources()
                .map_err(|e| missing_capability(e, "resources", &info_hint(&target)))?;
            // Templates are optional even where resources are not, so a server with
            // none of them must not turn the whole listing into an error.
            let templates = conn.session.list_resource_templates().unwrap_or_default();
            if cli.json {
                print_json(
                    ui,
                    &json!({
                        "resources": brief::resources(&resources, long),
                        "resourceTemplates": brief::resources(&templates, long),
                    }),
                );
            } else {
                ui.line(&format!("{} resource(s):\n", resources.len()));
                ui.resources(&resources, long);
                if !templates.is_empty() {
                    ui.line(&format!("\n{} template(s):\n", templates.len()));
                    ui.resources(&templates, long);
                }
            }
            Ok(0)
        }

        Cmd::Read { target, uri } => {
            let mut conn = dial(&store, &opts, &target)?;
            let outcome = conn.session.read_resource_watching(&uri, &mut notices);
            notices.finish();
            let mut result = outcome.map_err(|e| {
                missing_item(
                    e,
                    "resources",
                    &resources_hint(&target),
                    &info_hint(&target),
                )
            })?;
            let redirect = format!("mcpdial read {} {}", shell_word(&target), shell_word(&uri));
            let files = MediaFiles {
                dir: save_dir,
                stem: resource_stem(&uri),
            };
            ui.paged(|| emit_resource(ui, &out, &mut result, cli.json, false, &files, &redirect))?;
            Ok(0)
        }

        Cmd::Prompts { target, long } => {
            let mut conn = dial(&store, &opts, &target)?;
            let prompts = conn
                .session
                .list_prompts()
                .map_err(|e| missing_capability(e, "prompts", &info_hint(&target)))?;
            if cli.json {
                print_json(ui, &json!({ "prompts": brief::prompts(&prompts, long) }));
            } else {
                ui.line(&format!("{} prompt(s):\n", prompts.len()));
                print_prompts(ui, &prompts, long);
            }
            Ok(0)
        }

        Cmd::Prompt {
            target,
            name,
            arguments,
            elicit,
            no_browser,
        } => {
            opts.elicit = elicitation(elicit.as_deref(), no_browser, cli.json, false)?;
            let form = args::form(&arguments)?;
            let arguments = match form.json() {
                Some(text) => read_json_arg(text, "arguments").map_err(|e| {
                    Failure::hinted(
                        e,
                        json_arg_hint(
                            text,
                            &format!("mcpdial prompt {} {name}", shell_word(&target)),
                            format!(
                                "`mcpdial prompts {} --long` shows what {name} takes",
                                shell_word(&target)
                            ),
                        ),
                    )
                })?,
                // A prompt's arguments carry no schema: every one is a string.
                None => args::parse_pairs(form.pairs(), &Value::Null)?,
            };
            let mut conn = dial(&store, &opts, &target)?;
            let outcome = conn
                .session
                .get_prompt_watching(&name, arguments, &mut notices);
            notices.finish();
            let mut result = outcome.map_err(|e| {
                missing_item(e, "prompts", &prompts_hint(&target), &info_hint(&target))
            })?;
            if !cli.json {
                if let Some(d) = result["description"].as_str() {
                    ui.err_line(d);
                }
            }
            let files = MediaFiles {
                dir: save_dir,
                stem: file_stem(&name),
            };
            ui.paged(|| {
                emit(
                    ui,
                    &out,
                    &mut result,
                    cli.json,
                    false,
                    &files,
                    render_messages,
                )
            })?;
            Ok(0)
        }

        Cmd::Guide => {
            ui.out(include_str!("../docs/AGENTS.md"));
            Ok(0)
        }

        Cmd::Catalog { offline } => {
            let loaded = catalog::load(
                &store,
                &catalog::Source::from_env(),
                offline,
                opts.timeout_or_default(),
                &opts.user_agent,
            )?;
            if opts.verbose {
                ui.err_line(&format!("catalog: {}", loaded.origin));
            }
            if cli.json {
                print_json(ui, &loaded.entries);
            } else {
                ui.catalog(&loaded.entries);
                ui.err_line("\nadd one with: mcpdial add NAME --catalog ID");
            }
            Ok(0)
        }

        Cmd::Browse {
            all,
            offline,
            preview,
        } => browse::run(
            ui,
            &store,
            &opts,
            browse::Flags {
                all,
                offline,
                preview,
                interactive: !plain && std::io::stdin().is_terminal(),
            },
        ),

        Cmd::Completions { shell } => {
            // Building the whole command tree to walk it recurses deeper than
            // the 1 MB stack a Windows main thread is given, and every global
            // flag added takes it deeper still, so the walk gets a stack of its
            // own rather than being one flag away from overflowing.
            const SCRIPT_STACK: usize = 8 * 1024 * 1024;
            let failed =
                |what: &str| Error::transport(format!("writing the {shell} script: {what}"));
            std::thread::Builder::new()
                .stack_size(SCRIPT_STACK)
                .spawn(move || {
                    let mut command = Cli::command();
                    let name = command.get_name().to_string();
                    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
                })
                .map_err(|e| failed(&e.to_string()))?
                .join()
                .map_err(|_| failed("the writer stopped"))?;
            Ok(0)
        }

        Cmd::Rm { name } => {
            if store.remove_server(&name)? {
                if cli.json {
                    print_value(ui, &json!({ "removed": name }), true);
                } else {
                    ui.err_line(&format!("removed {name}"));
                }
                Ok(0)
            } else {
                Err(Error::usage(format!("no server named {name:?}")).into())
            }
        }

        Cmd::Ls { no_probe, refresh } => {
            if no_probe {
                let servers = store.servers()?;
                let creds = store.credentials()?;
                if cli.json {
                    let rows: Vec<Value> = servers
                        .iter()
                        .map(|(n, c)| {
                            saved_row(
                                n,
                                c,
                                creds.get(n).is_some_and(Credential::has_token),
                                daemon::is_running(&store, n),
                            )
                        })
                        .collect();
                    print_json(ui, &rows);
                } else {
                    // A column earns its place only once a server has something for it.
                    let with_source = servers.values().any(|c| c.source.is_some());
                    let with_timeout = servers.values().any(|c| c.timeout.is_some());
                    let running: Vec<&String> = servers
                        .keys()
                        .filter(|n| daemon::is_running(&store, n))
                        .collect();
                    let mut headers = vec!["NAME", "TYPE", "AUTH"];
                    if with_source {
                        headers.push("SOURCE");
                    }
                    if with_timeout {
                        headers.push("TIMEOUT");
                    }
                    if !running.is_empty() {
                        headers.push("DAEMON");
                    }
                    headers.push("LOCATION");
                    let rows: Vec<Vec<String>> = servers
                        .iter()
                        .map(|(n, c)| {
                            let auth = if let Some(var) = &c.token_env {
                                format!("${var}")
                            } else if creds.get(n).is_some_and(Credential::has_token) {
                                "saved".into()
                            } else {
                                "-".into()
                            };
                            let mut row = vec![n.clone(), c.kind().into(), auth];
                            if with_source {
                                row.push(c.source.as_ref().map_or("-", Source::label).into());
                            }
                            if with_timeout {
                                row.push(c.timeout.map_or("-".into(), |t| format!("{t}s")));
                            }
                            if !running.is_empty() {
                                row.push(daemon_label(running.contains(&n)));
                            }
                            row.push(c.location().into());
                            row
                        })
                        .collect();
                    ui.table(&headers, &rows);
                }
                return Ok(0);
            }
            let freshness = if refresh {
                client::Freshness::Live
            } else {
                client::Freshness::Remembered
            };
            let listing = client::listing(&store, &opts, freshness)?;
            if cli.json {
                let servers = store.servers()?;
                let creds = store.credentials()?;
                let rows: Vec<Value> = listing
                    .iter()
                    .map(|row| {
                        let mut out = match servers.get(&row.name) {
                            Some(cfg) => saved_row(
                                &row.name,
                                cfg,
                                creds.get(&row.name).is_some_and(Credential::has_token),
                                row.running,
                            ),
                            None => json!({}),
                        };
                        let probed = serde_json::to_value(row).expect("a listing is serializable");
                        if let (Some(fields), Value::Object(status)) = (out.as_object_mut(), probed)
                        {
                            fields.extend(status);
                        }
                        out
                    })
                    .collect();
                print_json(ui, &rows);
            } else if listing.is_empty() {
                ui.err_line(NO_SERVERS);
            } else {
                let rows: Vec<Vec<String>> = listing.iter().map(listing_row).collect();
                ui.table(&LISTING_HEADERS, &rows);
            }
            Ok(0)
        }

        Cmd::Tools {
            target: None, long, ..
        } => {
            let mut probes = client::probe_all(&store, &opts, true)?;
            if cli.json {
                for p in &mut probes {
                    p.tools = p.tools.take().map(|t| brief::tools(&t, long));
                }
                print_json(ui, &Servers { servers: &probes });
                return Ok(0);
            }
            if probes.is_empty() {
                ui.err_line(NO_SERVERS);
                return Ok(0);
            }
            listed(ui, long, || {
                for (i, p) in probes.iter().enumerate() {
                    if i > 0 {
                        ui.line("");
                    }
                    match &p.tools {
                        Some(tools) => {
                            ui.line(&format!(
                                "## {}  {}  ({} tools)",
                                p.name,
                                p.server.as_deref().unwrap_or(""),
                                tools.len()
                            ));
                            print_tools(ui, tools, long);
                        }
                        None => ui.line(&format!(
                            "## {}  {}{}",
                            p.name,
                            p.status.label(),
                            p.status
                                .detail()
                                .map(|d| format!(": {}", truncate(d)))
                                .unwrap_or_default()
                        )),
                    }
                }
            });
            Ok(0)
        }

        Cmd::Tools {
            target: Some(target),
            long,
            all,
            snapshot: to_file,
            check,
            strict,
        } => {
            // Both files are answered before anything is dialed: a path that
            // could never be written, and a snapshot that is not one, cost no
            // connection.
            if let Some(path) = &to_file {
                snapshot::reserve(path)?;
            }
            let promised = check.as_deref().map(snapshot::read).transpose()?;
            let mut conn = dial(&store, &opts, &target)?;
            let tools = if all {
                conn.list_all_tools()?
            } else {
                conn.list_tools()?
            };
            if let Some(path) = to_file {
                snapshot::write(&path, &snapshot::document(&conn.server_info, &tools))?;
                snapshot::wrote(ui, cli.json, &path, tools.len());
                return Ok(0);
            }
            if let Some(promised) = promised {
                let comparison = snapshot::compare(&promised, &tools, strict);
                comparison.report(ui, cli.json, snapshot::Report::Stdout);
                return Ok(if comparison.ok() { 0 } else { EXIT_DRIFT });
            }
            if cli.json {
                print_json(ui, &json!({ "tools": brief::tools(&tools, long) }));
            } else {
                listed(ui, long, || {
                    ui.line(&format!("{} tool(s):\n", tools.len()));
                    print_tools(ui, &tools, long);
                });
            }
            Ok(0)
        }

        Cmd::Grep(flags) => grep::run(ui, &store, &opts, cli.json, flags),

        Cmd::Info { target } => {
            let conn = dial(&store, &opts, &target)?;
            let init = &conn.server_info;
            if cli.json {
                print_json(ui, init);
            } else {
                ui.paged(|| {
                    let si = &init["serverInfo"];
                    ui.line(&format!(
                        "{} {}",
                        si["name"].as_str().unwrap_or("?"),
                        si["version"].as_str().unwrap_or("")
                    ));
                    ui.line(&format!(
                        "protocol {}",
                        init["protocolVersion"].as_str().unwrap_or("?")
                    ));
                    let caps: Vec<&str> = init["capabilities"]
                        .as_object()
                        .map(|o| o.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    ui.line(&format!(
                        "capabilities: {}",
                        if caps.is_empty() {
                            "(none)".into()
                        } else {
                            caps.join(", ")
                        }
                    ));
                    if let Some(instr) = init["instructions"].as_str() {
                        ui.line(&format!("\n{}", instr.trim()));
                    }
                });
            }
            Ok(0)
        }

        Cmd::Call {
            target,
            tool,
            arguments,
            elicit,
            no_browser,
            check,
            strict,
        } => {
            let promised = check.as_deref().map(snapshot::read).transpose()?;
            opts.elicit = elicitation(elicit.as_deref(), no_browser, cli.json, false)?;
            let form = args::form(&arguments)?;
            // A JSON object is settled before anything is dialed, as it always
            // was; pairs wait for the schema only an open session can supply.
            let object = form
                .json()
                .map(|text| {
                    read_json_arg(text, "arguments").map_err(|e| {
                        Failure::hinted(
                            e,
                            json_arg_hint(
                                text,
                                &format!("mcpdial call {} {tool}", shell_word(&target)),
                                format!(
                                    "`mcpdial schema {} {tool}` shows what {tool} takes",
                                    shell_word(&target)
                                ),
                            ),
                        )
                    })
                })
                .transpose()?;
            let r = client::resolve(&store, &target)?;
            refuse_denied(&r.config, &r.name, &tool)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            // Nothing is sent for a tool the snapshot says has moved. A
            // comparison that passes says nothing at all, so the result stays
            // the only thing this command prints.
            if let Some(promised) = promised {
                let live = conn.list_tools()?;
                let comparison = snapshot::compare(
                    &snapshot::named(&promised, &tool),
                    &snapshot::named(&live, &tool),
                    strict,
                );
                if !comparison.ok() {
                    comparison.report(ui, cli.json, snapshot::Report::Stderr);
                    return Ok(EXIT_DRIFT);
                }
            }
            let prefix = format!("mcpdial call {}", shell_word(&target));
            let tools_cmd = format!("`mcpdial tools {}`", shell_word(&target));
            let hint = |conn: &mut client::Connection, argument_error: bool| {
                let tools = conn.list_tools().unwrap_or_default();
                call_hint(&tools, &tool, argument_error, &prefix, "'", &tools_cmd)
            };
            let arguments = match object {
                Some(object) => object,
                None => {
                    let pairs = form.pairs();
                    let tools = if args::needs_schema(pairs) {
                        conn.list_tools().unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let schema =
                        find_tool(&tools, &tool).map_or(Value::Null, |t| t["inputSchema"].clone());
                    args::parse_pairs(pairs, &schema).map_err(|e| Failure {
                        error: e,
                        hint: call_hint(&tools, &tool, true, &prefix, "'", &tools_cmd),
                        tool: None,
                    })?
                }
            };
            let outcome = conn
                .session
                .call_tool_watching(&tool, arguments, &mut notices);
            notices.finish();
            let mut result = match outcome {
                Ok(result) => result,
                // The server said no; say what it wanted instead.
                Err(e) => {
                    let hint = server_refused(&e)
                        .then(|| hint(&mut conn, is_argument_error(&e)))
                        .flatten();
                    return Err(Failure {
                        error: e,
                        hint,
                        tool: None,
                    });
                }
            };
            let files = MediaFiles {
                dir: save_dir,
                stem: file_stem(&tool),
            };
            let text = rendered(ui, &mut result, cli.json, &files, render_content)?;
            let is_error =
                ui.paged(|| print_tool_result(ui, &out, &result, &text, cli.json, false))?;
            // A failed result that is really a schema complaint, or a server's way
            // of saying it has no such tool, gets the same answer as the JSON-RPC
            // error other servers would have sent.
            if is_error {
                if let Some(hint) = hint(&mut conn, reads_as_argument_error(&text)) {
                    print_hint(ui, &hint, cli.json);
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
            let mut conn = dial(&store, &opts, &target)?;
            let result = conn.session.request(&method, Some(params))?;
            let sent = out.deliver(
                Payload::Json {
                    value: &result,
                    one_line: false,
                },
                false,
            )?;
            ui.paged(|| output::show(ui, sent, As::Json, cli.json, || print_json(ui, &result)));
            Ok(0)
        }

        Cmd::Serve {
            target,
            listen,
            stdio,
            listen_any,
            bearer_env,
            allow,
            deny,
        } => Ok(serve::run(
            store,
            opts,
            &target,
            serve::Settings {
                listen,
                stdio,
                listen_any,
                bearer_env,
                allow,
                deny,
                json: cli.json,
            },
        )?),

        Cmd::Login {
            target,
            grant,
            scope,
            port,
            client_id,
            client_metadata_url,
            no_client_metadata,
            client_secret,
            client_secret_env,
            redirect_host,
            no_browser,
        } => {
            let r = client::resolve(&store, &target)?;
            let dialed = r.config.expanded(|var| std::env::var(var).ok())?;
            let Some(url) = dialed.http else {
                return Err(Error::usage(
                    "login only applies to HTTP servers; stdio servers need no token",
                )
                .into());
            };
            let client_secret = (client_secret || client_secret_env.is_some())
                .then(|| read_secret(ui, client_secret_env.as_deref(), "client secret"))
                .transpose()?;
            let existing = store.credential(&r.name)?;
            let http = client::oauth_http(&opts, &r.name, opts.timeout_for(&r)?);
            let client_metadata = match client_metadata_url {
                Some(url) => oauth::ClientMetadata::Url(url),
                None if no_client_metadata => oauth::ClientMetadata::Never,
                None => oauth::ClientMetadata::IfAdvertised,
            };
            let login_opts = oauth::LoginOptions {
                scope,
                port,
                client_id,
                client_secret,
                client_metadata,
                redirect_host,
                open_browser: !no_browser,
                timeout: Duration::from_secs(300),
            };
            let notify = |line: &str| ui.err_line(line);
            let cred = match grant.as_str() {
                "client-credentials" => {
                    oauth::login_client_credentials(&http, &url, &login_opts, notify)?
                }
                _ => oauth::login(&http, &url, existing.as_ref(), &login_opts, notify)?,
            };
            store.save_credential(&r.name, cred.clone())?;
            let refreshable = cred.can_refresh();
            if cli.json {
                ui.line(&format!(
                    "{}",
                    json!({ "login": {
                        "name": r.name,
                        "expires_at": cred.expires_at,
                        "refreshable": refreshable,
                        "registration": cred.registration,
                    } })
                ));
            } else {
                ui.err_line(&format!(
                    "saved token for {} ({}{})",
                    r.name,
                    expiry_label(&cred),
                    if refreshable { ", refreshable" } else { "" }
                ));
            }
            Ok(0)
        }

        Cmd::Logout { target } | Cmd::Token(TokenCmd::Rm { name: target }) => {
            let (name, dialable) = credential_key(&store, target);
            let removed = store.remove_credential(&name)?;
            // Removing a credential a server never had is the idempotent
            // success it looks like; a name that stands for nothing at all is
            // the typo `rm` already refuses, and is refused here the same way.
            if !removed && !dialable {
                return Err(Error::usage(format!(
                    "no server named {name:?} and no credential saved for it"
                ))
                .into());
            }
            if cli.json {
                ui.line(&format!(
                    "{}",
                    json!({ "removed_credential": removed.then_some(&name) })
                ));
            } else if removed {
                ui.err_line(&format!("removed credential for {name}"));
            } else {
                ui.err_line(&format!("no credential saved for {name}"));
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Set { name, env }) => {
            let (key, _) = credential_key(&store, name);
            let token = read_secret(ui, env.as_deref(), "token")?;
            let mut cred = store.credential(&key)?.unwrap_or_default();
            cred.access_token = Some(token);
            cred.expires_at = None;
            cred.source = Some("manual".into());
            store.save_credential(&key, cred)?;
            if cli.json {
                print_value(ui, &json!({ "saved_credential": key }), true);
            } else {
                ui.err_line(&format!("saved token for {key}"));
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Show { name }) => {
            let (key, _) = credential_key(&store, name);
            let Some(cred) = store.credential(&key)? else {
                return Err(Error::config(format!("no credential saved for {key}")).into());
            };
            if cli.json {
                // Metadata only. The secrets never leave the file through this path.
                print_json(
                    ui,
                    &json!({
                        "name": key,
                        "has_access_token": cred.has_token(),
                        "has_refresh_token": cred.refresh_token.is_some(),
                        "expires_at": cred.expires_at,
                        "expired": cred.is_expired(),
                        "scope": cred.scope,
                        "source": cred.source,
                        "client_id": cred.client_id,
                        "registration": cred.registration,
                        "has_client_secret": cred.client_secret.is_some(),
                        "issuer": cred.issuer,
                        "token_endpoint": cred.token_endpoint,
                    }),
                );
            } else {
                ui.line(&key);
                ui.line(&format!(
                    "  access token:  {}",
                    if cred.has_token() { "present" } else { "none" }
                ));
                ui.line(&format!("  expiry:        {}", expiry_label(&cred)));
                ui.line(&format!(
                    "  refresh token: {}",
                    if cred.refresh_token.is_some() {
                        "present"
                    } else {
                        "none"
                    }
                ));
                ui.line(&format!(
                    "  source:        {}",
                    cred.source.as_deref().unwrap_or("?")
                ));
                if let Some(s) = &cred.scope {
                    ui.line(&format!("  scope:         {s}"));
                }
                if let Some(c) = &cred.client_id {
                    ui.line(&format!("  client id:     {c}"));
                }
                if let Some(r) = cred
                    .registration
                    .as_deref()
                    .and_then(oauth::Registration::parse)
                {
                    ui.line(&format!("  registered:    {}", r.describe()));
                }
                if cred.client_secret.is_some() {
                    ui.line(&format!(
                        "  client secret: present ({})",
                        cred.token_endpoint_auth_method
                            .as_deref()
                            .unwrap_or(oauth::CLIENT_SECRET_POST)
                    ));
                }
                if let Some(i) = &cred.issuer {
                    ui.line(&format!("  issuer:        {i}"));
                }
                if let Some(t) = &cred.token_endpoint {
                    ui.line(&format!("  token url:     {t}"));
                }
            }
            Ok(0)
        }
    }
}

const LISTING_HEADERS: [&str; 8] = [
    "NAME", "TYPE", "STATUS", "AGE", "AUTH", "DAEMON", "SERVER", "TOOLS",
];

/// `tools` with no target: the probes under a key, as `tools TARGET` puts its
/// own list under `tools`. A bare array could never grow a field beside them.
#[derive(serde::Serialize)]
struct Servers<'a> {
    servers: &'a [client::Probe],
}

/// What was saved about one server, as every `ls --json` row carries it. A probe
/// adds its status fields on top of these rather than in place of them, so a
/// program reading a row never has to know whether `--no-probe` was passed.
fn saved_row(name: &str, cfg: &ServerConfig, credential: bool, running: bool) -> Value {
    json!({
        "name": name, "kind": cfg.kind(), "location": cfg.location(),
        "headers": cfg.headers, "token_env": cfg.token_env,
        "credential": credential,
        "source": cfg.source, "timeout": cfg.timeout,
        "running": running,
        "allow": cfg.allow, "deny": cfg.deny,
    })
}

/// One server as `ls` shows it, which is also what `add` shows after dialing.
fn listing_row(l: &Listing) -> Vec<String> {
    vec![
        l.name.clone(),
        l.kind.into(),
        l.status.label(),
        age_label(l.age_seconds),
        auth_label(l),
        daemon_label(l.running),
        l.server
            .clone()
            .or_else(|| l.status.detail().map(truncate))
            .unwrap_or_else(|| "-".into()),
        l.tools.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
    ]
}

fn auth_label(l: &Listing) -> String {
    match (l.auth, &l.status) {
        (client::AuthUsed::Env, _) => l
            .token_env
            .as_ref()
            .map_or_else(|| "env".into(), |var| format!("${var}")),
        (client::AuthUsed::Saved, _) => "saved".into(),
        (client::AuthUsed::None, Status::AuthRequired) => "needed".into(),
        (client::AuthUsed::None, Status::TokenRejected) => "rejected".into(),
        (client::AuthUsed::None, _) => "-".into(),
    }
}

fn daemon_label(running: bool) -> String {
    if running { "running" } else { "-" }.into()
}

/// `--idle SECS` as a duration; zero or less would be a daemon that quits at once.
fn idle_duration(secs: Option<f64>) -> Result<Option<Duration>, Failure> {
    match secs {
        None => Ok(None),
        Some(s) if s > 0.0 => Ok(Some(Duration::from_secs_f64(s))),
        Some(_) => Err(Error::usage("--idle needs a positive number of seconds").into()),
    }
}

fn age_label(seconds: u64) -> String {
    match seconds {
        0 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s => format!("{}m", s / 60),
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
    truncate_at(s.lines().next().unwrap_or(""), 60)
}

fn print_tools(ui: &dyn Presenter, tools: &[Value], long: bool) {
    // A `--long` listing has room to spell out what calling a tool does; the
    // short one carries only the hint a caller cannot afford to miss.
    let hints: &dyn Fn(&Value) -> String = if long {
        &client::hint_tags
    } else {
        &client::hint_mark
    };
    ui.named(tools, long, "parameters", &describe_params, hints);
}

/// A `--long` listing reads like a document and may run past the screen; the
/// short one is a summary that belongs on it.
fn listed(ui: &dyn Presenter, long: bool, print: impl FnOnce()) {
    if long {
        ui.paged(print);
    } else {
        print();
    }
}

fn print_prompts(ui: &dyn Presenter, prompts: &[Value], long: bool) {
    ui.named(prompts, long, "arguments", &describe_prompt_args, &|_| {
        String::new()
    });
}

/// A prompt's arguments carry no schema: every one of them is a string.
fn describe_prompt_args(prompt: &Value) -> Vec<String> {
    prompt["arguments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|a| {
            let mut line = a["name"].as_str().unwrap_or("?").to_string();
            if a["required"].as_bool().unwrap_or(false) {
                line.push_str(" (required)");
            }
            if let Some(d) = a["description"].as_str() {
                let first = d.trim().lines().next().unwrap_or("");
                if !first.is_empty() {
                    line.push_str(&format!(" - {first}"));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_files_are_named_after_their_source_and_never_overwritten() {
        assert_eq!(file_stem("take_screenshot"), "take_screenshot");
        assert_eq!(file_stem("shot/one:two"), "shot_one_two");
        assert_eq!(file_stem(".."), "media");
        assert_eq!(resource_stem("file:///logo.png"), "logo");
        assert_eq!(resource_stem("file:///dir/a.b.c/"), "a.b");
        assert_eq!(resource_stem("https://host/x?y=1#z"), "x");
        assert_eq!(resource_stem("file:///.hidden"), ".hidden");
        assert_eq!(resource_stem(""), "resource");

        let dir = std::env::temp_dir().join(format!(
            "mcpdial-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png = Media {
            kind: "image".into(),
            mime_type: "image/png".into(),
            bytes: b"\x89PNG".to_vec(),
        };
        let files = MediaFiles {
            dir: Some(&dir),
            stem: "shot".into(),
        };
        assert_eq!(files.place(&png).unwrap(), Some(dir.join("shot-1.png")));
        assert_eq!(files.place(&png).unwrap(), Some(dir.join("shot-2.png")));
        assert_eq!(std::fs::read(dir.join("shot-1.png")).unwrap(), b"\x89PNG");
        let nowhere = MediaFiles {
            dir: None,
            stem: "shot".into(),
        };
        assert_eq!(nowhere.place(&png).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    #[test]
    fn a_location_is_checked_before_it_is_saved() {
        assert!(validate_location(&ServerConfig::http("https://mcp.deepwiki.com/mcp")).is_ok());
        assert!(validate_location(&ServerConfig::http("http://127.0.0.1:8080")).is_ok());
        // A placeholder is filled in at dial time; the shape around it is what counts.
        assert!(validate_location(&ServerConfig::http("https://${HOST}/mcp")).is_ok());
        for bad in [
            "notaurl",
            "ftp://x/mcp",
            "http://",
            "https://a b/mcp",
            "mcp.example.com/mcp",
        ] {
            let e = validate_location(&ServerConfig::http(bad)).unwrap_err();
            assert!(matches!(e, Error::Usage(_)), "{bad}: {e}");
            assert!(e.to_string().contains(bad), "{bad}: {e}");
        }
        assert!(validate_location(&ServerConfig::stdio("npx -y thing /tmp")).is_ok());
        assert!(validate_location(&ServerConfig::stdio("   ")).is_err());
        assert!(validate_location(&ServerConfig::stdio("'unterminated")).is_err());
    }

    #[test]
    fn a_timeout_is_checked_before_it_is_saved() {
        assert_eq!(validate_timeout(0.2).unwrap(), 0.2);
        assert_eq!(validate_timeout(0.0).unwrap(), 0.0);
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let e = validate_timeout(bad).unwrap_err();
            assert!(matches!(e, Error::Usage(_)), "{bad}: {e}");
        }
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
