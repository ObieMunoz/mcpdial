use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use mcpdial::catalog;
use mcpdial::client::{self, describe_params, Listing, Options, Status};
use mcpdial::config::Source;
use mcpdial::protocol::METHOD_NOT_FOUND;
use mcpdial::registry::{Pick, Registry, Resolved};
use mcpdial::session::{render_messages, resource_bodies, ResourceBody};
use mcpdial::{oauth, Credential, Error, KnownVersion, ServerConfig, Store, USER_AGENT};
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
  mcpdial catalog                 # a reviewed list of servers, by category
  mcpdial add ctx7 --catalog context7                      # one of them
  mcpdial add ctx7 --registry io.github.upstash/context7   # from the MCP registry
  mcpdial ls                      # every saved server with its status and its age
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
    /// A `${VAR}` in any `-H` header value does the same for that header.
    #[arg(long, global = true, value_name = "VAR")]
    token_env: Option<String>,

    /// MCP protocol version to offer at initialize instead of the newest. With `add`, saved.
    #[arg(long, global = true, value_name = "VERSION")]
    protocol_version: Option<KnownVersion>,

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
        /// Replace a server already saved under this name
        #[arg(long)]
        force: bool,
        /// Save without dialing the server for its status
        #[arg(long)]
        no_probe: bool,
    },
    /// Import servers from a host's config (Claude Code, Claude Desktop, Cursor, ...)
    Import {
        /// A JSON file with an `mcpServers` object. Omit to scan the usual locations.
        file: Option<std::path::PathBuf>,
        /// Overwrite servers that already exist under the same name
        #[arg(long)]
        force: bool,
    },
    /// List the curated catalog of servers, grouped by category
    Catalog {
        /// Use the copy built into the binary instead of refreshing it
        #[arg(long)]
        offline: bool,
    },
    /// Keep one session open and run commands from stdin (state persists between calls)
    Shell { target: String },
    /// Forget a server and any credential saved for it
    Rm { name: String },
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
    /// Show one tool's name, description, and input and output schemas
    Schema { target: String, tool: String },
    /// Show the resources a server offers, and its URI templates
    Resources {
        target: String,
        /// Show full descriptions and mime types
        #[arg(short, long)]
        long: bool,
    },
    /// Read one resource. Text goes to stdout; binary needs a redirect
    Read { target: String, uri: String },
    /// Show the prompts a server offers
    Prompts {
        target: String,
        /// Show full descriptions and arguments
        #[arg(short, long)]
        long: bool,
    },
    /// Render a prompt into the messages it expands to
    Prompt {
        target: String,
        name: String,
        /// JSON object of arguments: inline, @file, or - for stdin
        #[arg(default_value = "{}")]
        arguments: String,
    },
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
    /// Print a shell completion script for bash, zsh, fish, elvish or powershell
    #[command(hide = true)]
    Completions { shell: Shell },
    /// Authorize in the browser once and save the token (HTTP servers)
    Login {
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
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let hint = split_object_hint(&e);
        let _ = e.print();
        if let Some(hint) = hint {
            eprintln!("{hint}");
        }
        std::process::exit(e.exit_code());
    });
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            if json {
                eprintln!("{}", f.to_json());
            } else {
                f.eprint();
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
}

impl Failure {
    fn hinted(error: Error, hint: impl Into<String>) -> Self {
        Self {
            error,
            hint: Some(hint.into()),
        }
    }

    fn eprint(&self) {
        eprintln!("error: {}", self.error);
        if let Some(hint) = &self.hint {
            eprintln!("{hint}");
        }
    }

    fn to_json(&self) -> Value {
        let mut v = error_json(&self.error);
        if let Some(hint) = &self.hint {
            v["error"]["hint"] = json!(hint);
        }
        v
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self { error, hint: None }
    }
}

fn print_json(v: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("a JSON value is serializable")
    );
}

fn print_value(v: &Value, compact: bool) {
    if compact {
        println!("{v}");
    } else {
        print_json(v);
    }
}

/// A hint on stderr: prose for a human, `{"hint": ...}` under `--json`, where
/// every line on either stream has to be an object.
fn print_hint(hint: &str, json: bool) {
    if json {
        eprintln!("{}", json!({ "hint": hint }));
    } else {
        eprintln!("{hint}");
    }
}

/// A tool result on stdout, and a marker on stderr when the tool reported an
/// error: a failure whose text is a plain sentence otherwise reads as success
/// to anyone not checking `$?`. Under `--json` the object carries `isError`
/// itself. Returns whether the tool reported an error.
fn print_tool_result(result: &Value, text: &str, json: bool, one_line: bool) -> bool {
    let failed = result["isError"].as_bool().unwrap_or(false);
    if json {
        print_value(result, one_line);
    } else {
        if !text.is_empty() {
            print_text(text);
        }
        if failed {
            eprintln!("(tool reported an error)");
        }
    }
    failed
}

fn dial(store: &Store, opts: &Options, target: &str) -> Result<client::Connection, Failure> {
    let r = client::resolve(store, target)?;
    Ok(client::connect(store, &r, opts)?)
}

fn info_hint(target: &str) -> String {
    format!("`mcpdial info {}`", shell_word(target))
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
    let registry = Registry::from_env(opts.timeout, &opts.user_agent);
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
fn from_catalog(store: &Store, opts: &Options, id: &str) -> Result<Resolved, Failure> {
    let loaded = catalog::load(
        store,
        &catalog::Source::from_env(),
        false,
        opts.timeout,
        &opts.user_agent,
    )?;
    if opts.verbose {
        eprintln!("catalog: {}", loaded.origin);
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
    let registry = Registry::from_env(opts.timeout, &opts.user_agent);
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

/// The catalog as a person reads it: one block per category, one line per entry.
fn print_catalog(entries: &[catalog::Entry]) {
    let width = |pick: fn(&catalog::Entry) -> &str| {
        entries
            .iter()
            .map(|e| pick(e).chars().count())
            .max()
            .unwrap_or(0)
    };
    let (id_w, name_w) = (width(|e| &e.id), width(|e| &e.name));
    for (i, (category, group)) in catalog::grouped(entries).iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{category}");
        for e in group {
            println!(
                "  {:<id_w$}  {:<name_w$}  {:<5}  {:<7}  {}",
                e.id,
                e.name,
                e.transport.as_str(),
                e.auth.as_str(),
                e.summary
            );
        }
    }
}

/// A note on stderr: `note:` before the first line, the rest indented under it.
fn print_note(note: &str) {
    let mut lines = note.lines();
    if let Some(first) = lines.next() {
        eprintln!("note: {first}");
    }
    for line in lines {
        eprintln!("      {line}");
    }
}

fn credential_key(store: &Store, target: String) -> String {
    client::resolve(store, &target)
        .map(|r| r.name)
        .unwrap_or(target)
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
fn read_secret(env: Option<&str>, what: &str) -> Result<String, Error> {
    if let Some(var) = env {
        return std::env::var(var)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::usage(format!("${var} is unset or empty")));
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("paste the {what} and press enter: ");
        std::io::stderr().flush().ok();
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

/// The hint when the shell split an unquoted JSON object at its commas, so that
/// `{"a": 1, "b": 2}` reached clap as the two words `a:1` and `b:2`.
fn split_object_hint(e: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if e.kind() != ErrorKind::UnknownArgument {
        return None;
    }
    let ContextValue::String(word) = e.get(ContextKind::InvalidArg)? else {
        return None;
    };
    let looks_like_field = word.contains(':') && !word.starts_with('-');
    let takes_json = std::env::args().any(|a| a == "call" || a == "prompt");
    (looks_like_field && takes_json).then(|| {
        "the shell split a JSON object at its commas; single quotes keep it whole: '{\"key\": \"value\", ...}'".to_string()
    })
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

/// Levenshtein distance, for "did you mean" suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
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
    let mut out = format!(
        "usage: {prefix} {} {quote}{}{quote}",
        tool["name"].as_str().unwrap_or("?"),
        client::example_arguments(tool)
    );
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

/// The hint that answers "what shape did you want?" after a call went wrong:
/// the tool's own usage when the tool exists, a near miss when it does not.
fn call_hint(
    tools: &[Value],
    name: &str,
    prefix: &str,
    quote: &str,
    tools_cmd: &str,
) -> Option<String> {
    match find_tool(tools, name) {
        Some(t) => Some(tool_usage(t, prefix, quote)),
        None => suggest_tool(tools, name, tools_cmd),
    }
}

/// A server error that the tool's schema would have prevented. Any other error
/// is the tool's own failure, and printing a schema under it is just noise.
fn is_argument_error(e: &Error) -> bool {
    match e {
        Error::Rpc { code: -32602, .. } => true,
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
/// which reads as a mistake in the request. Its `initialize` result already listed
/// what it does implement, so point there instead of at the bare code.
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

/// Rendered tool or prompt text on stdout. On a terminal the control characters
/// that move the cursor or open an escape sequence are shown as escapes instead:
/// a server that lists `\r` among its valid keys otherwise overwrites the start
/// of its own error message. Newlines and tabs are the text's own layout and
/// stay. A pipe gets the text as the server sent it.
fn print_text(text: &str) {
    if std::io::stdout().is_terminal() {
        println!("{}", visible(text));
    } else {
        println!("{text}");
    }
}

fn visible(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => write!(out, "\\x{:02x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out
}

fn advertises(server_info: &Value, capability: &str) -> bool {
    server_info["capabilities"].get(capability).is_some()
}

/// A resource on stdout, byte for byte. Base64 is no use to anyone reading it and
/// raw bytes corrupt a terminal, so binary asks for a redirect rather than picking
/// one of those two ways to be useless.
fn write_resource(bodies: &[ResourceBody], redirect: &str) -> Result<(), Failure> {
    let stdout = std::io::stdout();
    let binary = bodies.iter().any(|b| matches!(b, ResourceBody::Bytes(_)));
    if binary && stdout.is_terminal() {
        return Err(Failure::hinted(
            Error::usage("this resource is binary and stdout is a terminal"),
            format!("send it somewhere it can land: {redirect} > file"),
        ));
    }
    let mut out = stdout.lock();
    for body in bodies {
        let bytes = match body {
            ResourceBody::Text(t) => t.as_bytes(),
            ResourceBody::Bytes(b) => b.as_slice(),
        };
        out.write_all(bytes)
            .map_err(|e| Failure::from(Error::transport(format!("writing to stdout: {e}"))))?;
    }
    out.flush().ok();
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
    "info",
    "help",
    "quit",
];

const SHELL_SUMMARY: &str = "commands: tools, schema TOOL, call TOOL {\"arg\": \"value\"}, \
     resources, read URI, prompts, prompt NAME, raw METHOD, info, help, quit";

const SHELL_HELP: &str = r#"commands (one per line; # starts a comment):
  tools [--long]             every tool this server offers
  schema TOOL                one tool's full JSON input schema
  help [TOOL]                this list, or one tool's parameters
  call TOOL {"arg": "value"} call a tool; arguments are one JSON object, default {}
  resources [--long]         every resource, then every URI template
  read URI                   one resource's contents
  prompts [--long]           every prompt this server offers
  prompt NAME {"arg": "..."} expand a prompt into its messages
  raw METHOD {"json": ...}   send any JSON-RPC method
  info                       the initialize result
  quit                       close the session

At a terminal: Up and Down walk the history, Tab completes commands, tool and
prompt names and resource URIs, and ^C abandons the line being typed."#;

/// The answer to a tool name this server does not have.
fn no_such_tool(tools: &[Value], name: &str) -> Failure {
    Failure {
        error: Error::usage(format!("no tool named {name:?}")),
        hint: suggest_tool(tools, name, "`tools`"),
    }
}

/// [`call_hint`] for a `call` line typed at the shell, where the arguments are
/// written bare and the tool list comes from the session already open.
fn shell_call_hint(
    cache: &mut Option<Vec<Value>>,
    conn: &mut client::Connection,
    tool: &str,
) -> Option<String> {
    call_hint(shell_tools(cache, conn), tool, "call", "", "`tools`")
}

/// Tab completion for the shell: command names in the first word, then whatever
/// the command that was typed takes as its one argument.
#[derive(Default)]
struct ShellHelper {
    tools: Vec<String>,
    resources: Vec<String>,
    prompts: Vec<String>,
}

impl rustyline::completion::Completer for ShellHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let head = line.get(..pos).unwrap_or(line);
        let start = head
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let (before, word) = head.split_at(start);
        let pool: Vec<String> = match before.split_whitespace().collect::<Vec<_>>()[..] {
            [] => SHELL_COMMANDS.iter().map(|c| c.to_string()).collect(),
            ["call" | "schema" | "help"] => self.tools.clone(),
            ["read"] => self.resources.clone(),
            ["prompt"] => self.prompts.clone(),
            _ => Vec::new(),
        };
        Ok((
            start,
            pool.into_iter().filter(|c| c.starts_with(word)).collect(),
        ))
    }
}

impl rustyline::highlight::Highlighter for ShellHelper {}
impl rustyline::validate::Validator for ShellHelper {}
impl rustyline::hint::Hinter for ShellHelper {
    type Hint = String;
}
impl rustyline::Helper for ShellHelper {}

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
    fn next(&mut self) -> Result<Option<String>, Error> {
        match self {
            Input::Pipe { stdin, prompt } => {
                if let Some(p) = prompt {
                    eprint!("{p}");
                    std::io::stderr().flush().ok();
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
                        eprintln!("(^C again, or `quit`, to exit)");
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
                helper.tools = field_values(tools, "name");
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
        .get_or_insert_with(|| conn.session.list_tools().unwrap_or_default())
        .as_slice()
}

fn run(cli: Cli) -> Result<u8, Failure> {
    let store = Store::from_env()?;
    let opts = Options {
        timeout: Duration::from_secs_f64(cli.timeout.max(0.0)),
        user_agent: cli.user_agent.clone(),
        extra_headers: parse_headers(&cli.headers)?,
        token_env: cli.token_env.clone(),
        protocol_version: cli.protocol_version,
        verbose: cli.verbose,
    };

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
            force,
            no_probe,
        } => {
            // A registry entry is saved without being run, as documented: its
            // command usually needs values the user has yet to supply.
            let dial = !no_probe && registry.is_none();
            let mut notes = Vec::new();
            let mut cfg = match (http, stdio, registry) {
                (None, None, None) if catalog.is_some() => {
                    let resolved = from_catalog(&store, &opts, catalog.as_deref().unwrap_or(""))?;
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
            validate_location(&cfg)?;
            if let Some(old) = store.server(&name)?.filter(|_| !force) {
                return Err(Error::usage(format!(
                    "{name} is already saved ({} {}); pass --force to replace it",
                    old.kind(),
                    old.location()
                ))
                .into());
            }
            let summary = format!("{} {}", cfg.kind(), cfg.location());
            let mut saved = json!({ "name": name, "kind": cfg.kind(), "location": cfg.location() });
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
                println!("{}", json!({ "saved": saved }));
            } else {
                eprintln!("saved {name} ({summary})");
                for note in &notes {
                    print_note(note);
                }
                if let Some(row) = &row {
                    print_table(&LISTING_HEADERS, &[listing_row(row)]);
                }
            }
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
                )
                .into());
            }
            let existing = store.servers()?;
            let mut imported: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            // The running commentary is for a human; a program gets one object at the end.
            let say = |line: String| {
                if !cli.json {
                    eprintln!("{line}");
                }
            };
            for path in &files {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                let doc: Value = serde_json::from_str(&text)
                    .map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
                let found = mcpdial::import_config::extract(&doc);
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
                            if let Some(n) = f.note {
                                say(format!("       note: {n}"));
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
                println!("{}", json!({ "imported": imported, "skipped": skipped }));
            } else {
                eprintln!("imported {} server(s)", imported.len());
            }
            Ok(0)
        }

        Cmd::Shell { target } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let si = &conn.server_info["serverInfo"];
            let interactive = std::io::stdin().is_terminal();
            if interactive {
                eprintln!(
                    "connected to {} {}",
                    si["name"].as_str().unwrap_or("?"),
                    si["version"].as_str().unwrap_or("")
                );
                eprintln!("{SHELL_SUMMARY}");
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
            while let Some(raw) = input.next()? {
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
                        eprintln!("{SHELL_HELP}");
                        Ok(())
                    }
                    "help" => {
                        let tools = shell_tools(&mut cache, &mut conn);
                        match find_tool(tools, rest) {
                            Some(t) => {
                                eprintln!("{}", tool_usage(t, "call", ""));
                                Ok(())
                            }
                            None => Err(no_such_tool(tools, rest)),
                        }
                    }
                    "schema" if rest.is_empty() => Err(Failure::hinted(
                        Error::usage("schema needs a tool name"),
                        "usage: schema TOOL   (`tools` lists what this server offers)",
                    )),
                    "schema" => {
                        let tools = shell_tools(&mut cache, &mut conn);
                        match find_tool(tools, rest) {
                            Some(t) => {
                                print_value(t, cli.json);
                                Ok(())
                            }
                            None => Err(no_such_tool(tools, rest)),
                        }
                    }
                    "info" => {
                        print_value(&conn.server_info, cli.json);
                        Ok(())
                    }
                    "tools" => conn
                        .session
                        .list_tools()
                        .map_err(Failure::from)
                        .map(|tools| {
                            if cli.json {
                                println!("{}", json!({ "tools": tools }));
                            } else {
                                println!("{} tool(s):", tools.len());
                                print_tools(&tools, long);
                            }
                            cache = Some(tools);
                        }),
                    "resources" => match conn.session.list_resources() {
                        Err(e) => Err(missing_capability(e, "resources", info_cmd)),
                        Ok(found) => {
                            let templates =
                                conn.session.list_resource_templates().unwrap_or_default();
                            if cli.json {
                                println!(
                                    "{}",
                                    json!({"resources": found, "resourceTemplates": templates})
                                );
                            } else {
                                println!("{} resource(s):", found.len());
                                print_resources(&found, long);
                                if !templates.is_empty() {
                                    println!("\n{} template(s):", templates.len());
                                    print_resources(&templates, long);
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
                    "read" => match conn.session.read_resource(rest) {
                        Err(e) => Err(missing_capability(e, "resources", info_cmd)),
                        Ok(result) if cli.json => {
                            println!("{result}");
                            Ok(())
                        }
                        Ok(result) => {
                            resource_bodies(&result)
                                .map_err(Failure::from)
                                .and_then(|bodies| {
                                    let redirect =
                                        format!("mcpdial read {} {}", shell_word(&target), rest);
                                    write_resource(&bodies, &redirect)
                                })
                        }
                    },
                    "prompts" => match conn.session.list_prompts() {
                        Err(e) => Err(missing_capability(e, "prompts", info_cmd)),
                        Ok(found) => {
                            if cli.json {
                                println!("{}", json!({ "prompts": found }));
                            } else {
                                println!("{} prompt(s):", found.len());
                                print_prompts(&found, long);
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
                            parse_object(args, "arguments")
                                .and_then(|a| conn.session.get_prompt(name, a))
                                .map_err(|e| missing_capability(e, "prompts", info_cmd))
                                .map(|result| {
                                    if cli.json {
                                        println!("{result}");
                                    } else {
                                        let messages = render_messages(&result);
                                        if !messages.is_empty() {
                                            print_text(&messages);
                                        }
                                    }
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
                        } else {
                            // Arguments that do not parse and arguments the server
                            // rejects mean the same thing to whoever typed the line:
                            // show them what this tool takes.
                            match parse_object(args, "arguments") {
                                Err(e) => Err(Failure {
                                    error: e,
                                    hint: shell_call_hint(&mut cache, &mut conn, tool),
                                }),
                                Ok(a) => match conn.session.call_tool(tool, a) {
                                    Ok(result) => {
                                        let out = mcpdial::render_content(&result);
                                        let failed =
                                            print_tool_result(&result, &out, cli.json, true);
                                        if failed && reads_as_argument_error(&out) {
                                            if let Some(hint) =
                                                shell_call_hint(&mut cache, &mut conn, tool)
                                            {
                                                print_hint(&hint, cli.json);
                                            }
                                        }
                                        Ok(())
                                    }
                                    Err(e) => {
                                        let hint = is_argument_error(&e)
                                            .then(|| shell_call_hint(&mut cache, &mut conn, tool))
                                            .flatten();
                                        Err(Failure { error: e, hint })
                                    }
                                },
                            }
                        }
                    }
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
                                .map(|result| print_value(&result, cli.json))
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
                        println!("{}", f.to_json());
                    } else {
                        f.eprint();
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

        Cmd::Schema { target, tool } => {
            let mut conn = dial(&store, &opts, &target)?;
            let tools = conn.session.list_tools()?;
            let Some(t) = find_tool(&tools, &tool) else {
                let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
                return Err(Failure {
                    error: Error::Rpc {
                        code: -32602,
                        message: format!("Tool {tool} not found; available: {}", names.join(", ")),
                        data: None,
                    },
                    hint: closest(&tool, names.iter().copied())
                        .map(|near| format!("did you mean {near}?")),
                });
            };
            print_json(t);
            if t.get("outputSchema").is_some() {
                eprintln!("this tool declares an outputSchema: results carry structuredContent");
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
                print_json(&json!({ "resources": resources, "resourceTemplates": templates }));
            } else {
                println!("{} resource(s):\n", resources.len());
                print_resources(&resources, long);
                if !templates.is_empty() {
                    println!("\n{} template(s):\n", templates.len());
                    print_resources(&templates, long);
                }
            }
            Ok(0)
        }

        Cmd::Read { target, uri } => {
            let mut conn = dial(&store, &opts, &target)?;
            let result = conn
                .session
                .read_resource(&uri)
                .map_err(|e| missing_capability(e, "resources", &info_hint(&target)))?;
            if cli.json {
                print_json(&result);
                return Ok(0);
            }
            let redirect = format!("mcpdial read {} {}", shell_word(&target), shell_word(&uri));
            write_resource(&resource_bodies(&result)?, &redirect)?;
            Ok(0)
        }

        Cmd::Prompts { target, long } => {
            let mut conn = dial(&store, &opts, &target)?;
            let prompts = conn
                .session
                .list_prompts()
                .map_err(|e| missing_capability(e, "prompts", &info_hint(&target)))?;
            if cli.json {
                print_json(&json!({ "prompts": prompts }));
            } else {
                println!("{} prompt(s):\n", prompts.len());
                print_prompts(&prompts, long);
            }
            Ok(0)
        }

        Cmd::Prompt {
            target,
            name,
            arguments,
        } => {
            let arguments = read_json_arg(&arguments, "arguments").map_err(|e| {
                Failure::hinted(
                    e,
                    json_arg_hint(
                        &arguments,
                        &format!("mcpdial prompt {} {name}", shell_word(&target)),
                        format!(
                            "`mcpdial prompts {} --long` shows what {name} takes",
                            shell_word(&target)
                        ),
                    ),
                )
            })?;
            let mut conn = dial(&store, &opts, &target)?;
            let result = conn
                .session
                .get_prompt(&name, arguments)
                .map_err(|e| missing_capability(e, "prompts", &info_hint(&target)))?;
            if cli.json {
                print_json(&result);
            } else {
                if let Some(d) = result["description"].as_str() {
                    eprintln!("{d}");
                }
                let messages = render_messages(&result);
                if !messages.is_empty() {
                    print_text(&messages);
                }
            }
            Ok(0)
        }

        Cmd::Guide => {
            print!("{}", include_str!("../docs/AGENTS.md"));
            Ok(0)
        }

        Cmd::Catalog { offline } => {
            let loaded = catalog::load(
                &store,
                &catalog::Source::from_env(),
                offline,
                opts.timeout,
                &opts.user_agent,
            )?;
            if opts.verbose {
                eprintln!("catalog: {}", loaded.origin);
            }
            if cli.json {
                print_json(&loaded.entries);
            } else {
                print_catalog(&loaded.entries);
                eprintln!("\nadd one with: mcpdial add NAME --catalog ID");
            }
            Ok(0)
        }

        Cmd::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(0)
        }

        Cmd::Rm { name } => {
            if store.remove_server(&name)? {
                if cli.json {
                    println!("{}", json!({ "removed": name }));
                } else {
                    eprintln!("removed {name}");
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
                            json!({
                                "name": n, "kind": c.kind(), "location": c.location(),
                                "headers": c.headers, "token_env": c.token_env,
                                "credential": creds.get(n).is_some_and(Credential::has_token),
                                "source": c.source,
                            })
                        })
                        .collect();
                    print_json(&rows);
                } else {
                    // The column earns its place only once a server has a source.
                    let with_source = servers.values().any(|c| c.source.is_some());
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
                            row.push(c.location().into());
                            row
                        })
                        .collect();
                    if with_source {
                        print_table(&["NAME", "TYPE", "AUTH", "SOURCE", "LOCATION"], &rows);
                    } else {
                        print_table(&["NAME", "TYPE", "AUTH", "LOCATION"], &rows);
                    }
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
                print_json(&listing);
            } else if listing.is_empty() {
                eprintln!("{NO_SERVERS}");
            } else {
                let rows: Vec<Vec<String>> = listing.iter().map(listing_row).collect();
                print_table(&LISTING_HEADERS, &rows);
            }
            Ok(0)
        }

        Cmd::Tools { target: None, long } => {
            let probes = client::probe_all(&store, &opts, true)?;
            if cli.json {
                print_json(&probes);
                return Ok(0);
            }
            if probes.is_empty() {
                eprintln!("{NO_SERVERS}");
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
            let mut conn = dial(&store, &opts, &target)?;
            let tools = conn.session.list_tools()?;
            if cli.json {
                print_json(&json!({ "tools": tools }));
            } else {
                println!("{} tool(s):\n", tools.len());
                print_tools(&tools, long);
            }
            Ok(0)
        }

        Cmd::Info { target } => {
            let conn = dial(&store, &opts, &target)?;
            let init = &conn.server_info;
            if cli.json {
                print_json(init);
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
            let arguments = read_json_arg(&arguments, "arguments").map_err(|e| {
                Failure::hinted(
                    e,
                    json_arg_hint(
                        &arguments,
                        &format!("mcpdial call {} {tool}", shell_word(&target)),
                        format!(
                            "`mcpdial schema {} {tool}` shows what {tool} takes",
                            shell_word(&target)
                        ),
                    ),
                )
            })?;
            let mut conn = dial(&store, &opts, &target)?;
            let usage = |conn: &mut client::Connection| {
                let tools = conn.session.list_tools().unwrap_or_default();
                call_hint(
                    &tools,
                    &tool,
                    &format!("mcpdial call {}", shell_word(&target)),
                    "'",
                    &format!("`mcpdial tools {}`", shell_word(&target)),
                )
            };
            let result = match conn.session.call_tool(&tool, arguments) {
                Ok(result) => result,
                // The server rejected the arguments; say what it wanted instead.
                Err(e) => {
                    let hint = is_argument_error(&e).then(|| usage(&mut conn)).flatten();
                    return Err(Failure { error: e, hint });
                }
            };
            let text = mcpdial::render_content(&result);
            let is_error = print_tool_result(&result, &text, cli.json, false);
            // A failed result that is really a schema complaint gets the same
            // answer as the JSON-RPC error other servers would have sent.
            if is_error && reads_as_argument_error(&text) {
                if let Some(hint) = usage(&mut conn) {
                    print_hint(&hint, cli.json);
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
            print_json(&result);
            Ok(0)
        }

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
                .then(|| read_secret(client_secret_env.as_deref(), "client secret"))
                .transpose()?;
            let existing = store.credential(&r.name)?;
            let http = oauth::Http::new(opts.timeout, Some(opts.user_agent.clone()));
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
            let notify = |line: &str| eprintln!("{line}");
            let cred = match grant.as_str() {
                "client-credentials" => {
                    oauth::login_client_credentials(&http, &url, &login_opts, notify)?
                }
                _ => oauth::login(&http, &url, existing.as_ref(), &login_opts, notify)?,
            };
            store.save_credential(&r.name, cred.clone())?;
            let refreshable = cred.can_refresh();
            if cli.json {
                println!(
                    "{}",
                    json!({ "login": {
                        "name": r.name,
                        "expires_at": cred.expires_at,
                        "refreshable": refreshable,
                        "registration": cred.registration,
                    } })
                );
            } else {
                eprintln!(
                    "saved token for {} ({}{})",
                    r.name,
                    expiry_label(&cred),
                    if refreshable { ", refreshable" } else { "" }
                );
            }
            Ok(0)
        }

        Cmd::Logout { target } | Cmd::Token(TokenCmd::Rm { name: target }) => {
            let name = credential_key(&store, target);
            let removed = store.remove_credential(&name)?;
            if cli.json {
                println!(
                    "{}",
                    json!({ "removed_credential": removed.then_some(&name) })
                );
            } else if removed {
                eprintln!("removed credential for {name}");
            } else {
                eprintln!("no credential saved for {name}");
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Set { name, env }) => {
            let key = credential_key(&store, name);
            let token = read_secret(env.as_deref(), "token")?;
            let mut cred = store.credential(&key)?.unwrap_or_default();
            cred.access_token = Some(token);
            cred.expires_at = None;
            cred.source = Some("manual".into());
            store.save_credential(&key, cred)?;
            if cli.json {
                println!("{}", json!({ "saved_credential": key }));
            } else {
                eprintln!("saved token for {key}");
            }
            Ok(0)
        }

        Cmd::Token(TokenCmd::Show { name }) => {
            let key = credential_key(&store, name);
            let Some(cred) = store.credential(&key)? else {
                return Err(Error::config(format!("no credential saved for {key}")).into());
            };
            if cli.json {
                // Metadata only. The secrets never leave the file through this path.
                print_json(&json!({
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
                    "token_endpoint": cred.token_endpoint,
                }));
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
                if let Some(r) = cred
                    .registration
                    .as_deref()
                    .and_then(oauth::Registration::parse)
                {
                    println!("  registered:    {}", r.describe());
                }
                if cred.client_secret.is_some() {
                    println!(
                        "  client secret: present ({})",
                        cred.token_endpoint_auth_method
                            .as_deref()
                            .unwrap_or(oauth::CLIENT_SECRET_POST)
                    );
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

const LISTING_HEADERS: [&str; 7] = ["NAME", "TYPE", "STATUS", "AGE", "AUTH", "SERVER", "TOOLS"];

/// One server as `ls` shows it, which is also what `add` shows after dialing.
fn listing_row(l: &Listing) -> Vec<String> {
    vec![
        l.name.clone(),
        l.kind.into(),
        l.status.label(),
        age_label(l.age_seconds),
        auth_label(l),
        l.server
            .clone()
            .or_else(|| l.status.detail().map(truncate))
            .unwrap_or_else(|| "-".into()),
        l.tools.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
    ]
}

fn auth_label(l: &Listing) -> String {
    match (l.auth, &l.status) {
        (client::AuthUsed::Env, _) => "env".into(),
        (client::AuthUsed::Saved, _) => "saved".into(),
        (client::AuthUsed::None, Status::AuthRequired) => "needed".into(),
        (client::AuthUsed::None, Status::TokenRejected) => "rejected".into(),
        (client::AuthUsed::None, _) => "-".into(),
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

fn print_tools(tools: &[Value], long: bool) {
    print_named(tools, long, "parameters", describe_params);
}

fn print_prompts(prompts: &[Value], long: bool) {
    print_named(prompts, long, "arguments", describe_prompt_args);
}

/// Tools and prompts list identically; only the word for what they take differs.
fn print_named(items: &[Value], long: bool, takes: &str, describe: impl Fn(&Value) -> Vec<String>) {
    for item in items {
        let name = item["name"].as_str().unwrap_or("?");
        let desc = item["description"].as_str().unwrap_or("").trim();
        if long {
            println!("{name}");
            for line in desc.lines() {
                println!("    {}", line.trim_end());
            }
            let params = describe(item);
            if !params.is_empty() {
                println!("  {takes}:");
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

/// Resources and templates print the same way; only the key holding the URI differs.
fn print_resources(resources: &[Value], long: bool) {
    for r in resources {
        let uri = r["uri"]
            .as_str()
            .or_else(|| r["uriTemplate"].as_str())
            .unwrap_or("?");
        let desc = r["description"].as_str().unwrap_or("").trim();
        if long {
            println!("{uri}");
            for line in desc.lines() {
                println!("    {}", line.trim_end());
            }
            if let Some(mime) = r["mimeType"].as_str() {
                println!("    type: {mime}");
            }
            println!();
        } else {
            let summary = match desc.is_empty() {
                true => r["name"].as_str().unwrap_or(""),
                false => desc.lines().next().unwrap_or(""),
            };
            println!("  {uri:<44} {}", truncate_at(summary, 74));
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::completion::Completer;
    use rustyline::history::DefaultHistory;

    #[test]
    fn visible_escapes_what_would_move_the_cursor_and_keeps_layout() {
        assert_eq!(
            visible(
                "Error: k is invalid. Valid keys are: Enter,\r,\n,ShiftLeft,\0,\x1b[0m,\u{9b}\tend"
            ),
            "Error: k is invalid. Valid keys are: Enter,\\r,\n,ShiftLeft,\\0,\\x1b[0m,\\x9b\tend"
        );
        assert_eq!(
            visible("plain text\nsecond line"),
            "plain text\nsecond line"
        );
    }

    fn complete(helper: &ShellHelper, line: &str) -> (usize, Vec<String>) {
        let history = DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        helper.complete(line, line.len(), &ctx).unwrap()
    }

    #[test]
    fn completes_commands_then_tool_and_prompt_names_and_resource_uris() {
        let helper = ShellHelper {
            tools: ["list_pages", "list_console_messages", "new_page"]
                .map(String::from)
                .to_vec(),
            resources: ["file:///a.md", "file:///b.png"].map(String::from).to_vec(),
            prompts: ["summarize", "translate"].map(String::from).to_vec(),
        };
        // The first word is a command.
        let (at, found) = complete(&helper, "sch");
        assert_eq!((at, found), (0, vec!["schema".to_string()]));
        // The argument to these three is a tool, and completion starts at it.
        let (at, found) = complete(&helper, "call list_");
        assert_eq!(at, 5);
        assert_eq!(found, ["list_pages", "list_console_messages"]);
        assert_eq!(complete(&helper, "schema new").1, ["new_page"]);
        assert_eq!(complete(&helper, "help li").1.len(), 2);
        // Each of the other two pools answers to its own command.
        assert_eq!(complete(&helper, "read file:///b").1, ["file:///b.png"]);
        assert_eq!(complete(&helper, "prompt sum").1, ["summarize"]);
        assert!(complete(&helper, "read sum").1.is_empty());
        // Nothing to say about a tool's arguments, or about other commands.
        assert!(complete(&helper, "call new_page {\"ur").1.is_empty());
        assert!(complete(&helper, "raw tools/").1.is_empty());
        // A server that lists no tools simply offers nothing.
        assert!(complete(&ShellHelper::default(), "call li").1.is_empty());
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
    fn argument_errors_are_recognised_in_both_shapes() {
        assert!(is_argument_error(&Error::Rpc {
            code: -32602,
            message: "bad".into(),
            data: None
        }));
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
