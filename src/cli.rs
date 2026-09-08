//! Every flag and every subcommand mcpdial takes, and the words that describe them.
//!
//! Clap's derive keeps the parser and its help text in one place, which makes this
//! file the reference for what the tool accepts - and `tests/contract.rs` holds
//! `call --help` byte for byte, so a line moved here is a change to the contract.
//! It sits apart from the code that runs the commands because the two change for
//! different reasons.

use crate::present::style::ColorMode;
use crate::{grep, tasks};
use clap::{Parser, Subcommand};
use clap_complete::Shell;
use mcpdial::{KnownVersion, Level};
use std::path::PathBuf;

/// The positionals more than one subcommand takes, worded once so they cannot
/// drift apart.
pub(crate) const TARGET_HELP: &str = "A saved name, an http(s):// URL, or stdio:<command>";
pub(crate) const SAVED_NAME_HELP: &str = "A saved server (`mcpdial ls` lists them)";
pub(crate) const CREDENTIAL_TARGET_HELP: &str =
    "A saved name, or the http(s):// URL a token was saved for";

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
pub(crate) struct Cli {
    /// Seconds to wait for a reply; else the server's saved timeout, else 60. With `add`, saved.
    /// (MCPDIAL_TIMEOUT=SECS does the same)
    #[arg(long, global = true, value_name = "SECS")]
    pub(crate) timeout: Option<f64>,

    /// Emit JSON instead of a readable summary (MCPDIAL_JSON=1 does the same)
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Print what a pipe would get, even at a terminal (MCPDIAL_PLAIN=1 does the same)
    #[arg(long, global = true)]
    pub(crate) plain: bool,

    /// Print long output straight to the terminal instead of through a pager
    /// (MCPDIAL_PAGER= does the same everywhere)
    #[arg(long, global = true)]
    pub(crate) no_pager: bool,

    /// Print a server's text as it was written instead of rendering its
    /// markdown, for copying it out of the terminal
    #[arg(long, global = true)]
    pub(crate) raw: bool,

    /// Colour the terminal output: `always` (for `less -R`), `never`, or `auto`
    /// (at a terminal, unless NO_COLOR is set)
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub(crate) color: ColorMode,

    /// Trace every message on stderr (and pass a stdio server's stderr through)
    #[arg(short, long, global = true)]
    pub(crate) verbose: bool,

    /// Print a line on stderr for every progress notification a server sends
    /// during a call, whether or not stderr is a terminal
    #[arg(long, global = true)]
    pub(crate) progress: bool,

    /// Show a server's own log notifications from this level up (default
    /// warning), and ask a server that supports logging to send no less
    #[arg(long, global = true, value_name = "LEVEL")]
    pub(crate) log_level: Option<Level>,

    /// Append every message and transport event to FILE as JSON Lines, secrets
    /// redacted (MCPDIAL_TRACE=FILE does the same everywhere)
    #[arg(long, global = true, value_name = "FILE")]
    pub(crate) trace: Option<PathBuf>,

    /// Override the User-Agent (HTTP only) (MCPDIAL_USER_AGENT does the same)
    #[arg(long, global = true)]
    pub(crate) user_agent: Option<String>,

    /// Extra HTTP header, repeatable. With `add`, saved to the server.
    #[arg(
        short = 'H',
        long = "header",
        global = true,
        value_name = "'Name: value'"
    )]
    pub(crate) headers: Vec<String>,

    /// Env var holding a bearer token; beats any saved credential. With `add`, saved.
    /// A `${VAR}` in any `-H` header value does the same for that header.
    #[arg(long, global = true, value_name = "VAR")]
    pub(crate) token_env: Option<String>,

    /// MCP protocol revision to speak, instead of working out which the server does.
    /// With `add`, saved.
    #[arg(long, global = true, value_name = "VERSION")]
    pub(crate) protocol_version: Option<KnownVersion>,
    /// Fail on the first transient HTTP failure instead of sending the request once more
    #[arg(long, global = true)]
    pub(crate) no_retry: bool,

    /// Write image, audio and blob blocks to DIR/<tool>-<n>.<ext> instead of
    /// printing them (call, prompt, read, shell)
    #[arg(long, global = true, value_name = "DIR")]
    pub(crate) save_dir: Option<PathBuf>,

    /// Show at most N characters of a result; under --json a `truncated` object
    /// replaces it (call, prompt, read, raw, shell; MCPDIAL_MAX_CHARS=N does the same)
    #[arg(long, global = true, value_name = "N")]
    pub(crate) max_chars: Option<usize>,

    /// Write the whole result to FILE, which must not exist, and print one line
    /// saying so (call, prompt, read, raw, shell)
    #[arg(short = 'o', long, global = true, value_name = "FILE")]
    pub(crate) output: Option<PathBuf>,

    /// Dial a stdio server afresh even when `start` left one running
    /// (MCPDIAL_NO_DAEMON=1 does the same everywhere)
    #[arg(long, global = true)]
    pub(crate) no_daemon: bool,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
    /// Pick a saved server and one of its tools, then print the call it made
    Pick,
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
        /// Have the server run TOOL in the background, and poll until it finishes
        #[arg(long, conflicts_with = "detach")]
        task: bool,
        /// Start TOOL in the background, print the task id, and exit
        #[arg(long)]
        detach: bool,
        /// Seconds the server is asked to keep a --task or --detach task for (default 3600)
        #[arg(long, value_name = "SECS")]
        ttl: Option<u64>,
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
    /// Suggest values for a prompt argument or a resource template variable
    Complete {
        #[arg(help = TARGET_HELP)]
        target: String,
        #[command(subcommand)]
        of: Completing,
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
    /// List the background tasks a server is running, or get, wait for, or cancel one
    Tasks(tasks::Flags),
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
    /// Show or change how mcpdial itself behaves
    #[command(subcommand)]
    Config(ConfigCmd),
}

/// What `complete` is asking about: the two things a server may be asked to
/// suggest values for, and nothing else - the spec names no third reference.
#[derive(Subcommand)]
pub(crate) enum Completing {
    /// One argument of a prompt
    Prompt {
        /// A prompt name from `mcpdial prompts TARGET`
        name: String,
        /// The argument to suggest values for
        argument: String,
        /// What has been typed of it so far; empty asks for everything
        #[arg(default_value = "")]
        value: String,
        /// The arguments already settled, which the server may narrow by: a JSON object or @file
        #[arg(long, value_name = "JSON")]
        context: Option<String>,
    },
    /// One variable of a resource template
    Resource {
        /// A uriTemplate from `mcpdial resources TARGET`
        template: String,
        /// The variable to suggest values for
        variable: String,
        /// What has been typed of it so far; empty asks for everything
        #[arg(default_value = "")]
        value: String,
        /// The variables already settled, which the server may narrow by: a JSON object or @file
        #[arg(long, value_name = "JSON")]
        context: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    /// Show where saved tokens are kept, or move them: `file` (default) or `keychain`
    Credentials {
        /// The store to move every saved token into. Omit to show the current one.
        #[arg(value_name = "STORE", value_parser = ["file", "keychain"])]
        store: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum TokenCmd {
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
