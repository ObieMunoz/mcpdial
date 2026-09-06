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
mcpdial add NAME ... [--allow GLOB]... [--deny GLOB]...   offer some of a server's tools, or all but some
mcpdial set NAME [--allow GLOB]... [--deny GLOB]... [--clear-allow] [--clear-deny]   change those lists; no flags shows them
mcpdial search QUERY [--limit N] [--refresh] [--offline]   the MCP registry, ranked, from a local copy
mcpdial import [FILE] [--from HOST] [--force]  pull servers from Claude, Cursor, Windsurf, VS Code, Codex, OpenCode configs
mcpdial export [NAME...] [--format mcpservers|vscode|codex] [--merge FILE]  the same servers in a host's shape, on stdout
mcpdial rm NAME
mcpdial catalog [--offline]      the reviewed list of servers, grouped by category
mcpdial browse [--all] [--offline]   tick catalog servers to save and dial; the saved ones start ticked
mcpdial pick                     pick a server and a tool, run it, print the call; what a bare `mcpdial` does

mcpdial ls [--no-probe]          every saved server, with live status and tool count
mcpdial tools [TARGET] [--long] [--all]  tools on one server, or on every server
mcpdial tools TARGET --snapshot FILE     write the tools in full, to commit beside a script
mcpdial tools TARGET --check FILE [--strict]   how they differ from that file; exit 3 on drift
mcpdial grep PATTERN [TARGET] [--tools] [--resources] [--prompts] [--instructions]
                     [-i] [-E] [-m N]    find one across every saved server at once
mcpdial info TARGET              server name, version, capabilities, instructions
mcpdial call TARGET TOOL ['{"json":"args"}' | @file.json | - | key=value ...] [--elicit JSON|@file]
mcpdial call TARGET TOOL ... --check FILE [--strict]   refuse to call a tool that drifted
mcpdial call TARGET TOOL ... --task [--ttl SECS]      the server runs it; poll until it finishes
mcpdial call TARGET TOOL ... --detach [--ttl SECS]    the server runs it; print the id and exit
mcpdial tasks TARGET             the background tasks that server is running
mcpdial tasks TARGET get ID      one task's status, without waiting
mcpdial tasks TARGET result ID   wait for one, then print its result as `call` would
mcpdial tasks TARGET cancel ID   ask the server to stop one
mcpdial schema TARGET TOOL       one tool's input schema
mcpdial resources TARGET [--long]  every resource, then every URI template
mcpdial read TARGET URI          one resource: text to stdout, bytes to a redirect or --save-dir
mcpdial --save-dir DIR ...       file image, audio and blob blocks as DIR/<tool>-<n>.<ext>
mcpdial --max-chars N ...        show at most N characters of a result; say so on stderr
mcpdial -o FILE ...              write the whole result to FILE; print one line saying so
mcpdial prompts TARGET [--long]  every prompt a server offers
mcpdial prompt TARGET NAME ['{"json":"args"}' | @file.json | - | key=value ...] [--elicit JSON|@file]
mcpdial complete TARGET prompt NAME ARG [VALUE] [--context JSON]     what the server suggests goes here
mcpdial complete TARGET resource TEMPLATE VAR [VALUE] [--context JSON]   the same for a uriTemplate variable
mcpdial raw TARGET METHOD ['{"json":"params"}' | @file.json | -]
mcpdial shell TARGET [--no-browser]  one session, many commands; state persists between calls
mcpdial start NAME [--idle SECS] keep a stdio server running; later commands share its session
mcpdial stop NAME                end it
mcpdial serve NAME [--listen ADDR] [--bearer-env VAR] [--allow PAT]... [--deny PAT]...
mcpdial serve NAME --stdio       the same, on this process's own stdin and stdout

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

Global flags: `--json` for machine output (or `MCPDIAL_JSON=1`), `-v` to trace every
message on stderr, `--trace FILE` to append every message and transport event to a file
as JSON Lines, `--timeout SECS` (or `MCPDIAL_TIMEOUT`), `-H` for extra headers,
`--token-env VAR` to force a token from the environment, `--user-agent` to override the
default browser UA (or `MCPDIAL_USER_AGENT`), `--protocol-version VERSION` to name one
MCP revision instead of working out which the server speaks,
`--no-daemon` to dial a server afresh even while `start` has one running,
`--no-retry` to fail on the first transient HTTP failure instead of sending the
request once more, `--progress` to print a line on stderr for every progress
notification a server sends during a call, `--log-level LEVEL` to move the line a
server's own log messages print from, `--max-chars N` (or `MCPDIAL_MAX_CHARS=N`) and
`--output FILE` (`-o`) to keep a large result out of a context window, `--plain` (or
`MCPDIAL_PLAIN=1`) to print at a terminal exactly what a pipe would get, `--no-pager`
to print long output straight to the terminal, `--raw` to print a server's text as it
was written instead of rendering its markdown, and `--color always|never|auto` to say
when to colour: `auto`, the default, colours at a terminal unless `NO_COLOR` is set,
and `always` colours into a pipe for `less -R`.

`--max-chars` and `--output` apply to `call`, `prompt`, `read`, `raw` and `shell`.
A result over the bound is cut between characters, never inside one; under `--json`
it is replaced by an object with a `truncated` key rather than half rewritten, so a
program can tell a head from a whole result by testing for that one key. `--output`
writes the payload exactly - the result object under `--json`, the rendered text
otherwise, and a resource's bytes unchanged, which is the other way past the
redirect a terminal asks a `read` of a blob for. It refuses a path that already
exists or is a directory, before the server is dialed. `mcpdial guide` has the
detail.

What a pipe gets is frozen. Everything mcpdial prints goes through one of two
presenters: `Plain`, chosen for a pipe, for `--json`, for `--plain`, for
`MCPDIAL_PLAIN` and for `TERM=dumb`, is the same bytes release after release, and
`Rich`, chosen for a person at a terminal and for the pipe that asked for colour
with `--color always`, is the one place the output may differ: the `STATUS` column
of `ls` and the `error:` prefix carry their meaning in colour, JSON (`schema`,
`info`, `raw`, a tool's `structuredContent`) comes with its keys, strings, numbers,
booleans and null in colour, and a PNG a tool returns is drawn under its placeholder
line on the terminals that can show one. `NO_COLOR`, and `--color never`, turn the
colour off. A snapshot test holds `Plain` to its word; see Development below.

An image is drawn only where the terminal names itself in the environment:
`TERM_PROGRAM=iTerm.app` and `TERM_PROGRAM=WezTerm` get iTerm2's OSC 1337 sequence,
and `TERM_PROGRAM=ghostty`, `KITTY_WINDOW_ID`, `TERM=xterm-kitty` and
`TERM=xterm-ghostty` get kitty's APC `_G` chunks. The bytes travel as the PNG the
server sent, so nothing else - a JPEG, a GIF, an SVG - is drawn, and neither is
anything inside `tmux` or `screen`, whose passthrough is off by default. A terminal
that does not say what it is keeps the `[image image/png, 84 KB]` placeholder and
nothing more, because an escape sequence sent where it is not understood spills the
base64 across the screen. The placeholder line stays either way, `--save-dir` still
names the file it wrote in it, and a pipe, `--json` and `--plain` print what they
always have.

Most tool results are markdown, so at a terminal the text `call` and `prompt` print
is rendered rather than shown with its asterisks and pipes: headings in bold, lists
as bullets, tables in a box, code in a dim block. Only a block that says it is
markdown - a heading, a list item, a fenced code block, a table row or a block quote
in its first lines - is rendered, so a one-line answer is never touched, and the
rendering keeps every line break a server sent, wrapping only a line wider than the
screen. `--raw` prints the markdown as it was written, for copying it back out.

At a terminal, the output of `call`, `read`, `prompt`, `raw`, `schema`, `tools
--long` and `info` goes through a pager when it is taller than the screen, as
`git log` does: `$MCPDIAL_PAGER`, else `$PAGER`, else `less -RFX`. The same holds
for each command typed in `shell`, whose prompt returns when the pager exits.
`--no-pager`, or `MCPDIAL_PAGER=` set empty, prints it straight; a pipe never
pages, and a pager that cannot be started is skipped without a word.

At a terminal, a `call` that leaves out something the tool requires is asked for it
rather than refused. The tool's own schema decides the question: a string is a line,
a number is a line read as one, a boolean is `y/n`, an array or an object is a line
of JSON, and named values are a numbered list, or `fzf` where it is installed.
Missing optional arguments are offered too, and Enter leaves them out. Every answer
is read exactly as the same text in a `key=value` pair would be, and the finished
call is printed as the one-line command that would have made it, ready to paste into
a script. Nothing is asked unless stdin is a terminal: under `--json`, under
`--plain`, and with anything piped in, a missing argument is the error and the usage
line it has always been.

A bare `mcpdial` at a terminal, with servers already saved, picks one: the saved
servers with the status of their last probe, then that server's tools with the first
line of what each one is for, then the arguments asked for from the tool's schema
exactly as above. The call runs, the result is shown, and the line that would have
made the same call outright is printed under it in dim text, quoted so it pastes
straight into a script:

```
$ mcpdial
  1) echo  connected  stdio  5 tools
server (1-1): 1
  1) echo   Echo a message back.
  2) fail   Always returns a tool error.
tool (1-2): 1
  message (string, required): hello there
Echo: hello there
mcpdial call echo echo 'message=hello there'
```

`mcpdial pick` is the same thing spelled out, for anyone who aliases the bare form.
Picking is `fzf` where it is on `PATH` (`--height 40% --reverse`), and the numbered
list above where it is not; either the number or the line itself answers, and `^D`
or Esc leaves without sending anything. With nothing saved yet the bare form opens
`browse` instead. Under a pipe, under `--json` and under `--plain` a bare `mcpdial`
is the usage error and exit 2 it has always been, and `mcpdial pick` says it needs a
terminal rather than reading one, so a script that calls either by mistake still
fails at once instead of waiting for a keystroke.

`--timeout` bounds every wait: the flag on the command line, else `MCPDIAL_TIMEOUT`,
else the timeout saved with the server, else 60 seconds. `add --timeout SECS` saves one
for a server that installs packages on first launch or runs tools for minutes, `ls
--no-probe` shows it, and `import` keeps a numeric `timeout` (seconds) it finds in a
host's config.

mcpdial speaks both eras of the protocol. A session opens with `server/discover`,
which is all that revision `2026-07-28` has; a server that has never heard of it gets
the `initialize` handshake instead, offering `2025-11-25` and running on whichever of
`2025-11-25`, `2025-06-18` and `2025-03-26` it answers with. `mcpdial info` shows the
version in use either way, and a server that names one mcpdial does not speak is
reported as such. On `2026-07-28` there is no handshake to stand behind a request, so
each one carries the version, the client's identity and its capabilities itself and
mirrors its method and subject into the `Mcp-Method` and `Mcp-Name` headers.
`--protocol-version VERSION` skips the working out - `2025-11-25` holds a server that
serves both eras to the handshake, `2025-06-18` suits one that misbehaves when offered
anything newer - and `add --protocol-version` saves the choice.

That revision also renumbered the answer to a resource that is not there, from the
`-32002` every revision before it used to a plain `-32602`. mcpdial reads both, and a
`read` or a `prompt` that finds nothing carries a hint naming the listing that would
have shown what does exist, so the number itself never has to be looked up.

### Watching a long call

A crawl, a build or a browser session can run for minutes with nothing on the screen.
Servers report as they go, in `notifications/progress` and `notifications/message`,
but only for a request that invited them to: mcpdial sends a `progressToken` on a
`call`, `prompt` or `read` when there is somewhere for the answer to go, and never
otherwise.

At a terminal that is one updating line on stderr. Anywhere else `--progress` asks
for it explicitly, one plain line per notification, so a captured log holds them:

```
$ mcpdial --progress call crawler crawl '{"url":"https://example.com"}' > pages.json
progress: 1/12 fetching https://example.com
progress: 2/12 fetching https://example.com/about
...
```

A server's own log messages print on stderr from `warning` up, as `server [warning]
crawler: rate limited`. `--log-level LEVEL` moves that line - the eight RFC 5424
names, `debug` through `emergency` - and a server that advertises the `logging`
capability is told the level as well, so it need not send what would only be
filtered here.

Under `--json` both arrive on stderr as one object per line,
`{"notification":{"method":"notifications/progress","params":{...}}}`. stdout is the
result and nothing else, whatever the server says on the way.

### Starting a call and coming back for it

Protocol 2025-11-25 lets the server run a tool call in the background and hand back a
task id instead of a result. `--task` starts one and polls it to the end, at the
interval the server asks for, showing whatever it says about itself on stderr at a
terminal:

```
$ mcpdial call crawler crawl '{"url":"https://example.com"}' --task
```

The exit code is `call`'s: 0, or 1 when the result carries `isError` or the task ended
`failed` or `cancelled`. A tool whose own metadata says `execution.taskSupport:
"required"` is started as a task without being asked, with a note on stderr saying so,
rather than refused.

`--detach` starts the task, prints the id on stdout, and exits 0:

```
$ id=$(mcpdial call crawler crawl '{"url":"https://example.com"}' --detach)
$ # ... do something else ...
$ mcpdial tasks crawler get $id
task t-4f9
status working
tool crawl
message 12 of 40 pages
$ mcpdial tasks crawler result $id > pages.json
```

`--ttl SECS` asks the server to keep the task for that long (an hour by default); what
it agreed to is what mcpdial notes. `mcpdial tasks TARGET` lists what the server is
running, `cancel ID` asks it to stop, and every one of them takes `--json`.

The task is the server's, not mcpdial's: nothing runs in the background here, and
killing mcpdial mid-poll leaves the task running for `tasks TARGET result` to collect.
That is also the catch for a stdio server, whose process this command spawned and
would take with it, so `--detach` against one is refused until [`start`](#keeping-a-stdio-server-running-between-calls)
is holding it open. An HTTP server needs nothing: it was always somewhere else. One
that scopes tasks to an HTTP session may still not recognise an id from a different
invocation, and says so when asked.

What is kept on this machine is a note, in `tasks.json`: which ids were started here,
which tool each was, and how long the server said it would keep it. It is read to know
what to ask about and to put a tool name beside a status that carries none; `tasks/get`
is the only thing that says what state a task is in. Notes are dropped as soon as
anything sees the task finish, and dropped anyway once the ttl has passed, so a note
whose server has forgotten it does not accumulate.

### Arguments, without the quoting

A tool's arguments are one JSON object, which a shell fights over every quote of. They
can be `key=value` pairs instead, and mcpdial types each one from the tool's own
`inputSchema` before it sends the call:

```
mcpdial call fs read_text_file path=/tmp/x
mcpdial call ctx7 search query="rust ureq" limit=5 fuzzy=true   # 5 is a number, true a boolean
mcpdial call srv tool tags:='["a","b"]' id:='"123"'             # := is JSON, whatever the schema says
```

A value stays a string unless the schema names one scalar type for it, so a union type
or a key the schema never mentions is sent as typed. An array or an object has to come
through `:=`, and a key the schema shuts out (`additionalProperties: false`) is refused
before anything is sent, with the nearest property named. The first word after the tool
name picks the form: `{`, `@` or a bare `-` means the JSON object, and the two forms
cannot be mixed. `prompt` takes pairs too, where every value is a string.

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

A parameter that takes more than one shape is named as all of them
(`repoName: string|string[]`), an array as what it holds (`pages: object[]`), and an
object with its own fields one level in (`at: object {x: number, y?: string}`), where a
`?` marks a field the schema does not require. The same lines follow a call the server
rejected, so a retry needs no second lookup; `mcpdial schema TARGET TOOL` prints the
whole `inputSchema` when the summary is not enough.

What a tool says about *itself* goes on the name line: the server's own `title` where it
says more than the name does, then a tag for every annotation hint the tool set —
`[read-only]`, `[destructive]`, `[idempotent]`, `[open-world]` — and `[task:optional]`
or `[task:required]` where the server will run the tool as a task.

```
$ mcpdial tools files --long
erase  "Erase a file"  [destructive] [open-world]
    Remove a file permanently.
  parameters:
    path: string (required)
```

Only a hint the server actually set is printed. The spec's default for `destructiveHint`
is true, so a tool that annotates nothing is one that said nothing, not one that promised
to be safe. The short listing has room for a single hint, and marks a destructive tool
with a trailing `*` on its name:

```
$ mcpdial tools files
  read_file                    Read a file.
  erase*                       Remove a file permanently.
```

The same tags head `mcpdial schema TARGET TOOL`, the shell's `help TOOL`, and the usage
block printed under a rejected call, since that is where a caller looks next.

Under `--json` the same flag says how much of the server's own object comes back. A
listing is each tool's `name` and the first line of its `description`, which is what it
takes to choose one; `mcpdial schema TARGET TOOL` then hands over that tool whole,
`inputSchema` and all. `--long` is every tool whole at once, which on a server with
fifty of them is tens of kilobytes. `resources` and `prompts` list the same two ways.

### Pinning what a server promised

A script written against `read_text_file(path)` breaks silently when the server renames
the parameter or adds a required one. Write down what the server offers today, commit
the file next to the scripts that depend on it, and check it before the work starts:

```
$ mcpdial tools files --snapshot tools.json
wrote 14 tool(s) to tools.json

$ mcpdial tools files --check tools.json; echo "exit=$?"
ok
exit=0
```

Months later, the server has moved:

```
$ mcpdial tools files --check tools.json; echo "exit=$?"
tool read_text_file: required property path was removed
tool read_text_file: property file is new and required
info: tool list_directory_with_sizes: not in the snapshot
2 differences
exit=3
```

Exit 3 is drift and nothing else - not the 1 that means the server refused - so a CI job
can branch on it. Lines prefixed `info:` are changes nobody breaks on (a new tool, a new
optional parameter, a requirement the server dropped); they are printed and leave the
exit code alone. `--strict` makes them failures too, and holds each snapshotted tool to
the object the snapshot holds, key for key. `--json` prints
`{"ok": bool, "differences": [{"tool", "kind", "detail", "level"}]}`.

The file is the server's whole tool objects, `inputSchema` and all, sorted by name with
every object's keys sorted, so a diff is only what changed. `mcpdial call TARGET TOOL
ARGS --check tools.json` runs the same comparison for that one tool and refuses to send
anything when it fails. A snapshot is never written over: a path that exists, or is a
directory, is exit 2 before the server is dialed.
### Finding one across every server

Ten saved servers is ten `tools` listings to read before the one that "creates a
calendar event" turns up. `mcpdial grep` dials them all at once and runs a pattern
over everything they offer:

```
$ mcpdial grep wiki
wiki
  tool      read_wiki_structure       Get a list of documentation topics for a repository
  tool      ask_question              Ask any question about a GitHub repository
notes
  resource  file:///wiki/index.md     The wiki index
```

Tools match on name, title, description and the names and descriptions of the
parameters in their `inputSchema`; resources on URI, name, description and mime type,
and templates on their `uriTemplate`; prompts on name, description and argument names;
and the server's own `instructions` are read a line at a time, the way grep reads a
file, so the line the pattern is on is the line reported. `--tools`, `--resources`,
`--prompts` and `--instructions` narrow the search to those kinds and combine, and only
the listings they ask for are fetched — a server that never advertised `resources` is
never asked for them.

`PATTERN` is a substring by default. `-E` reads it as a regular expression, `-i`
ignores case, and `-m N` reports at most N matches. Exit 1 means nothing matched, so
`grep` composes: `mcpdial grep calendar --json && ...`. A pattern `-E` cannot compile is
exit 2, before anything is dialed.

Under `--json` the answer is `{"matches": [...], "skipped": [...]}`, each match carrying
`server`, `kind`, the `name` the next command needs, a one-line `description`, and the
`matched` field the pattern was found in under the server's own word for it
(`description`, `parameter`, `uriTemplate`, `argument`, …), so `grep` and then
`schema TARGET TOOL` is two requests instead of a listing per server.

Dialing several servers is several ways to be let down, and none of them ends the
search. A server that is down, or wants a token nobody saved, is one entry under
`skipped` carrying the same `status` object `ls` reports for it, one line on stderr
without `--json`, and every other server is searched anyway. Each dial is bounded by the
ten seconds a status probe waits rather than the minute a call gets, so one hung server
costs ten seconds and not the whole search; `--timeout` and a server's own saved timeout
still win. Naming one TARGET searches that server alone, and one that will not answer is
then the command's own error, as it is for `tools TARGET`.

`-E` is the one feature here with a dependency behind it: `regex-lite`, which is pure
Rust, pulls in nothing itself, and costs the static binary far less than the full
`regex` engine's Unicode tables. Substring matching, which is the default, needs nothing.

### Allowing and denying tools

A filesystem server with fourteen tools is usually wanted for three of them, and an
agent that can see `delete_file` will eventually call it. A saved server can carry an
allow list and a deny list of glob patterns (`*` and `?`, matched against the tool
name), and the restriction travels with the name into every command, the shell included:

```
$ mcpdial add fs --stdio "npx -y @modelcontextprotocol/server-filesystem /srv" \
      --allow 'read_*' --allow list_directory --deny 'delete_*'
$ mcpdial set fs                     # show the lists
$ mcpdial set fs --deny 'delete_*' --deny move_file   # replace the deny list
$ mcpdial set fs --clear-allow       # offer every tool not denied
```

Deny wins over allow, and an empty allow list means every tool not denied. `tools fs`
and `ls` count and show only what is permitted; `tools fs --all` shows the hidden ones
too, marked `(denied)`. Calling a hidden tool is exit 2 with nothing sent:

```
$ mcpdial call fs delete_file '{"path":"/srv/x"}'
error: delete_file is denied for fs by its deny list; edit with mcpdial set fs
```

The lists apply to the saved name alone; an ad-hoc URL or `stdio:` target has no
config to carry one. `raw` is the escape hatch and is never filtered.

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
`resources`, `read`, `prompts`, `prompt`, `raw`, `elicit`, `show`, `save`, `retry`,
`edit`, `subscribe`, `unsubscribe`, `subscriptions`, `listen`, `info`, `help`, and
`quit` (`exit` ends the session too); a `#` starts a comment. With `--json` each result
is one line of JSON. In a script, any failed command makes the exit code 1 after the
script finishes.

Arguments are one JSON object, or the same `key=value` pairs the command line takes.
When a line does not work, the answer says what the tool actually takes, in both forms,
rather than leaving you to go read the schema:

```
chrome> list_pages
error: list_pages is a tool, not a command
usage: call list_pages {}
chrome> call new_page
error: MCP error -32602: Invalid arguments for tool new_page: Required at url
usage: call new_page {"url": "<string>"}
   or: call new_page url=<string>
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
leaves). Inside a `call`'s arguments object Tab completes the tool's own parameter
names, one nested object deep, offering only the keys the object does not have yet, and
completes the values of a parameter whose schema names them:

```
chrome> call new_page {"u<TAB>
chrome> call new_page {"url":                     # the one match completes itself

chrome> call navigate_page {"url": "x", "<TAB>
"pageId":     "timeout":                          # url is written, so it is not offered
```

A `prompt`'s arguments complete the same way, from the names the prompt declares. Where
only the server knows what a value may be - a prompt argument, or one variable of a
`uriTemplate` - Tab asks it, if it declared the `completions` capability:

```
docs> prompt summarize {"style": "th<TAB>
docs> prompt summarize {"style": "thorough"       # completion/complete answered

docs> read file:///notes/we<TAB>
file:///notes/weekly.md                           # the template's variable, filled in
```

That request is a network round trip inside a keystroke, so it is given two seconds and
no more: a server that is slow, wedged or gone costs one Tab that much, leaves the line
exactly as it was, and prints nothing into the middle of it. `-v` traces it like any
other message. `mcpdial complete` asks the same question from outside the shell.

History is kept per saved server in `~/.config/mcpdial/history-NAME`. Piped input is
read plainly, exactly as before, with no editing, no history and no completion, so
scripts are unaffected.

### The shell remembers what it printed

Every `call`, `read`, `prompt` and `raw` result is numbered as it goes out - a dim
`[3]` before it at a terminal, nothing at all under a pipe, where the bytes are the
contract. A later line names one instead of running it again:

```
chrome> call take_screenshot
[1] [image image/png, 84 KB]
chrome> save 1 home.png
wrote 86,016 bytes to home.png
chrome> call navigate_page url=https://example.com
[2] Successfully navigated to https://example.com.
chrome> retry url=https://example.com/about
call navigate_page {"url":"https://example.com/about"}
[3] Successfully navigated to https://example.com/about.
```

- `show N` prints result N again, exactly as it printed the first time. `_` names the
  last result and `$3` the third, and a bare `show` means `_`.
- `save N FILE` writes it: text as text, a binary block as its bytes, a resource as the
  bodies it arrived as, and the whole result object under `--json`. `save N` with no
  file names one after the tool and the media type - `take_screenshot.png`,
  `tools_list.json`. It refuses to write over a file that is already there, to write to
  a directory, or to write where it cannot, in the same words `--output` uses.
- `retry` sends the last call again. `retry key=value ...` sends it with those
  arguments changed, typed by the tool's schema exactly as a `call` line is;
  `retry TOOL key=value ...` looks back for the last call of that tool instead. A call
  that failed is remembered too, which is usually the one worth running again.
- `edit N` opens the arguments of the call that made result N in `$EDITOR` (or
  `$VISUAL`) and sends them when it exits; a bare `edit` takes the last call. An editor
  that exits badly, or that leaves the file empty, sends nothing.

The numbers themselves only appear at a terminal, but the commands work under a pipe
too: a script that counted its own calls can `show 2` or `save 2 out.txt` just the
same. A session holds its last 50 results, or 8 MiB of them, whichever runs out first,
and drops the oldest past that; the newest is always kept. Nothing is written to disk
and nothing outlives the process.

### When the server has a question

Some servers stop halfway through a call and ask for one more fact - a confirmation,
a region, a parameter nobody passed - with `elicitation/create`. At a terminal mcpdial
puts the question to you, one property at a time, with its type, its bounds and its
default, and Enter alone takes the default:

```
$ mcpdial call deploy release '{"app":"api"}'
server asked: confirm before running
confirm (yes/no): y
region (1) us 2) eu): 2
```

Unattended there is nobody to ask, so the request is **declined** rather than left
hanging: a decline is the answer the spec has for "the value is not coming", and the
server carries on without it. Unattended is whatever a missing argument treats as
unattended - a pipe on any stream, `--json`, `--plain`, `MCPDIAL_PLAIN` or a dumb
terminal - so one rule covers both questions. To answer without a human, hand the values
over up front:

```
$ mcpdial call deploy release '{"app":"api"}' --elicit '{"confirm": true, "region": "eu"}'
$ mcpdial call deploy release '{"app":"api"}' --elicit @answers.json
```

A form whose required properties those cover is accepted with them; one they miss, or a
value the schema's own bounds forbid, is declined without being sent. In the shell,
`elicit {"confirm": true}` sets the answers for every call after it. A url-mode request
is a different thing: its address is printed, opened in a browser unless `--no-browser`,
and accepted at once, because the rest happens out of band.

mcpdial declares only what can really answer, so a server that checks does not ask for
what it will not get. Which servers can be answered depends on how they ask. Before
revision `2026-07-28` the question is a request sent while the call is still running, and
only a transport that can carry a reply back gets a declaration: stdio does, `mcpdial
start` included; Streamable HTTP does not, because the reply would need a second POST
while the first is still open.

`2026-07-28` turns the question into a returned value - `resultType: "input_required"`,
with the requests to answer and an opaque `requestState` - and the answer into the same
call sent again, carrying `inputResponses` and that state. A fresh request is something
every transport can send, so on that revision **Streamable HTTP is answered too**. A
server that keeps asking is given up on after four rounds rather than answered for ever,
and `raw` prints the `input_required` result as it came, since it was asked to send one
request.

### Following what a server changes

A session is the one place a server's own notifications mean anything, and a long one
outlives what it was told at the start: a server that adds a tool, or rewrites a
resource, has moved on from the lists the shell cached for `help` and Tab completion.
`subscribe URI` follows one resource, with a file to keep in step with it or nothing, in
which case its new contents are printed:

```
$ mcpdial shell logs
logs> subscribe file:///build/latest.log build.log
following file:///build/latest.log -> build.log
logs> call rebuild {}
started build 4821
file:///build/latest.log -> build.log (1841 bytes)
logs> subscriptions
1 subscription(s):
  file:///build/latest.log -> build.log
logs> unsubscribe file:///build/latest.log
no longer following file:///build/latest.log
```

The file is replaced whole, through a sibling and a rename, so whatever is reading it
sees the old contents or the new ones and never half of either. `unsubscribe` for a URI
this session never followed is an error naming it, rather than a silent success that
would leave the real subscription running, and a resource that has gone away reports the
failed read once and leaves the session and every other subscription where they were.

Nothing is acted on in the middle of a command. A notification arriving during a call is
put aside and the lists re-read at the prompt after it, so a line about it never lands
between two lines of the server's, and five hundred reports of one change are still one
refresh. **A piped session hears none of it**: the prose is a dim line for a person
watching, and under `--json` the same fact is one object on stderr for a script to react
to.

```
$ mcpdial --json shell logs < script.txt 2> notifications.jsonl
$ cat notifications.jsonl
{"notification":{"method":"notifications/tools/list_changed"}}
{"notification":{"method":"notifications/resources/updated","params":{"uri":"file:///build/latest.log"}}}
```

Which request carries all this depends on the revision the session settled on. Up to
2025-11-25 `subscribe` sends `resources/subscribe`, and the server pushes its updates on
whatever stream is open, so they arrive with the next command's reply. 2026-07-28 removed
both `resources/subscribe` and the GET stream in favour of `subscriptions/listen`, one
long-lived response stream that a client opts into per notification type: there
`subscribe` sends nothing, and `listen [SECONDS]` opens the stream, collects what arrives
and hands the prompt back. The bound is a wall clock, and the default is five seconds, so
a server that acknowledges nothing costs a wait rather than a session.

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
### Lending a server to a sandbox, without lending its token

`mcpdial serve` puts a saved server on loopback as a plain MCP endpoint. It dials the
upstream with the credential you logged in with once, and hands the client the protocol
and nothing else: the upstream `Authorization` and any saved headers are added on the
proxy's side, and the client's own headers never travel upstream. So an agent in a
sandbox can use a server it has no credential for, and still has none if it is
compromised.

```
$ mcpdial login work                                          # once, in the browser
$ export SANDBOX_TOKEN=$(openssl rand -hex 16)
$ mcpdial serve work --listen 127.0.0.1:8321 --bearer-env SANDBOX_TOKEN --deny 'delete_*'
serving work on http://127.0.0.1:8321/mcp
clients must send Authorization: Bearer $SANDBOX_TOKEN
^C to stop
```

and in the sandbox, which holds `SANDBOX_TOKEN` and nothing else:

```
$ mcpdial add work --http http://127.0.0.1:8321/mcp --token-env SANDBOX_TOKEN
```

`--listen` defaults to `127.0.0.1:0`, whose chosen port is printed (and is `serve.url`
under `--json`); an address that is not loopback takes `--listen-any`, since anyone who
can reach the port can use the server behind it. `--bearer-env VAR` makes clients present
that value, and a wrong or missing one gets a 401 without the upstream being dialed at
all. `--allow` and `--deny` take globs (`*`, `?`) matched against tool names: a denied
tool is left out of `tools/list` and a call to it is refused with `-32602`. Deny beats
allow, an empty allow list admits everything not denied, and the lists saved with the
server (`add --deny`, `set --deny`) still hold on top of these.

`mcpdial serve NAME --stdio` speaks the same MCP on its own stdin and stdout instead, so
a host application can list it as a stdio server and reach a remote server it could not
otherwise authenticate to:

```json
{"mcpServers": {"work": {"command": "mcpdial", "args": ["serve", "work", "--stdio"]}}}
```

The client's `initialize` is answered here, with mcpdial's identity over the upstream's
capabilities minus the ones a proxy cannot relay, and each client session opens an
upstream session of its own. Token refresh happens upstream, invisibly. `^C` ends every
upstream session and exits 0.

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

### Exporting to a host

`mcpdial export` is `import` pointed the other way: it writes saved servers out in the
shape a host reads and prints them, so a set tested here with `ls` can be pasted into
Claude Code, Cursor, Windsurf, VS Code or Codex without retyping. `--format mcpservers`
(the default) writes the `mcpServers` object; `vscode` writes `servers` with an explicit
`type`; `codex` writes `[mcp_servers.NAME]` tables. With no names every saved server is
exported. `--merge FILE` prints that host's file with the exported entries replaced or
added and everything else in it left alone; nothing is ever written for you, so the
redirect and the diff stay yours. Exit 2 if a named server is not saved.

```
$ mcpdial export fs > .mcp.json
fs: its allow and deny lists are not exported; the host will offer every tool
$ mcpdial export --format codex --merge ~/.codex/config.toml > merged.toml
```

No credential is exported. A saved OAuth token stays here and the host is told to log in
for itself; a `token_env` travels as the `${VAR}` placeholder it is, as an `Authorization:
Bearer ${VAR}` header for the hosts that expand one and as Codex's own
`bearer_token_env_var`. Each thing left behind is named on stderr, one line per server
(`{"note": "..."}` under `--json`), so nothing goes missing silently.

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

`mcpdial browse` is the same list as a checklist, the way LazyVim's extras or Mason
read: one line per entry with a box, the id, the name, the transport, what it asks for
and the summary, grouped by category. Entries already saved start ticked, found by
the source `add --catalog` and `add --registry` record. Tick what you want and press
Enter: each new entry is converted as `add --catalog` converts it, anything the entry
leaves to you (a directory to serve, a required `${VAR}`) is asked for at the prompt,
and it is saved under its catalog id, or `id-2` when that name is taken. The new
servers are then dialed together and their `ls` rows printed, with a `mcpdial login
NAME` line under any that says `auth required`. Unticking a saved entry removes it,
after one line asking to confirm. Esc leaves with nothing changed. A bare `mcpdial`
at a terminal opens the checklist as long as nothing is saved yet; once something is,
it picks one of what is saved instead.

With [fzf](https://github.com/junegunn/fzf) on `PATH` the list is fzf's, `--multi`
with a preview pane showing the entry, what it will ask for and the exact `add`
command; the saved entries sit at the top and marking one removes it. Without fzf a
small picker of mcpdial's own takes the screen: arrows move, Space ticks, `/` filters,
Enter applies. `browse --all` swaps the catalog for the whole registry index `search`
keeps, which fzf filters comfortably; the built-in picker stays with the catalog.
Under a pipe, or with `--json` or `--plain`, `browse` prints the entries as objects,
exactly what `catalog --json` prints (`--all` prints the registry's own objects), so a
program never meets a picker.

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
Use `--redirect-host` to skip the guessing. Dynamic registration declares
`application_type: "native"`, without which an OpenID Connect registration endpoint reads
the client as a web application and refuses a loopback redirect on principle.

The authorization server is held to its own identity. The `issuer` of its metadata is
recorded beside the PKCE verifier before the browser is opened anywhere, and the `iss`
the authorization response comes back with (RFC 9207) has to be that same string, byte
for byte, or the code is never sent to a token endpoint: a response from somewhere else
is a mix-up attack, and nothing in it is acted on, not even its error message. A server
that advertises `authorization_response_iss_parameter_supported` and then sends no `iss`
is refused too. That issuer is saved with the credential and shown by `token show`,
because a client id belongs to the authorization server that granted it: if the server
behind a URL moves to a different one, a dynamically registered client is registered
again there, and a client registered out of band is not quietly presented to a server
that never issued it — the mismatch is reported, and `--client-id` names one that fits.

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

**`mcpdial logout NAME`**, and `mcpdial token rm NAME`, delete the saved credential and
exit 0 whether or not there was one to delete, so either is safe to run twice. A name
that is neither saved nor a URL or `stdio:` target names nothing to log out of, and is
exit 2 the way `mcpdial rm` answers one.

**`--token-env VAR`**, on the command line or saved with `add`, reads the token from the
environment on every call and beats any saved credential. `ls` names the variable in its
AUTH column, as `$VAR`, whether or not the servers were dialed.

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
~/.config/mcpdial/config.json        how mcpdial behaves: where credentials are kept
~/.config/mcpdial/catalog.json       the catalog as last refreshed (a cache)
~/.config/mcpdial/registry/          a copy of the registry's list, for `search`
~/.config/mcpdial/tasks.json         background task ids started here (a note, not the truth)
~/.config/mcpdial/run/NAME.sock      where a server kept running by `start` listens
```

The `run` directory is created mode 0700, so only its owner can reach a running
server through it.

Removing a server with `rm` also removes its credential. The copy of the registry
remembers which registry it came from, so pointing `MCPDIAL_REGISTRY` at another one
starts a fresh copy.

Every environment variable mcpdial reads:

| Variable | What it does |
|---|---|
| `MCPDIAL_HOME` | The directory above, instead of the default |
| `XDG_CONFIG_HOME` | The same one level up: `$XDG_CONFIG_HOME/mcpdial` |
| `MCPDIAL_JSON=1` | `--json` on every command |
| `MCPDIAL_TIMEOUT=SECS` | `--timeout SECS` on every command |
| `MCPDIAL_USER_AGENT=NAME` | `--user-agent NAME` on every command |
| `MCPDIAL_PLAIN=1` | `--plain` on every command |
| `MCPDIAL_NO_DAEMON=1` | `--no-daemon` on every command |
| `MCPDIAL_TRACE=FILE` | `--trace FILE` on every command |
| `MCPDIAL_REGISTRY=URL` | The registry `search` and `add --registry` consult |
| `MCPDIAL_CATALOG=URL` | A URL or a file to read the catalog from, instead of the official list |
| `MCPDIAL_CREDENTIALS=file\|keychain` | Where tokens are kept, whatever `config.json` says |

A flag on the command line beats its variable, which beats the built-in default. The
`=1` ones are off when unset, empty or `0`, and on for anything else. A
`MCPDIAL_TIMEOUT` that is not a non-negative number of seconds is exit 2, since a
typo that silently left every call on 60 seconds would never be found.

"Owner only" is mode 0600 on unix. On Windows it is an access list naming the account
that ran `login`, applied as the file is created rather than after, so the tokens are
never on disk under the permissions the profile directory hands down.

### Keeping tokens in the OS keychain instead

An owner-only file is still a plaintext file, readable by anything else running as
you. If that is not good enough, move the tokens into the keychain the system
already has:

```
mcpdial config credentials keychain   # moves every saved token in, deletes the file
mcpdial config credentials            # says which store is in use, and why
mcpdial config credentials file       # moves them back out again
```

Each credential becomes one item under the service `mcpdial` with the server's name
as the account: the macOS keychain through `security`, the Secret Service
(GNOME Keyring, KWallet) through `secret-tool`, and the Windows Credential Manager
through its own API. No crate is added for any of it and the binary stays one file.
Every command reads and writes tokens the same way whichever store is in use;
`token show` names the keychain when that is where the token is.

Switching stores copies before it deletes, so an interrupted move leaves every
credential in at least one of the two and running the command again finishes it.
`MCPDIAL_CREDENTIALS=file` overrides the setting for one run, which is how a CI job
holds itself to the file whatever the config directory it inherited says; the
setting cannot be changed while that variable is set, since the change would not
take effect.

Where there is no keychain - a headless Linux session with no Secret Service, or no
`secret-tool` installed - `mcpdial config credentials keychain` fails and changes
nothing. It never falls back to the file quietly: the file is where you just said
not to put the token. On macOS, the first use of an item by a given binary raises
the keychain's own permission dialog, and a `cargo install` upgrade puts the binary
at a new path, so it asks again.

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
`usage`) and the status or code behind it. `schema TARGET TOOL` returns one tool's
input schema, and a tool the server does not have is a `usage` error (exit 2) listing
what it does have: the `tools/list` behind it succeeded and nothing was sent for that
tool. `tools TARGET --json` returns `{"tools": [...]}`, each tool a `name` and the first
line of its `description` until `--long` asks for the schemas, and `tools --json` with
no target returns `{"servers": [...]}`, one probe per saved server, each listing its
tools the same way. `grep PATTERN --json` searches every saved server at once and
returns `{"matches": [...], "skipped": [...]}`, which is how to find a tool without
reading a listing per server; exit 1 means nothing matched. Every `ls --json` row
carries what was saved — `location`, `headers`, `token_env`, `credential`, `source`,
`timeout`, `allow`, `deny` — whether or not the servers were dialed; `--no-probe`
leaves the status fields out rather than putting different ones in their place.
Arguments can come from a file (`@args.json`) or stdin (`-`), or be
`key=value` pairs typed from that schema, so quoting is never a problem. `shell --json`
gives one JSON line per command, errors included, in order.

To see, keep or share what actually went over the wire, pass `--trace FILE` (or set
`MCPDIAL_TRACE=FILE`). Every message sent or received on any transport is appended as
one JSON object per line, `{"t": "2026-09-05T10:11:12.345Z", "dir": "send", "transport":
"http", "target": "wiki", "message": {...}}`, along with transport events (`"dir":
"event"` with `"event": "http"`, `"spawn"`, `"exit"` or `"retry"` and a `"detail"`
object: the status, content type and elapsed milliseconds of each HTTP attempt, or a
stdio server's exit status and last stderr lines). No header is ever written, so a
bearer token never is, and `access_token`, `refresh_token`, `client_secret` and `code`
inside an OAuth exchange are replaced with `"****"`. The file is created owner-only,
since tool output can be private; attach it to a bug report. `-v` is independent and
may be given alongside.

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
cargo build --no-default-features # the agent-only binary: no terminal presentation at all
```

The terminal presentation lives behind the default-on `rich` cargo feature, which is
where any crate it comes to need belongs; `--no-default-features` builds a binary that
prints the piped form everywhere.

`tests/contract.rs` runs every command in the list above, under a pipe and under
`--json`, and compares exit code, stdout and stderr with the files under
`tests/snapshots/`. A change there fails the test, on purpose: what a program or an
agent reads from mcpdial is the contract. When a change to it is intended, regenerate
the files and read the diff before committing it:

```bash
MCPDIAL_UPDATE_SNAPSHOTS=1 cargo test --test contract
```

and say in the pull request that the contract changed and why. A pull request about
how things look at a terminal should never need to.

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
