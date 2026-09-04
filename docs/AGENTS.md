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
   content blocks are printed as plain text, one per line.
4. **Keep state.** `mcpdial shell TARGET --json` reads one command per line from stdin
   and prints one JSON line per command. Send `quit` or close stdin to finish.

## Passing arguments

Arguments must be a JSON object. Three ways to supply one:

```
mcpdial call fs read_text_file '{"path":"/tmp/x"}'   # inline
mcpdial call fs read_text_file @args.json            # from a file
echo '{"path":"/tmp/x"}' | mcpdial call fs read_text_file -   # from stdin
```

Use a file or stdin for anything large or containing quotes.

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
**stderr**, leaving the result object on stdout untouched.

## Shell protocol

Input, one command per line:

```
call TOOL {"json":"args"}     # args optional, default {}
tools                         # list tools
schema TOOL                   # one tool's inputSchema
raw METHOD {"json":"params"}  # any JSON-RPC method
info                          # the initialize result
help [TOOL]                   # commands, or one tool's parameters
quit
```

Lines starting with `#` are ignored. Output with `--json`: one line per command. `call`
prints the result object; `tools` prints `{"tools":[...]}`; `schema` prints the tool
object; errors print `{"error":{...}}`. The process exits 1 at the end if any command
failed and stdin was not a terminal.

A bare tool name is not a command; `call` it. Nothing else in the line is guessed at.

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
- `mcpdial guide` prints this document.
