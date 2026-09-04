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
mcpdial add NAME --stdio "command args..."
mcpdial rm NAME

mcpdial ls [--no-probe]          every saved server, with live status and tool count
mcpdial tools [TARGET] [--long]  tools on one server, or on every server
mcpdial info TARGET              server name, version, capabilities, instructions
mcpdial call TARGET TOOL ['{"json":"args"}']
mcpdial raw TARGET METHOD ['{"json":"params"}']

mcpdial login TARGET [--scope S] [--port N] [--client-id ID] [--no-browser]
mcpdial logout TARGET
mcpdial token set NAME [--env VAR]   token from stdin or an env var, never an argument
mcpdial token show NAME              metadata only; the secret is never printed
mcpdial token rm NAME
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
