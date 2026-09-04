---
name: mcpdial
description: Call any MCP server's tools from the shell with mcpdial - list saved servers and their status, read tool schemas, call tools with JSON arguments, and keep a stateful session open. Use when a task needs an MCP server (Chrome DevTools, a filesystem server, a remote HTTP MCP service) and no MCP connector is wired into the host.
---

# mcpdial

`mcpdial` is a CLI that speaks the MCP protocol directly over HTTP or stdio. It replaces
a host connector: anything you would do through an MCP tool, you can do with a shell
command.

Run `mcpdial guide` for the full agent reference. The short version:

```bash
mcpdial ls --json                       # saved servers with live status and tool counts
mcpdial tools TARGET --json             # every tool with its inputSchema
mcpdial schema TARGET TOOL              # one tool's schema
mcpdial call TARGET TOOL '{"k":"v"}' --json
mcpdial call TARGET TOOL @args.json --json      # large arguments from a file
mcpdial shell TARGET --json             # one session; one command per stdin line
```

`TARGET` is a saved name, an `http(s)://` URL, or `stdio:<command>`.

Rules:

- Always pass `--json`. Results go to stdout; errors go to stderr as `{"error":{...}}`.
- Exit 0 success, 1 the server refused or the tool set `isError`, 2 bad usage.
- Read `tools --json` or `schema` before calling a tool you have not called before.
- Use `shell` for servers whose state matters across calls, such as a browser: each
  plain `call` is a fresh process.
- Never run `login` without `--no-browser`; relay the printed URL to the user. Never
  put a token on the command line; use `--token-env VAR`.
- If a saved server is not there, add it: `mcpdial add NAME --http URL` or
  `mcpdial add NAME --stdio "cmd args"`, or `mcpdial import` to pull from host configs.
