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

Every [release](https://github.com/ObieMunoz/mcpdial/releases) ships one static binary
per platform, with no runtime dependencies:

```bash
curl -fsSL https://github.com/ObieMunoz/mcpdial/releases/latest/download/mcpdial-aarch64-apple-darwin.tar.gz | tar xz
sudo install mcpdial-aarch64-apple-darwin/mcpdial /usr/local/bin/
```

| Platform | Archive |
|---|---|
| macOS, Apple silicon | `mcpdial-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `mcpdial-x86_64-apple-darwin.tar.gz` |
| Linux, x86-64 | `mcpdial-x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM64 | `mcpdial-aarch64-unknown-linux-musl.tar.gz` |
| Windows, x86-64 | `mcpdial-x86_64-pc-windows-msvc.zip` |

The Linux binaries are linked against musl, so they run on any distribution, and each
release carries a `SHA256SUMS` file to check a download against. A binary a *browser*
downloaded on macOS is quarantined until `xattr -d com.apple.quarantine mcpdial`; `curl`
does not set that flag. Or install it with
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall), or from source:

```bash
cargo binstall --git https://github.com/ObieMunoz/mcpdial mcpdial
cargo install --git https://github.com/ObieMunoz/mcpdial
```

`mcpdial completions SHELL` prints a completion script for bash, zsh, fish, elvish or
powershell; [docs/reference.md](docs/reference.md#shell-completions) says where each
shell wants it. What changed in each version is in [CHANGELOG.md](CHANGELOG.md).

## Getting started

```bash
mcpdial catalog                       # a reviewed list of servers to start from
mcpdial add docs --catalog context7   # save one under a name
mcpdial tools docs                    # what it offers
mcpdial call docs search query="rust ureq" limit=5
```

`mcpdial browse` is the same catalog as a checklist, and `mcpdial import` picks up the
servers you already configured in Claude, Cursor, Windsurf, VS Code, Codex or OpenCode.
A bare `mcpdial` at a terminal picks a server, then a tool, then asks for its arguments
and prints the one-line command that would have made the same call.

## Targets

Every command that talks to a server takes a `TARGET`, which is one of:

| Form | Meaning |
|---|---|
| `wiki` | A name you saved with `mcpdial add` |
| `https://host/mcp` | An ad-hoc Streamable HTTP endpoint |
| `stdio:npx -y some-server /tmp` | An ad-hoc local process speaking MCP on stdio |

Saved names are the normal case. The other two exist so a one-off never needs setup.

## Commands

`mcpdial --help` lists every command and every global flag, and `mcpdial CMD --help`
covers one. The ones worth knowing about first:

| | |
|---|---|
| `add` `rm` `set` `ls` | Save servers, and see their live status |
| `tools` `schema` `grep` `info` | What a server offers, across one server or all of them |
| `call` `prompt` `read` `raw` | Actually use it |
| `shell` `start` `stop` | Keep one session open across many calls |
| `login` `logout` `token` | Credentials |
| `import` `export` `catalog` `browse` `search` | Get servers in and out |
| `serve` | Lend a server to something that must not hold its token |

Global flags worth knowing: `--json` for machine output, `-v` to trace every message on
stderr, `--timeout SECS`, and `--max-chars N` / `-o FILE` to keep a large result out of a
context window.

## A short tour

**Arguments, without the quoting.** A tool's arguments are one JSON object, which a
shell fights over every quote of. They can be `key=value` pairs instead, typed from the
tool's own `inputSchema` before the call goes out:

```bash
mcpdial call fs read_text_file path=/tmp/x
mcpdial call ctx7 search query="rust ureq" limit=5 fuzzy=true  # 5 is a number, true a boolean
mcpdial call srv tool tags:='["a","b"]'                        # := is JSON, whatever the schema says
```

**Find one across every server.** Ten saved servers is ten `tools` listings to read
before the one that "creates a calendar event" turns up. `mcpdial grep` dials them all at
once and searches tools, resources, prompts and instructions. A server that is down is
skipped and named, not fatal; exit 1 means nothing matched, so it composes.

**Stateful servers.** Every `call` starts a fresh session, and for a stdio server that
means a fresh process — a browser server launches a new Chrome each time. `mcpdial shell`
holds one session open and reads commands from stdin, with history, Tab completion of
tool and parameter names, `| .path` and `| jq` filters, and `show`/`save`/`retry`/`edit`
for results it already printed. `mcpdial start NAME` does the same for an agent that
issues one command at a time: the server runs in the background and every later command
shares its session.

**Allowing and denying tools.** A filesystem server with fourteen tools is usually wanted
for three of them, and an agent that can see `delete_file` will eventually call it.

```bash
mcpdial add fs --stdio "npx -y @modelcontextprotocol/server-filesystem /srv" \
    --allow 'read_*' --deny 'delete_*'
```

Deny wins over allow, an empty allow list means every tool not denied, and the
restriction travels with the name into every command. Calling a hidden tool is exit 2
with nothing sent.

**Lend a server without lending its token.** `mcpdial serve NAME` puts a saved server on
loopback as a plain MCP endpoint, dialing upstream with the credential you logged in with
and handing the client the protocol and nothing else. So an agent in a sandbox can use a
server it has no credential for, and still has none if it is compromised.

**Pin what a server promised.** A script written against `read_text_file(path)` breaks
silently when the server renames the parameter. Write down what the server offers today,
commit it beside the script, and check it before the work starts:

```bash
mcpdial tools files --snapshot tools.json
mcpdial tools files --check tools.json    # exit 3 on drift, and nothing else
```

**Long calls.** Servers report as they go: at a terminal that is one updating line on
stderr, and `--progress` asks for it explicitly anywhere else. Where the server supports
it, `--task` runs the call in the background and polls to the end, and `--detach` prints
a task id to collect later with `mcpdial tasks`.

The detail behind all of this — terminal output, the catalog and registry, import and
export, protocol revisions, elicitation — is in [docs/reference.md](docs/reference.md).

## Authentication

**`mcpdial login NAME`** runs OAuth 2.1 the way the MCP spec describes it: discover the
authorization server, identify the client, open the browser for the authorization-code
grant with PKCE, catch the redirect on a loopback port, and save the result. The browser
step happens once; the token is refreshed automatically afterwards, so a saved server
keeps working indefinitely. `--grant client-credentials` is the headless form, for a cron
job or a CI step that owns a confidential client.

**`mcpdial token set NAME`** saves a token you obtained some other way, read from stdin
or `--env VAR`. It is never accepted as an argument, so it cannot land in `ps` output or
shell history. **`--token-env VAR`** reads one from the environment on every call
instead, and **`${VAR}`** does the same inside any header, URL or stdio environment:

```bash
mcpdial add gh --http https://api.githubcopilot.com/mcp/ -H 'Authorization: Bearer ${GITHUB_TOKEN}'
```

The single quotes matter — they keep the shell from expanding the placeholder before
mcpdial sees it. It is saved as written and read fresh each time the server is dialed, so
`servers.json` names the secret without holding it.

Client identification, issuer validation, redirect URIs and the OS keychain are covered
in [docs/reference.md](docs/reference.md#authentication-in-detail).

## Where things live

```
~/.config/mcpdial/servers.json       what you configured (safe to share)
~/.config/mcpdial/credentials.json   tokens, refresh tokens, client ids (owner only)
~/.config/mcpdial/config.json        how mcpdial behaves: where credentials are kept
~/.config/mcpdial/catalog.json       the catalog as last refreshed (a cache)
~/.config/mcpdial/registry/          a copy of the registry's list, for `search`
~/.config/mcpdial/probes.json        what `ls` last saw, and each server's protocol era
~/.config/mcpdial/tasks.json         background task ids started here (a note, not the truth)
~/.config/mcpdial/run/NAME.sock      where a server kept running by `start` listens
```

"Owner only" is mode 0600 on unix, and an access list naming the account that ran `login`
on Windows. Removing a server with `rm` also removes its credential. Tokens can live in
the OS keychain instead; see the reference.

`MCPDIAL_HOME` moves that directory, and most global flags have an environment variable
beside them (`MCPDIAL_JSON`, `MCPDIAL_TIMEOUT`, `MCPDIAL_PLAIN`, …). A flag beats its
variable, which beats the default; the full table is in
[docs/reference.md](docs/reference.md#environment-variables).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | The server said no: HTTP error, JSON-RPC error, or a tool result with `isError` |
| 2 | Usage or config problem; nothing was sent |
| 3 | `--check`: the server's tools drifted from the snapshot; nothing was sent |

Errors are one line on stderr, so it composes:

```
$ mcpdial call wiki nope; echo "exit=$?"
error: MCP error -32602: Tool nope not found
exit=1
```

## Calling it from a program or an agent

Pass `--json` on every command. Results go to stdout; errors go to stderr as a single
object with a `kind` to branch on (`rpc`, `http`, `transport`, `auth`, `config`,
`usage`). Arguments can come from a file (`@args.json`), stdin (`-`), or `key=value`
pairs, so quoting is never a problem, and `--trace FILE` records every message that went
over the wire, with credentials redacted.

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
servers. A third comes from the edge rather than the protocol: bot mitigation in front of
public servers rejects default library user agents, so every request carries a real
browser UA.

## Development

```bash
cargo test            # unit tests plus end-to-end tests against a fake MCP + OAuth server
cargo clippy --all-targets
cargo run --example echo_server   # the smallest real stdio MCP server, used by the tests
cargo build --no-default-features # the agent-only binary: no terminal presentation at all
```

`tests/contract.rs` runs every command under a pipe and under `--json` and compares exit
code, stdout and stderr with the files under `tests/snapshots/`. A change there fails the
test, on purpose: what a program or an agent reads from mcpdial is the contract. See
[docs/development.md](docs/development.md) for regenerating those snapshots and for
cutting a release.

## License

MIT
