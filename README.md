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
per platform, with no runtime dependencies. Download the archive for yours and put
`mcpdial` on your `PATH`:

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

The Linux binaries are linked against musl, so they run on any distribution. Each
release also carries a `SHA256SUMS` file; `sha256sum -c --ignore-missing SHA256SUMS`
(`shasum -a 256 -c --ignore-missing` on macOS) checks a download against it. A binary
that a browser downloaded on macOS is quarantined until
`xattr -d com.apple.quarantine mcpdial`; `curl` does not set that flag.

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) the same archive is
fetched and installed in one step:

```bash
cargo binstall --git https://github.com/ObieMunoz/mcpdial mcpdial
```

Or build from source with a Rust toolchain:

```bash
cargo install --git https://github.com/ObieMunoz/mcpdial
```

Or from a checkout: `cargo install --path .`. What changed in each version is in
[CHANGELOG.md](CHANGELOG.md).

### Shell completions

`mcpdial completions SHELL` prints a completion script for `bash`, `zsh`, `fish`,
`elvish` or `powershell`. It is generated from the argument parser itself, so it covers
every subcommand, every flag and every fixed value without drifting from the binary.
The command is hidden from `mcpdial --help` so it does not crowd the command list, but
it completes like any other and `mcpdial completions --help` describes it.

**bash**, with [bash-completion](https://github.com/scop/bash-completion) installed:

```bash
mkdir -p ~/.local/share/bash-completion/completions
mcpdial completions bash > ~/.local/share/bash-completion/completions/mcpdial
```

Without it, source the script from `~/.bashrc` instead:

```bash
echo 'source <(mcpdial completions bash)' >> ~/.bashrc
```

**zsh**, in a file called `_mcpdial` anywhere on `$fpath`:

```bash
mkdir -p ~/.zfunc
mcpdial completions zsh > ~/.zfunc/_mcpdial
```

`~/.zshrc` then needs `fpath=(~/.zfunc $fpath)` *before* it calls `compinit`. On
Homebrew, `$(brew --prefix)/share/zsh/site-functions/` is already on `$fpath`, so
writing there takes no `.zshrc` change. Either way, `rm -f ~/.zcompdump*` and start a
new shell if the old completions linger.

**fish**:

```bash
mcpdial completions fish > ~/.config/fish/completions/mcpdial.fish
```

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
mcpdial add NAME --http URL [-H 'Name: value']... [--token-env VAR] [--protocol-version V] [--timeout SECS] [--force] [--no-probe]
mcpdial add NAME --stdio "command args..." [--env KEY=VALUE]... [--cwd DIR] [--protocol-version V] [--timeout SECS] [--force] [--no-probe]
mcpdial add NAME --catalog ID    one entry of the reviewed catalog; `mcpdial catalog` lists them
mcpdial add NAME --registry io.github.owner/server [--package npm|pypi|oci] [--remote] [--arg VALUE]...
mcpdial search QUERY [--limit N] [--refresh] [--offline]   the MCP registry, ranked, from a local copy
mcpdial import [FILE] [--from HOST] [--force]  pull servers from Claude, Cursor, Windsurf, VS Code, Codex, OpenCode configs
mcpdial rm NAME
mcpdial catalog [--offline]      the reviewed list of servers, grouped by category

mcpdial ls [--no-probe]          every saved server, with live status and tool count
mcpdial tools [TARGET] [--long]  tools on one server, or on every server
mcpdial info TARGET              server name, version, capabilities, instructions
mcpdial call TARGET TOOL ['{"json":"args"}' | @file.json | -]
mcpdial schema TARGET TOOL       one tool's input schema
mcpdial resources TARGET [--long]  every resource, then every URI template
mcpdial read TARGET URI          one resource: text to stdout, bytes to a redirect or --save-dir
mcpdial --save-dir DIR ...       file image, audio and blob blocks as DIR/<tool>-<n>.<ext>
mcpdial prompts TARGET [--long]  every prompt a server offers
mcpdial prompt TARGET NAME ['{"json":"args"}' | @file.json | -]
mcpdial raw TARGET METHOD ['{"json":"params"}' | @file.json | -]
mcpdial shell TARGET             one session, many commands; state persists between calls
mcpdial start NAME [--idle SECS] keep a stdio server running; later commands share its session
mcpdial stop NAME                end it

mcpdial login TARGET [--scope S] [--port N] [--client-id ID] [--client-metadata-url URL]
                     [--no-client-metadata] [--redirect-host H] [--no-browser]
mcpdial login TARGET --grant client-credentials --client-id ID (--client-secret | --client-secret-env VAR)
mcpdial logout TARGET
mcpdial token set NAME [--env VAR]   token from stdin or an env var, never an argument
mcpdial token show NAME              metadata only; the secret is never printed
mcpdial token rm NAME
mcpdial guide                    the usage guide for programs and agents
mcpdial completions SHELL        a completion script; see Install above
```

Global flags: `--json` for machine output, `-v` to trace every message on stderr,
`--timeout SECS`, `-H` for extra headers, `--token-env VAR` to force a token from the
environment, `--user-agent` to override the default browser UA, `--protocol-version
VERSION` to offer an older MCP revision at `initialize`, `--no-daemon` to dial a
server afresh even while `start` has one running, and `--no-retry` to fail on the
first transient HTTP failure instead of sending the request once more.

`--timeout` bounds every wait: the flag on the command line, else the timeout saved
with the server, else 60 seconds. `add --timeout SECS` saves one for a server that
installs packages on first launch or runs tools for minutes, `ls --no-probe` shows it,
and `import` keeps a numeric `timeout` (seconds) it finds in a host's config.

`initialize` offers protocol `2025-11-25` and runs on whichever version the server
answers with, out of `2025-11-25`, `2025-06-18` and `2025-03-26`; `mcpdial info` shows
the one agreed. A server that answers with a version mcpdial does not speak is reported
as such. For a server that misbehaves when offered the newest, `--protocol-version
2025-06-18` offers that instead, and `add --protocol-version` saves the choice.

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
`resources`, `read`, `prompts`, `prompt`, `raw`, `info`, `help`, and `quit`; a `#` starts
a comment. With `--json` each result is one line of JSON. In a script, any failed command
makes the exit code 1 after the script finishes.

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

At a terminal the prompt is a real line editor: Up and Down walk the history, Tab
completes command names and tool names, and `^C` abandons the line being typed (twice
leaves). History is kept per saved server in `~/.config/mcpdial/history-NAME`. Piped
input is read plainly, exactly as before, so scripts are unaffected.

### Keeping a stdio server running between calls

A shell is one process holding one session. An agent that issues one command at a
time cannot hold it open, and a server started with `npx -y ...` pays its startup on
every `call`. `mcpdial start` runs the server in the background and keeps the session;
every command that names the server then goes through that process instead of dialing:

```
$ mcpdial start chrome
started chrome (pid 41234)
$ mcpdial call chrome new_page '{"url":"https://example.com"}'
$ mcpdial call chrome list_pages        # the same Chrome, the same page
$ mcpdial ls                            # DAEMON column says `running`
$ mcpdial stop chrome
stopped chrome
```

Nothing starts in the background unless you say `start`: a plain `call` is still a full
session of its own, so a script that never heard of `start` behaves exactly as before.
`start --idle 300` makes the daemon exit after five minutes with no caller, which is
the setting to use for a server you keep forgetting to stop; the default is to run
until `stop`. `--no-daemon` on any command, or `MCPDIAL_NO_DAEMON=1` in the
environment, dials a fresh process even while a daemon is running.

Requests are relayed one caller at a time: a second `call` while the first is still
waiting on the server queues behind it, and a `shell` attached to the daemon holds the
queue until it quits. A request the server makes mid-call (a `ping`, `roots/list`) is
handed to the calling process, which is the one at a terminal. The daemon's
`--timeout` bounds how long it waits on the server for any one request. If the daemon
was killed rather than stopped, the next command finds the socket dead, removes it with
a note on stderr, and dials as if it had never been there.

`start` applies to saved stdio servers and to Unix (macOS, Linux). An HTTP server has
no process to keep, and session reuse for it is a separate matter; on Windows `start`
and `stop` say they are not supported yet, and everything else dials.

### Importing from a host you already configured

`mcpdial import` reads the `mcpServers` shape that Claude Code, Claude Desktop, Cursor,
and Windsurf all use, including the per-project entries nested in `~/.claude.json`, and
saves each server under its existing name. It also reads VS Code's `mcp.json`, where
the object is called `servers`, Codex's `config.toml` with its `[mcp_servers.NAME]`
tables, and OpenCode's `opencode.json` with its `mcp` object. With no file argument it
scans the usual locations: `.mcp.json` and `.vscode/mcp.json` in the current
directory, then `~/.claude.json`, Cursor, Windsurf, `~/.codex/config.toml`,
`~/.config/opencode/opencode.json`, and the Claude Desktop and VS Code user files
where each platform keeps them. `--from vscode|codex|opencode|claude|cursor|windsurf`
scans one host's locations alone, for when two hosts use the same name for different
servers. `command` plus `args` become one stdio command line, `env` and `cwd` are kept,
and `url` plus `headers` become an HTTP server; Codex's `bearer_token_env_var` is saved
as `token_env`. Nothing else in those files is read.

VS Code's `${input:ID}` references, which the editor prompts for, are saved as
`${MCPDIAL_INPUT_ID}` placeholders (the id uppercased, anything but letters and digits
as `_`), and a note names each variable to set before dialing; nothing is prompted for
and no secret is written. A server's `envFile` is opened only to learn its keys: each
becomes a `${KEY}` placeholder in the saved `env`, the note lists them, and the values
stay in the file. Under `--json` the notes ride in the receipt under `notes`, keyed by
server name.

Codex's `config.toml` is read by a small reader of its own rather than a full TOML
parser: tables, dotted keys, strings, arrays, inline tables, booleans, numbers and
comments, which is everything a server entry uses. A multi-line string, an array of
tables or a date in the file is refused with its line number rather than misread.

### Adding from the catalog

`mcpdial catalog` prints a reviewed list of servers, grouped by category: source
control, browsers, docs and search, databases, productivity, cloud and infra, AI and
data, and local files. Each line carries the id to add it by, its transport, and what
it will ask for: `none`, an `oauth` login in the browser, an `api-key`, or `env` for
other environment such as a connection string.

```
$ mcpdial catalog
Docs and search
  context7   Context7   http   none   Up-to-date library documentation and code examples
  deepwiki   DeepWiki   http   none   Ask questions about any public GitHub repository
  ...
$ mcpdial add docs --catalog context7
saved docs (http https://mcp.context7.com/mcp)
```

`add NAME --catalog ID` saves one entry. Most entries point at the MCP registry and
are converted exactly as `--registry` converts them, below, so a required environment
variable arrives as a `${VAR}` placeholder with a note; the rest carry their
configuration in the catalog itself. `--json` prints the entries as objects, so a
program gets a short, trustworthy list instead of guessing package names.

The list ships inside the binary and is refreshed from this repository's `main`
branch at most once a day, cached under `MCPDIAL_HOME/catalog.json`. `--offline`, or
a refresh that fails, uses the built-in copy without comment. `MCPDIAL_CATALOG=URL`
or `MCPDIAL_CATALOG=PATH` reads the list from somewhere else, such as a checkout of
this repository. To add a server, open a pull request against
[catalog.json](catalog.json): CI checks the file's shape and that every registry
name still resolves.

### The registry, as the escape hatch

When a server is not in the catalog or in a host config you already have, the
[official registry](https://registry.modelcontextprotocol.io) may list it, with what
it runs as and what it needs. It holds the whole ecosystem, some twenty-seven
thousand entries with no review behind them, so it is where to look for a server
nothing else lists, not where to start. `mcpdial search` finds one in it:

```
$ mcpdial search github --limit 3
NAME                                TRANSPORTS         SOURCE    DESCRIPTION
io.github.github/github-mcp-server  http, stdio (oci)  registry  Connect AI assistants to GitHub - manage repos, issues, P...
io.github.Abhishekkumar2021/github  stdio (npm)        registry  GitHub via MCP: search, repos, issues, PRs, files, notifi...
io.github.pipeworx-io/github        http               registry  GitHub MCP — wraps the GitHub public REST API (no auth re...
3 of 210 matches; --limit N shows more
```

The registry's own search matches names alone, alphabetically, which for `github`
lists an Obsidian vault and three mirrors before GitHub's own server, and for
`browser automation` finds nothing. So `search` keeps a copy of the whole list under
`~/.config/mcpdial/registry/`, fetched once (a minute or two, with a progress line
at a terminal) and brought up to date with the registry's `updated_since` the next
time it is a day old. `--refresh` fetches it all again, `--offline` searches the copy
as it is, and a registry that cannot be reached is a note, not a failure, as long as
there is a copy. Every word of the query must appear in an entry's name, title or
description, case aside; the `io.github.` prefix on a name is not searched, since it
says where the code is hosted, not what the server is. Matches are ranked: an exact
name or title first, then `io.github.<vendor>` where the vendor is a query word, then
title matches, then name matches, then description matches. Within a rank, entries
with an HTTP remote or an npm package come before PyPI and container ones, a
namespace of more than a hundred entries is pushed down, and listings that share a
repository collapse to the newest. A matching entry the catalog lists goes first of
all, with `catalog` in the SOURCE column. Under `--json` the registry's own objects
are printed, untouched, in that order, so a program gets the same ranking. Nothing
matched is exit 1.

`mcpdial add NAME --registry <registry name>` then saves an entry without running
anything. The registry name is the entry's own `name`, as `search` prints it, like
`io.github.upstash/context7`:

```
$ mcpdial add ctx7 --registry io.github.upstash/context7
saved ctx7 (http https://mcp.context7.com/mcp)
note: headers this server accepts, not saved:
        Authorization (secret): API key for authentication. Accepts "Bearer <key>" or the raw key.
      pass one with -H 'Name: value'; for Authorization, --token-env VAR or `mcpdial login` also serve.
```

A Streamable HTTP remote is preferred when the entry has one, since there is nothing
to install; `--package npm|pypi|oci` picks a package instead, and `--remote` insists
on the remote. An npm package becomes `npx -y <package>@<version>`, a PyPI package
`uvx <package>==<version>`, and a container image `docker run -i --rm <image>`, each
with the arguments the entry lists. An SSE-only remote is saved with the same note
`import` gives.

Environment variables and headers the entry marks as required are saved as `${VAR}`
placeholders, so the file never holds a secret, and a note on stderr lists every
variable and header the server reads; the optional ones are not saved, since exported
in your shell they reach a local server anyway. Values the entry leaves to you, such
as a directory to serve, come from `--arg VALUE`, repeated in the order the note lists
them; a required one that is missing is exit 2 and nothing is saved. `--env`, `--cwd`,
`-H` and `--token-env` apply on top, as they do for `--http` and `--stdio`. Everything
read from the registry is quoted the way `import` quotes, one word per value, so a
crafted entry cannot add to the command line. `mcpdial ls` afterwards is the test.

`MCPDIAL_REGISTRY=URL` points at a private registry that serves the same API.

What was added this way is remembered: the entry's registry name and version are
saved under `source` in `servers.json`, `ls --no-probe` shows the name in a `SOURCE`
column, and its `--json` rows carry the object. A server added by hand or by `import`
has none.

### Quoting a stdio command line

A `--stdio` string, and the part of a `stdio:` target after the colon, is split into
argv by POSIX rules on every platform: whitespace separates, single and double quotes
group, and a backslash escapes the character after it. Windows is no exception, so a
native path spends its separators as escapes unless it is quoted or written with
forward slashes, which Windows accepts too:

```
mcpdial add fs --stdio "C:\tools\fs-server.exe C:\data"      # wrong: \t and \d are eaten
mcpdial add fs --stdio "'C:\tools\fs-server.exe' 'C:\data'"  # single quotes keep them
mcpdial add fs --stdio "C:/tools/fs-server.exe C:/data"      # or sidestep them
```

`mcpdial import` quotes what it reads, so servers brought over from another host need
nothing.

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
authorization server from the resource metadata, identify the client, open the browser
for the authorization-code grant with PKCE, catch the redirect on a loopback port,
exchange the code, and save the result. The browser step happens once. Afterwards the
access token is refreshed automatically when it expires, and once more on an unexpected
401, so a saved server keeps working indefinitely.

The client is identified in the order the spec asks for. `--client-id` names one
registered out of band (with `--client-secret` or `--client-secret-env` if it is
confidential). Otherwise, when the server's metadata says
`client_id_metadata_document_supported`, the client id is the URL of a client ID
metadata document that the server fetches for itself, so nothing is registered per
machine: [`docs/client-metadata.json`](docs/client-metadata.json), published by
[`.github/workflows/pages.yml`](.github/workflows/pages.yml) at
<https://obiemunoz.github.io/mcpdial/client-metadata.json> whenever it changes (the
repository's Pages source is GitHub Actions). `--client-metadata-url URL` presents a
document of your own instead, whether or not the server advertises support. Failing
both, a public client is registered dynamically (RFC 7591); `--no-client-metadata`
goes straight to that, for a server whose document support is broken. `login` says
which method it used, and so does `token show`.

The redirect URI is `http://127.0.0.1:PORT/callback`, and if the server refuses that,
`http://localhost:PORT/callback` is tried next, because some servers allowlist only the
name. If both are refused, the server is not following RFC 8252 and the error says what
to change; for a Doorkeeper server that is one line in `config/initializers/doorkeeper.rb`.
Use `--redirect-host` to skip the guessing.

**`mcpdial login NAME --grant client-credentials`** is for a cron job, a CI step or a
headless agent that owns a confidential client: pass `--client-id` and the secret from
stdin (`--client-secret`) or the environment (`--client-secret-env VAR`), never as an
argument, and the token is requested with the client-credentials grant. No browser is
involved, then or later: when the token expires it is renewed by running the grant
again with the saved secret. A server that advertises `grant_types_supported` without
`client_credentials` is refused up front, with the grants it does offer.

Servers must be addressed at their final URL. A redirect (say, `www.` to the bare host)
would turn the POST into a GET, so `mcpdial` refuses to follow it and names the URL to
use instead.

**`mcpdial token set NAME`** saves a token you obtained some other way, read from stdin or
from `--env VAR`. It is never accepted as a command-line argument, so it cannot land in
`ps` output or shell history.

**`--token-env VAR`**, on the command line or saved with `add`, reads the token from the
environment on every call and beats any saved credential.

**`${VAR}` in the config** does the same for any header, and for a stdio server's
environment, working directory and command line, and for the URL:

```
mcpdial add gh --http https://api.githubcopilot.com/mcp/ -H 'Authorization: Bearer ${GITHUB_TOKEN}'
```

The single quotes matter: they keep the shell from expanding the placeholder before
`mcpdial` sees it. It is saved as written and read from the environment each time
the server is dialed, so `servers.json` names the secret without holding it, and `ls
--no-probe` shows the name, never the value. `${VAR:-default}` supplies a fallback,
`$$` is a literal `$`, and nothing else is interpreted. A variable that is unset with
no default is exit 2 before anything is sent. `import` keeps such placeholders from a
host config, and `add --registry` writes them for what an entry marks as required.

## Where things live

```
~/.config/mcpdial/servers.json       what you configured (safe to share)
~/.config/mcpdial/credentials.json   tokens, refresh tokens, client ids (owner only)
~/.config/mcpdial/catalog.json       the catalog as last refreshed (a cache)
~/.config/mcpdial/registry/          a copy of the registry's list, for `search`
~/.config/mcpdial/run/NAME.sock      where a server kept running by `start` listens
```

The `run` directory is created mode 0700, so only its owner can reach a running
server through it.

Override the directory with `MCPDIAL_HOME`, or `XDG_CONFIG_HOME`. Removing a server with
`rm` also removes its credential. `MCPDIAL_REGISTRY` names the registry `search` and
`add --registry` consult, when it is not the official one; the copy remembers which
registry it came from, so pointing at another one starts a fresh copy. `MCPDIAL_CATALOG`
is a URL or file to read the catalog from.

"Owner only" is mode 0600 on unix. On Windows it is an access list naming the account
that ran `login`, applied as the file is created rather than after, so the tokens are
never on disk under the permissions the profile directory hands down.

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
servers may frame the reply as `text/event-stream` rather than JSON - carrying their own
notifications, logs and pings on that stream ahead of the answer - and the
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

### Cutting a release

1. Set `version` in `Cargo.toml` and generate the changelog section from the commits
   since the last tag with [git-cliff](https://git-cliff.org):

   ```bash
   git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md
   ```

   Pull requests are squash-merged, so each commit on `main` is one PR whose title is
   a conventional commit subject (`feat:`, `fix:`, ...); `cliff.toml` maps those to
   the Added, Fixed and Changed lists. Nothing in `CHANGELOG.md` is written by hand.
   `cargo test` fails until the version has a section with at least one entry. Merge
   that to `main`.
2. Tag the merge commit and push the tag:

   ```bash
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

The release workflow refuses a tag that does not match `Cargo.toml`, builds every
target in the table above, checks that each binary reports the tagged version, and
publishes a GitHub Release whose notes are that changelog section, with the archives
and their `SHA256SUMS` attached. Nothing is published to crates.io.

## License

MIT
