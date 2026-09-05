# mcpdial for agents

`mcpdial` lets a program talk to any MCP server from a shell. Read this once, then use
`--json` on every command so output is machine-parseable.

## Mental model

- A **target** is a saved server name (`mcpdial ls --json` lists them), an `http(s)://`
  URL, or `stdio:<command line>` for a local process.
- Every invocation is a full session: connect, `initialize`, do one thing, exit. Use
  `shell` when state must survive between calls (a browser, a database cursor, a REPL).
- The tool never prompts, except `token set` with no stdin and a TTY. `login` opens a
  browser and waits for a human; do not run it unattended without `--no-browser`.

## Workflow

1. **What is available.** `mcpdial ls --json` returns one object per saved server with
   `status.state` (`connected`, `auth_required`, `token_rejected`, `blocked`, `http`,
   `unreachable`, `error`), `server` (name and version), and `tools` (count).
2. **What a server can do.** `mcpdial tools TARGET --json` returns `{"tools": [...]}`
   with each tool's `name`, `description`, and full `inputSchema`. For one tool,
   `mcpdial schema TARGET TOOL` returns just that object.
3. **Call it.** `mcpdial call TARGET TOOL '{"json":"arguments"}' --json` prints the
   `tools/call` result: `{"content": [...], "isError": bool}`. Without `--json` the text
   content blocks are printed as plain text, one per line, and a result with `isError`
   set is followed by `(tool reported an error)` on stderr.
4. **Keep state.** `mcpdial shell TARGET --json` reads one command per line from stdin
   and prints one JSON line per command. Send `quit` or close stdin to finish.
5. **Add what is missing.** `mcpdial add NAME --registry <registry name>` saves an
   entry of the MCP registry without running it; the registry name is the entry's
   own `name`, like `io.github.owner/server`, and must be known already. A required
   value the entry leaves to the user is exit 2 with a `hint` naming it, and
   `--arg VALUE` supplies it. Required environment variables are saved as `${VAR}`
   placeholders and named on stderr, or under `saved.notes` with `--json`; they must be
   set before the server is dialed. Every command that changes what is saved prints a
   receipt with `--json`; see below.

## Passing arguments

Arguments must be a JSON object. Three ways to supply one:

```
mcpdial call fs read_text_file '{"path":"/tmp/x"}'   # inline
mcpdial call fs read_text_file @args.json            # from a file
echo '{"path":"/tmp/x"}' | mcpdial call fs read_text_file -   # from stdin
```

Use a file or stdin for anything large or containing quotes.

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

When the arguments were the mistake, either unparseable or rejected by the server with
`-32602`, the error carries a `hint` string holding the tool's usage line and one line
per parameter, so a retry needs no extra `schema` call:

```
{"error":{"kind":"rpc","code":-32602,"message":"... Required at url",
          "hint":"usage: call new_page {\"url\": \"<string>\"}\n  url: string (required) - ..."}}
```

If the tool name itself is unknown, `hint` names the nearest one instead. Some servers
report a schema violation as a *result* with `isError` and the `-32602` text in its
content rather than as a JSON-RPC error; that case prints the same usage block on
**stderr**, as `{"hint":"usage: ..."}` with `--json`, leaving the result object on stdout
untouched.

With `--json`, every line on either stream is a JSON object.

## Receipts

Commands that change what is saved report on stderr for a human, and with `--json` print
one object on stdout instead, so nothing has to be confirmed by parsing a sentence:

```
mcpdial add NAME ... --json      {"saved":{"name":"x","kind":"http","location":"https://u/mcp",...}}
mcpdial rm NAME --json           {"removed":"x"}
mcpdial import FILE --json       {"imported":["a","b"],"skipped":["c"]}
mcpdial login TARGET --json      {"login":{"name":"x","expires_at":1760000000,"refreshable":true}}
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
words in it; each is a usage error (exit 2) with nothing written. `token show` with no
saved credential is a config error (exit 2). `login` prints its progress, including the
authorization URL, as plain lines on stderr in either mode.

## Shell protocol

Input, one command per line:

```
call TOOL {"json":"args"}     # args optional, default {}
tools                         # list tools
schema TOOL                   # one tool's inputSchema
resources                     # resources, then resource templates
read URI                      # one resource's contents
prompts                       # list prompts
prompt NAME {"json":"args"}   # expand a prompt into its messages
raw METHOD {"json":"params"}  # any JSON-RPC method
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

## Authentication

- `ls --json` says `auth_required` when a server wants a token and none is saved.
- `mcpdial login TARGET --no-browser` prints an authorization URL and waits up to five
  minutes for the redirect. Relay the URL to a human; do not try to complete it yourself.
- If a token already exists in the environment: `mcpdial --token-env VAR ...` or
  `mcpdial token set NAME --env VAR`. Never put a token on the command line.
- Saved tokens refresh automatically. A `token_rejected` status after that means the
  human needs to `login` again.

## Limits

- `--timeout SECS` (default 60) bounds every wait. stdio servers that install packages
  on first run (`npx -y ...`) can need more.
- A stdio server that exits before replying reports its exit status and last stderr
  lines in the error message. A wrong package name shows up there as an npm 404.
- Servers are called at their final URL. A redirect is reported, not followed.
- `initialize` offers protocol `2025-11-25`; `info --json` reports the version the
  server agreed to under `protocolVersion`. A `transport` error naming a version
  mcpdial does not speak means the server wants one it was not offered; retry with
  `--protocol-version 2025-06-18` (or `2025-03-26`), and save it with `add`.
- `mcpdial guide` prints this document.
