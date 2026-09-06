# mcpdial for agents

`mcpdial` lets a program talk to any MCP server from a shell. Read this once, then use
`--json` on every command so output is machine-parseable.

## Mental model

- A **target** is a saved server name (`mcpdial ls --json` lists them), an `http(s)://`
  URL, or `stdio:<command line>` for a local process.
- Every invocation is a full session: connect, `initialize`, do one thing, exit. Use
  `shell` when state must survive between calls (a browser, a database cursor, a REPL),
  or `start NAME` to keep a saved stdio server running so that separate invocations
  share it.
- The tool never prompts, except `token set` with no stdin and a TTY. `login` opens a
  browser and waits for a human; do not run it unattended without `--no-browser`.

## Workflow

1. **What is available.** `mcpdial ls --json` returns one object per saved server with
   `status.state` (`connected`, `auth_required`, `token_rejected`, `blocked`, `http`,
   `unreachable`, `error`), `server` (name and version), `tools` (count), and
   `running` (true while a `start` daemon holds the server open).
2. **What a server can do.** `mcpdial tools TARGET --json` returns `{"tools": [...]}`
   with each tool's `name`, `description`, and full `inputSchema`. For one tool,
   `mcpdial schema TARGET TOOL` returns just that object. A saved server may carry
   `allow` and `deny` lists of glob patterns (`mcpdial set NAME --json` shows them);
   `tools` and `ls` then list and count only the permitted tools, and `tools NAME
   --all` adds the hidden ones with `"denied": true`.
3. **Call it.** `mcpdial call TARGET TOOL '{"json":"arguments"}' --json`, or
   `mcpdial call TARGET TOOL key=value ... --json`, prints the
   `tools/call` result: `{"content": [...], "isError": bool}`. Without `--json` the text
   content blocks are printed as plain text, one per line, and a result with `isError`
   set is followed by `(tool reported an error)` on stderr.
4. **Keep state.** `mcpdial shell TARGET --json` reads one command per line from stdin
   and prints one JSON line per command. Send `quit` or close stdin to finish. When
   each command has to be its own invocation, `mcpdial start NAME [--idle SECS]`
   keeps a saved stdio server running in the background and prints
   `started NAME (pid N)` (`--json`: `{"name","pid","socket"}`); every later command
   naming NAME shares that one session until `mcpdial stop NAME`. Requests are
   served one caller at a time, so concurrent invocations queue rather than fail.
   `--no-daemon` on a command, or `MCPDIAL_NO_DAEMON=1`, dials a fresh process
   instead. A daemon that was killed leaves a socket behind; the next command removes
   it with a `note:` line on stderr and dials. Unix only: on Windows `start` exits 2.
5. **Add what is missing.** `mcpdial catalog --json` lists a reviewed set of servers,
   each with an `id`, a `category`, a `transport` and an `auth` (`none`, `oauth`,
   `api-key`, `env`); `mcpdial add NAME --catalog ID` saves one. Beyond the catalog,
   `mcpdial search QUERY --json` prints the MCP registry's entries matching every
   word of the query, ranked (catalog entries first, then an exact name or title,
   the official `io.github.<vendor>` namespace, title, name and description
   matches), as the registry's own objects: the name to add is `server.name`, and
   `server.remotes` and `server.packages` say how it is dialed. It runs over a local
   copy of the registry, fetched on first use and refreshed once a day; `--offline`
   skips the network. Exit 1 means nothing matched. Then
   `mcpdial add NAME --registry <registry name>` saves an entry of the MCP registry
   without running it; the registry name is the entry's own `name`, like
   `io.github.owner/server`. A required value the entry leaves to the user is exit 2
   with a `hint` naming it, and `--arg VALUE` supplies it. Required environment
   variables are saved as `${VAR}` placeholders and named on stderr, or under
   `saved.notes` with `--json`; they must be set before the server is dialed. Every
   command that changes what is saved prints a receipt with `--json`; see below.
6. **Hand a set to a host.** `mcpdial export [NAME...] [--format mcpservers|vscode|codex]`
   writes saved servers back out in the shape a host reads, on stdout: the `mcpServers`
   object Claude Code, Cursor and Windsurf read, VS Code's `servers` with an explicit
   `type`, or Codex's `[mcp_servers.NAME]` tables. With no names every saved server is
   written; a name that is not saved is exit 2 with nothing printed. `--merge FILE`
   prints that host's file with the exported entries replaced or added and everything
   else in it untouched, and never writes the file itself. No credential is exported: a
   saved token stays here and the host logs in for itself, and a `token_env` travels as
   the `${VAR}` placeholder it is, in an `Authorization` header or in Codex's
   `bearer_token_env_var`. Everything left behind is one line on stderr, one per
   server, as `{"note":"..."}` under `--json`.

## Passing arguments

Arguments are one JSON object. Three ways to supply one:

```
mcpdial call fs read_text_file '{"path":"/tmp/x"}'   # inline
mcpdial call fs read_text_file @args.json            # from a file
echo '{"path":"/tmp/x"}' | mcpdial call fs read_text_file -   # from stdin
```

Use a file or stdin for anything large or containing quotes.

Or they are `key=value` pairs, typed by the tool's own `inputSchema`:

```
mcpdial call fs read_text_file path=/tmp/x
mcpdial call ctx7 search query="rust ureq" limit=5 fuzzy=true
mcpdial call srv tool tags:='["a","b"]' meta:='{"k":1}'
mcpdial call srv tool id:='"123"'     # a string where the schema says string|number
```

The first word after the tool name decides: one starting with `{` or `@`, or a bare
`-`, is the JSON object; anything else means every remaining word is a pair. The two
forms cannot be mixed, which is exit 2 with nothing sent.

`key=value` reads the text as `inputSchema.properties[key].type` declares it:
`integer` and `number` parse as a number, `boolean` takes `true` or `false` alone,
`string`, an unknown key and a union type such as `["string","number"]` all stay text,
and `array` and `object` are refused, naming the form below. `key:=json` parses the
value as JSON and ignores the type, which is how arrays, objects and a forced string
are written. A key that is not in `properties` when the schema says
`additionalProperties: false` is exit 2 before the call goes out, with the nearest
property named. A missing required property is not checked here; the server's `-32602`
and its `hint` answer that. Nested keys (`a.b=1`) are not a form; use `:=`.

Pairs cost one extra request, the `tools/list` the schema comes from, unless every
pair uses `:=`. In `shell` that list is cached. `prompt` takes pairs too, where every
value is a string and nothing is fetched.

## Media

A tool that returns an image or audio block (a screenshot, say) hands back base64 that is
no use in a context window. Without `--json` such a block prints as one line,
`[image image/png, 4 KB]`, and the text blocks around it print as they are. Pass
`--save-dir DIR` to any `call`, `prompt` or `read` that can return media, or before
`shell` so every command in it honours it: each block is written to
`DIR/<tool>-<n>.<ext>`, the extension taken from its mime type, and the line names the
file: `[image saved to shots/take_screenshot-1.png, 4 KB]`. With `--json` alone the
result is the server's object, base64 included; with `--json --save-dir DIR` each media
block's `data` (or an embedded resource's `blob`) is replaced by `"path"` and a `"bytes"`
count, so the object is small and the file is where it says.

## Resources and prompts

Tools are one third of MCP. `mcpdial info TARGET --json` reports which of the three a
server implements under `capabilities`.

```
mcpdial resources TARGET --json   {"resources":[...],"resourceTemplates":[...]}
mcpdial read TARGET URI --json    the resources/read result
mcpdial prompts TARGET --json     {"prompts":[...]}
mcpdial prompt TARGET NAME '{"json":"args"}' --json   the prompts/get result
```

A `resourceTemplates` entry carries a `uriTemplate` (RFC 6570) instead of a `uri`; expand
it yourself before calling `read`. A `resources/read` result holds `contents`, each entry
carrying either `text` or a base64 `blob`. Without `--json`, `read` writes text to stdout
byte for byte and a blob as the raw bytes it stands for, and refuses to put bytes on a
terminal: redirect it (`mcpdial read TARGET URI > file`). `prompt` without `--json` prints
one `role: text` line per message and puts the prompt's description on stderr.

A prompt's `arguments` are names and descriptions with no schema behind them: every value
is a string. `mcpdial prompts TARGET --long` lists them.

A server that never implemented one of these answers `-32601`. That error carries a `hint`
naming the missing capability, so a bare method-not-found never has to be decoded.

## Exit codes and errors

| Exit | Meaning |
|---|---|
| 0 | Success. For `call`, the tool did not set `isError`. |
| 1 | The server refused: HTTP error, JSON-RPC error, tool `isError`, or a transport failure. |
| 2 | Usage or config problem. Nothing was sent. |

With `--json`, errors are one JSON object on **stderr**:

```json
{"error":{"kind":"rpc","message":"MCP error -32602: Tool nope not found","code":-32602}}
{"error":{"kind":"http","message":"HTTP 401 ...","status":401,"www_authenticate":"Bearer realm=..."}}
{"error":{"kind":"transport","message":"server exited with status 1 before replying. ..."}}
```

`kind` is one of `rpc`, `http`, `transport`, `auth`, `config`, `usage`. In `shell --json`
mode the same object is printed on **stdout** in sequence with results, so ordering is
preserved.

A `call` or `schema` of a tool the saved server's allow or deny list hides is exit 2
with nothing sent, a `config` error naming the tool:

```json
{"error":{"kind":"config","message":"delete_file is denied for fs by its deny list; edit with mcpdial set fs","tool":"delete_file"}}
```

Do not work around it with `raw tools/call`, which the lists never filter: the person
who saved the server hid that tool on purpose.

When the arguments were the mistake, either unparseable or rejected by the server with
`-32602`, the error carries a `hint` string holding the tool's usage line in both
argument forms and one line per parameter, so a retry needs no extra `schema` call:

```
{"error":{"kind":"rpc","code":-32602,"message":"... Required at url",
          "hint":"usage: call new_page {\"url\": \"<string>\"}\n   or: call new_page url=<string>\n  url: string (required) - ..."}}
```

If the tool name itself is unknown, `hint` names the nearest one instead. Some servers
report a schema violation, or an unknown tool name, as a *result* with `isError` rather
than as a JSON-RPC error; that case prints the same usage block or suggestion on
**stderr**, as `{"hint":"usage: ..."}` with `--json`, leaving the result object on stdout
untouched.

With `--json`, every line on either stream is a JSON object.

## Receipts

Commands that change what is saved report on stderr for a human, and with `--json` print
one object on stdout instead, so nothing has to be confirmed by parsing a sentence:

```
mcpdial add NAME ... --json      {"saved":{"name":"x","kind":"http","location":"https://u/mcp",...}}
mcpdial set NAME ... --json      {"saved":{"name":"x","kind":"http","location":"https://u/mcp","allow":[...],"deny":[...]}}
mcpdial rm NAME --json           {"removed":"x"}
mcpdial import FILE --json       {"imported":["a","b"],"skipped":["c"],"notes":{"a":["..."]}}
mcpdial login TARGET --json      {"login":{"name":"x","expires_at":1760000000,"refreshable":true,"registration":"dynamic"}}
mcpdial logout TARGET --json     {"removed_credential":"x"}     null when none was saved
mcpdial token set NAME --json    {"saved_credential":"x"}
mcpdial token rm NAME --json     {"removed_credential":"x"}     null when none was saved
```

`add` dials the server it just saved and reports the row `ls --json` would, so `saved`
also carries `status`, `auth`, `server` and `tools`; `--no-probe` skips the dial and
leaves `name`, `kind` and `location` alone, as does `--registry`, which never runs what
it saves. The exit code is 0 whenever the save succeeded, whatever `status` says: read
it. `add` refuses a name that is already saved unless `--force` is passed, and rejects
an `--http` value that is not an `http(s)://` URL or a `--stdio` command line with no
words in it; each is a usage error (exit 2) with nothing written. `add --allow GLOB`
and `--deny GLOB` (repeatable) save tool allow and deny lists, and `saved` carries each
list that is not empty. `set NAME --allow ... --deny ...` replaces a list, `--clear-allow`
and `--clear-deny` empty one, and `set NAME` with no flags prints
`{"name":"x","allow":[...],"deny":[...]}` instead of a receipt. `token show` with no
saved credential is a config error (exit 2). `login` prints its progress, including the
authorization URL, as plain lines on stderr in either mode.

## Shell protocol

Input, one command per line:

```
call TOOL {"json":"args"}     # args optional, default {}
call TOOL key=value ...       # the same, typed by the tool's schema
tools                         # list tools
schema TOOL                   # one tool's inputSchema
resources                     # resources, then resource templates
read URI                      # one resource's contents
prompts                       # list prompts
prompt NAME {"json":"args"}   # expand a prompt into its messages
raw METHOD {"json":"params"}  # any JSON-RPC method; allow and deny lists do not apply
info                          # the initialize result
help [TOOL]                   # commands, or one tool's parameters
quit
```

Lines starting with `#` are ignored. Output with `--json`: one line per command. `call`
prints the result object; `tools` prints `{"tools":[...]}`; `schema` prints the tool
object; `resources` prints `{"resources":[...],"resourceTemplates":[...]}`; `prompts`
prints `{"prompts":[...]}`; `read`, `prompt` and `info` print their results untouched;
errors print `{"error":{...}}`. A usage hint under a failed `call` result goes to stderr
as `{"hint":"..."}`, so stdout stays one line per command. The process exits 1 at the
end if any command failed and stdin was not a terminal.

A bare tool name is not a command; `call` it. Nothing else in the line is guessed at.
Line editing, history and Tab completion apply only when stdin and stdout are both a
terminal; piped input is read one line at a time with no editing and no history.

## Serving a saved server to something that must not hold its token

`mcpdial serve NAME` re-exposes one saved server as a plain MCP endpoint, dialing
upstream with the saved credential and never handing it on. It runs until `^C`
(exit 0), so start it in the background and read where it landed:

```
mcpdial serve NAME [--listen ADDR] [--bearer-env VAR] [--allow PAT]... [--deny PAT]...
mcpdial serve NAME --stdio
```

`--listen` defaults to `127.0.0.1:0`; a non-loopback address is a usage error
(exit 2) unless `--listen-any` is passed. With `--json` the first line on stdout is
the receipt, before anything is served:

```json
{"serve":{"name":"work","url":"http://127.0.0.1:54321/mcp","address":"127.0.0.1:54321",
          "bearer_env":"SANDBOX_TOKEN","allow":[],"deny":["delete_*"]}}
```

Point a client at `serve.url` like any other HTTP target, with `--token-env` holding
the same value as `--bearer-env`; a wrong or missing token is a 401 and nothing
reaches the upstream server. `--allow` / `--deny` are globs (`*`, `?`) over tool
names, deny first: a denied tool is absent from `tools/list` and a call to it is
`-32602`; the allow and deny lists saved with the server apply here as well.
`initialize` is answered by the proxy, so `serverInfo.name` is `mcpdial`
rather than the upstream's name, and the capabilities are the upstream's minus the
ones that need the server to speak first (`logging`, `listChanged`, `subscribe`).

`--stdio` speaks the same protocol on stdin and stdout instead, which makes
`stdio:mcpdial serve NAME --stdio` a target like any other. Nothing but protocol
goes to stdout in that mode, so there is no receipt.

## Authentication

- `ls --json` says `auth_required` when a server wants a token and none is saved.
- `mcpdial login TARGET --no-browser` prints an authorization URL and waits up to five
  minutes for the redirect. Relay the URL to a human; do not try to complete it yourself.
- `login` identifies the client with `--client-id` if given, else with a client ID
  metadata document when the server advertises support, else by dynamic registration.
  `--no-client-metadata` forces the last; `token show --json` reports `registration`.
- When a client id and secret are available (a confidential client registered out of
  band), `mcpdial login TARGET --grant client-credentials --client-id ID
  --client-secret-env VAR` needs no human: no browser, no redirect, and the token is
  renewed by running the grant again.
- If a token already exists in the environment: `mcpdial --token-env VAR ...` or
  `mcpdial token set NAME --env VAR`. Never put a token on the command line.
- Saved tokens refresh automatically. A `token_rejected` status after that means the
  human needs to `login` again.

## Limits

- `--timeout SECS` (default 60) bounds every wait. stdio servers that install packages
  on first run (`npx -y ...`) can need more. A saved server can carry its own timeout
  (`add --timeout SECS`; `ls --no-probe --json` reports it under `timeout`), used when
  the flag is not given; the flag beats it. The status probe behind `ls`, and behind
  `add` on save, waits 10 seconds when neither says: a server that takes longer to
  answer `initialize` is reported as `unreachable`.
- A stdio server that exits before replying reports its exit status and last stderr
  lines in the error message. A wrong package name shows up there as an npm 404.
- Servers are called at their final URL. A redirect is reported, not followed.
- A session opens with `server/discover`, all that revision `2026-07-28` has; a server
  that has never heard of it gets the `initialize` handshake instead, offering
  `2025-11-25`. `info --json` reports the version in use under `protocolVersion` and
  the server's identity under `serverInfo` either way; on `2026-07-28` the discovery
  result's own `supportedVersions`, `resultType` and `_meta` are there beside them. A
  `transport` error naming a version mcpdial does not speak means the server wants one
  it was not offered; retry with `--protocol-version 2025-06-18` (or `2025-03-26`), and
  save it with `add`. `--protocol-version` also pins `2026-07-28`, or `2025-11-25` to
  hold a server that serves both eras to the handshake.
- A transient HTTP failure is retried once, only where that is provably safe: the
  request is idempotent (`server/discover`, `initialize`, a `*/list`, `ping`, or a
  tool whose listing carries `idempotentHint`), or it failed before the server could
  have processed it (connection refused or reset with no reply, DNS failure, 429, or
  502/503/504 before a session was issued). A timeout, any other 4xx, a JSON-RPC error
  and a stdio server that dies are never retried. `--no-retry` reports the first
  failure.
- `mcpdial guide` prints this document.
