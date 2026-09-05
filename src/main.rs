use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use mcpdial::client::{self, describe_params, Listing, Options, Status};
use mcpdial::protocol::METHOD_NOT_FOUND;
use mcpdial::session::{render_messages, resource_bodies, ResourceBody};
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
        /// Space-separated scopes (default: whatever the server advertises)
        #[arg(long)]
        scope: Option<String>,
        /// Fixed loopback port for the redirect (default: any free port)
        #[arg(long)]
        port: Option<u16>,
        /// Use a pre-registered client id instead of dynamic registration
        #[arg(long)]
        client_id: Option<String>,
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
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(f) => {
            if json {
                eprintln!("{}", f.to_json());
            } else {
                eprintln!("error: {}", f.error);
                if let Some(hint) = &f.hint {
                    eprintln!("{hint}");
                }
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
        Err(e) => Err(Error::usage(format!(
            "{what} must be a JSON object like {{\"key\": \"value\"}}; {:?} is not JSON ({e})",
            truncate_at(text, 60)
        ))),
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
                    return Err(
                        Error::usage("pass exactly one of --http URL or --stdio CMD").into(),
                    )
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
                )
                .into());
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
                    .map(String::from)
                    .unwrap_or_else(|| truncate_at(&r.name, 24))
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
                                println!(
                                    "{}",
                                    if cli.json {
                                        t.to_string()
                                    } else {
                                        serde_json::to_string_pretty(t).unwrap()
                                    }
                                );
                                Ok(())
                            }
                            None => Err(no_such_tool(tools, rest)),
                        }
                    }
                    "info" => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&conn.server_info).unwrap()
                        );
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
                        let (name, args) =
                            rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                        let args = match args.trim() {
                            "" => "{}",
                            a => a,
                        };
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
                        let (tool, args) =
                            rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                        let args = match args.trim() {
                            "" => "{}",
                            a => a,
                        };
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
                                        let failed = result["isError"].as_bool().unwrap_or(false);
                                        if cli.json {
                                            println!("{result}");
                                        } else {
                                            if !out.is_empty() {
                                                print_text(&out);
                                            }
                                            if failed {
                                                eprintln!("(tool reported an error)");
                                            }
                                        }
                                        if failed && reads_as_argument_error(&out) {
                                            if let Some(hint) =
                                                shell_call_hint(&mut cache, &mut conn, tool)
                                            {
                                                eprintln!("{hint}");
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
                        let (method, params) =
                            rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                        if method.is_empty() {
                            Err(Failure::hinted(
                                Error::usage("raw needs a method"),
                                "usage: raw METHOD {\"json\": \"params\"}   e.g. raw tools/list",
                            ))
                        } else {
                            parse_object(params.trim(), "params")
                                .and_then(|p| conn.session.request(method, Some(p)))
                                .map_err(Failure::from)
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
                        eprintln!("error: {}", f.error);
                        if let Some(hint) = &f.hint {
                            eprintln!("{hint}");
                        }
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
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
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
            println!("{}", serde_json::to_string_pretty(t).unwrap());
            if t.get("outputSchema").is_some() {
                eprintln!("this tool declares an outputSchema: results carry structuredContent");
            }
            Ok(0)
        }

        Cmd::Resources { target, long } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let info_cmd = format!("`mcpdial info {}`", shell_word(&target));
            let resources = conn
                .session
                .list_resources()
                .map_err(|e| missing_capability(e, "resources", &info_cmd))?;
            // Templates are optional even where resources are not, so a server with
            // none of them must not turn the whole listing into an error.
            let templates = conn.session.list_resource_templates().unwrap_or_default();
            if cli.json {
                let both = json!({ "resources": resources, "resourceTemplates": templates });
                println!("{}", serde_json::to_string_pretty(&both).unwrap());
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
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let info_cmd = format!("`mcpdial info {}`", shell_word(&target));
            let result = conn
                .session
                .read_resource(&uri)
                .map_err(|e| missing_capability(e, "resources", &info_cmd))?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                return Ok(0);
            }
            let redirect = format!("mcpdial read {} {}", shell_word(&target), shell_word(&uri));
            write_resource(&resource_bodies(&result)?, &redirect)?;
            Ok(0)
        }

        Cmd::Prompts { target, long } => {
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let info_cmd = format!("`mcpdial info {}`", shell_word(&target));
            let prompts = conn
                .session
                .list_prompts()
                .map_err(|e| missing_capability(e, "prompts", &info_cmd))?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "prompts": prompts })).unwrap()
                );
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
                    format!(
                        "`mcpdial prompts {} --long` shows what {name} takes",
                        shell_word(&target)
                    ),
                )
            })?;
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let info_cmd = format!("`mcpdial info {}`", shell_word(&target));
            let result = conn
                .session
                .get_prompt(&name, arguments)
                .map_err(|e| missing_capability(e, "prompts", &info_cmd))?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
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

        Cmd::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(0)
        }

        Cmd::Rm { name } => {
            if store.remove_server(&name)? {
                eprintln!("removed {name}");
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
            let freshness = if refresh {
                client::Freshness::Live
            } else {
                client::Freshness::Remembered
            };
            let listing = client::listing(&store, &opts, freshness)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&listing).unwrap());
            } else if listing.is_empty() {
                eprintln!("no servers saved yet. Try: mcpdial add wiki --http https://mcp.deepwiki.com/mcp");
            } else {
                let rows: Vec<Vec<String>> = listing
                    .iter()
                    .map(|l| {
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
                    })
                    .collect();
                print_table(
                    &["NAME", "TYPE", "STATUS", "AGE", "AUTH", "SERVER", "TOOLS"],
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
            let arguments = read_json_arg(&arguments, "arguments").map_err(|e| {
                Failure::hinted(
                    e,
                    format!(
                        "`mcpdial schema {} {tool}` shows what {tool} takes",
                        shell_word(&target)
                    ),
                )
            })?;
            let r = client::resolve(&store, &target)?;
            let mut conn = client::connect(&store, &r, &opts)?;
            let result = match conn.session.call_tool(&tool, arguments) {
                Ok(result) => result,
                // The server rejected the arguments; say what it wanted instead.
                Err(e) => {
                    let hint = is_argument_error(&e)
                        .then(|| conn.session.list_tools().unwrap_or_default())
                        .and_then(|tools| {
                            call_hint(
                                &tools,
                                &tool,
                                &format!("mcpdial call {}", shell_word(&target)),
                                "'",
                                &format!("`mcpdial tools {}`", shell_word(&target)),
                            )
                        });
                    return Err(Failure { error: e, hint });
                }
            };
            let is_error = result["isError"].as_bool().unwrap_or(false);
            let text = mcpdial::render_content(&result);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
            } else if !text.is_empty() {
                print_text(&text);
            }
            // A failed result that is really a schema complaint gets the same
            // answer as the JSON-RPC error other servers would have sent.
            if is_error && reads_as_argument_error(&text) {
                let tools = conn.session.list_tools().unwrap_or_default();
                if let Some(hint) = call_hint(
                    &tools,
                    &tool,
                    &format!("mcpdial call {}", shell_word(&target)),
                    "'",
                    &format!("`mcpdial tools {}`", shell_word(&target)),
                ) {
                    eprintln!("{hint}");
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
            client_secret,
            client_secret_env,
            redirect_host,
            no_browser,
        } => {
            let r = client::resolve(&store, &target)?;
            let Some(url) = r.config.http.clone() else {
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
            let login_opts = oauth::LoginOptions {
                scope,
                port,
                client_id,
                client_secret,
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
            let token = read_secret(env.as_deref(), "token")?;
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
                        "has_client_secret": cred.client_secret.is_some(),
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
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() > 60 {
        format!("{}...", first.chars().take(57).collect::<String>())
    } else {
        first.to_string()
    }
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
