# mcpdial

Dial any [MCP](https://modelcontextprotocol.io) server from the shell. No SDK, no host
app, no connector.

```
$ mcpdial ls
NAME  TYPE   STATUS         AUTH   SERVER                          TOOLS
ctx7  http   connected      -      Context7 4.0.5                  2
fs    stdio  connected      -      secure-filesystem-server 0.2.0  14
wiki  http   connected      -      DeepWiki 2.14.3                 3
work  http   auth required  needed -                               -

$ mcpdial login work          # browser opens once; the token is saved and refreshed
$ mcpdial call work list_projects
```

An MCP server is JSON-RPC 2.0 over one of two transports: HTTP POST to one URL, or
newline-delimited JSON on a subprocess's stdin/stdout. The "connector" in a host
application is a convenience, not part of the protocol. Anything that can POST a body or
spawn a process is a complete client. `mcpdial` is that, plus the bookkeeping that makes
it reusable: a list of named servers, saved OAuth tokens, live connection status, and a
listing of every tool on every server.

## Install

```bash
cargo install --git https://github.com/ObieMunoz/mcpdial
```

Or from a checkout: `cargo install --path .`. One static binary, no runtime dependencies.

## Targets

Every command that talks to a server takes a `TARGET`, which is one of:

| Form | Meaning |
|---|---|
| `wiki` | A name you saved with `mcpdial add` |
| `https://host/mcp` | An ad-hoc Streamable HTTP endpoint |
| `stdio:npx -y some-server /tmp` | An ad-hoc local process speaking MCP on stdio |

Saved names are the normal case. The other two exist so a one-off never needs setup.

## Commands

```
mcpdial add NAME --http URL [-H 'Name: value']... [--token-env VAR]
mcpdial add NAME --stdio "command args..." [--env KEY=VALUE]... [--cwd DIR]
mcpdial import [FILE] [--force]  pull servers from Claude Code, Claude Desktop, Cursor configs
mcpdial rm NAME

mcpdial ls [--no-probe]          every saved server, with live status and tool count
mcpdial tools [TARGET] [--long]  tools on one server, or on every server
mcpdial info TARGET              server name, version, capabilities, instructions
mcpdial call TARGET TOOL ['{"json":"args"}' | @file.json | -]
mcpdial schema TARGET TOOL       one tool's input schema
mcpdial raw TARGET METHOD ['{"json":"params"}' | @file.json | -]
mcpdial shell TARGET             one session, many commands; state persists between calls

mcpdial login TARGET [--scope S] [--port N] [--client-id ID] [--redirect-host H] [--no-browser]
mcpdial logout TARGET
mcpdial token set NAME [--env VAR]   token from stdin or an env var, never an argument
mcpdial token show NAME              metadata only; the secret is never printed
mcpdial token rm NAME
mcpdial guide                    the usage guide for programs and agents
```

Global flags: `--json` for machine output, `-v` to trace every message on stderr,
`--timeout SECS`, `-H` for extra headers, `--token-env VAR` to force a token from the
environment, `--user-agent` to override the default browser UA.

### Seeing what a server offers

`mcpdial tools` with no target probes every saved server in parallel and prints each
one's tools. `--long` adds full descriptions and a parameter list derived from the tool's
JSON schema:

```
$ mcpdial tools wiki --long
read_wiki_structure
    Get a list of documentation topics for a GitHub repository.
  parameters:
    repoName: string (required) - GitHub repository: owner/repo (e.g. "facebook/react")
```

### Stateful servers and the shell

Every `call` starts a fresh session, and for a stdio server that means a fresh process.
A browser automation server such as `chrome-devtools-mcp` launches a new Chrome each
time, so a page opened by one call is gone by the next. `mcpdial shell` keeps one
session open and reads commands from stdin, one per line:

```
$ mcpdial shell chrome
chrome> call list_pages
## Pages
1: about:blank [selected]
chrome> call navigate_page {"pageId":1,"url":"https://example.com"}
Successfully navigated to https://example.com.
chrome> call list_pages
## Pages
1: Example Domain (https://example.com/) [selected]
chrome> quit
```

It reads a script from a pipe just as well. Commands are `call`, `tools`, `schema`,
`raw`, `info`, `help`, and `quit`; a `#` starts a comment. With `--json` each result is
one line of JSON. In a script, any failed command makes the exit code 1 after the script
finishes.

Arguments are one JSON object. When a line does not work, the answer says what the tool
actually takes rather than leaving you to go read the schema:

```
chrome> list_pages
error: list_pages is a tool, not a command
usage: call list_pages {}
chrome> call new_page
error: MCP error -32602: Invalid arguments for tool new_page: Required at url
usage: call new_page {"url": "<string>"}
  url: string (required) - URL to load in the new page
  timeout: number - Maximum wait time in milliseconds
```

A tool name with a typo gets the nearest real one. `schema TOOL` prints a tool's full
input schema, `help TOOL` just its parameters, and `mcpdial call` outside the shell
answers a rejected call the same way, with a line you can paste back into the terminal.
Under `--json` the same text arrives as `error.hint`. Some servers report a schema
violation as a failed result rather than a JSON-RPC error; both get the same answer.

### Importing from a host you already configured

`mcpdial import` reads the `mcpServers` shape that Claude Code, Claude Desktop, Cursor,
and Windsurf all use, including the per-project entries nested in `~/.claude.json`, and
saves each server under its existing name. With no file argument it scans the usual
locations. `command` plus `args` become one stdio command line, `env` and `cwd` are
kept, and `url` plus `headers` become an HTTP server. Nothing else in those files is read.

### When a stdio server dies on startup

A stdio server's stderr is captured, and if the process exits before answering, the
error shows its exit status and its last stderr lines. A typo in an npm package name
looks like this instead of a bare "closed stdout":

```
error: server exited with status 1 before replying. Its last stderr lines:
  | npm error code E404
  | npm error 404 Not Found - GET https://registry.npmjs.org/chrome-dev-tools-mcp - Not found
```

### Status

`mcpdial ls` connects to every server and classifies the outcome:

| Status | Meaning |
|---|---|
| `connected` | `initialize` succeeded; the server column shows its name and version |
| `auth required` | The server challenged for a credential and none was saved |
| `token rejected` | A credential was sent and the server refused it |
| `blocked (403)` | 403 with **no** `WWW-Authenticate`: WAF, allowlist, or geo block. A token will not help |
| `unreachable` | Socket, DNS, or process failure; detail in the server column |

That third-versus-fourth distinction is deliberate. A 403 with no challenge header means
nobody asked for a token, and telling you to check your scopes would waste your time.

## Authentication

The only hard part of talking to a remote MCP server is the credential. Three ways in:

**`mcpdial login NAME`** runs OAuth 2.1 the way the MCP spec describes it: discover the
authorization server from the resource metadata, register a public client dynamically,
open the browser for the authorization-code grant with PKCE, catch the redirect on a
loopback port, exchange the code, and save the result. The browser step happens once.
Afterwards the access token is refreshed automatically when it expires, and once more on
an unexpected 401, so a saved server keeps working indefinitely.

The redirect URI is `http://127.0.0.1:PORT/callback`, and if the server refuses that,
`http://localhost:PORT/callback` is tried next, because some servers allowlist only the
name. If both are refused, the server is not following RFC 8252 and the error says what
to change; for a Doorkeeper server that is one line in `config/initializers/doorkeeper.rb`.
Use `--redirect-host` to skip the guessing.

Servers must be addressed at their final URL. A redirect (say, `www.` to the bare host)
would turn the POST into a GET, so `mcpdial` refuses to follow it and names the URL to
use instead.

**`mcpdial token set NAME`** saves a token you obtained some other way, read from stdin or
from `--env VAR`. It is never accepted as a command-line argument, so it cannot land in
`ps` output or shell history.

**`--token-env VAR`**, on the command line or saved with `add`, reads the token from the
environment on every call and beats any saved credential.

## Where things live

```
~/.config/mcpdial/servers.json       what you configured (safe to share)
~/.config/mcpdial/credentials.json   tokens, refresh tokens, client ids (mode 0600)
```

Override the directory with `MCPDIAL_HOME`, or `XDG_CONFIG_HOME`. Removing a server with
`rm` also removes its credential.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | The server said no: HTTP error, JSON-RPC error, or a tool result with `isError` |
| 2 | Usage or config problem; nothing was sent |

Errors are one line on stderr, so it composes:

```
$ mcpdial call wiki nope; echo "exit=$?"
error: MCP error -32602: Tool nope not found
exit=1
```

## Calling it from a program or an agent

Pass `--json` on every command. Results go to stdout; errors go to stderr as a single
object with a `kind` to branch on (`rpc`, `http`, `transport`, `auth`, `config`,
`usage`) and the status or code behind it. `schema TARGET TOOL` returns one tool's
input schema. Arguments can come from a file (`@args.json`) or stdin (`-`), so quoting
is never a problem. `shell --json` gives one JSON line per command, errors included, in
order.

The full reference for programs is [docs/AGENTS.md](docs/AGENTS.md), and it is embedded
in the binary: `mcpdial guide` prints it, so an agent can load it into context without
finding the file. A Claude Code skill that teaches the same thing lives in
[skills/mcpdial/SKILL.md](skills/mcpdial/SKILL.md); copy that directory to
`~/.claude/skills/mcpdial/` to enable it.

## As a library

The binary is a thin layer over a small library with no async runtime:

```rust
use mcpdial::{HttpTransport, Session};

let mut s = Session::new(HttpTransport::new("https://mcp.deepwiki.com/mcp"));
s.initialize()?;
let result = s.call_tool("read_wiki_structure", serde_json::json!({"repoName": "x/y"}))?;
println!("{}", mcpdial::render_content(&result));
# Ok::<(), mcpdial::Error>(())
```

`Store`, `client::resolve`, `client::connect`, and `client::probe_all` expose the saved
servers, token selection, and status probing if you want them.

## How it works, in three facts

1. Every message is a JSON-RPC 2.0 object.
2. Transport is either HTTP POST to one endpoint, or newline-delimited JSON over stdio.
3. The methods you need are `initialize`, `tools/list`, and `tools/call`.

Two details make a naive `curl` attempt fail, and both are handled here: Streamable HTTP
servers may frame the reply as `text/event-stream` rather than JSON, and the
`notifications/initialized` notification after `initialize` is mandatory on stateful
servers. A third detail comes from the edge rather than the protocol: bot mitigation in
front of public servers rejects default library user agents, so every request carries a
real browser UA.

## Development

```bash
cargo test            # unit tests plus end-to-end tests against a fake MCP + OAuth server
cargo clippy --all-targets
cargo run --example echo_server   # the smallest real stdio MCP server, used by the tests
```

## License

MIT
