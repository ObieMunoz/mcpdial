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
mcpdial tools TARGET --json             # every tool's name and one-line description
mcpdial grep PATTERN --json             # find one across every saved server at once
mcpdial schema TARGET TOOL              # one tool's schema, in full
mcpdial call TARGET TOOL '{"k":"v"}' --json
mcpdial call TARGET TOOL @args.json --json      # large arguments from a file
mcpdial call TARGET TOOL k=v n=5 --json         # pairs typed from the tool's schema
mcpdial shell TARGET --json             # one session; one command per stdin line
mcpdial start NAME [--idle SECS]        # keep a stdio server running; calls share it
mcpdial stop NAME
```

`TARGET` is a saved name, an `http(s)://` URL, or `stdio:<command>`.

Rules:

- Always pass `--json`, or export `MCPDIAL_JSON=1` once so every command carries it.
  Results go to stdout; errors go to stderr as `{"error":{...}}`.
- Exit 0 success, 1 the server refused or the tool set `isError`, 2 bad usage.
- Before calling a tool you have not called before: `tools --json` to pick one, then
  `schema TARGET TOOL` for its `inputSchema`. `tools --long --json` returns every
  schema at once and is rarely worth the context. A rejected call answers with
  `error.hint`: the tool's usage line and its parameters.
- With several servers saved, `grep PATTERN --json` beats a listing per server: it
  searches tools, resources, prompts and instructions on all of them at once and
  answers `{"matches":[...],"skipped":[...]}`, each match naming the `server`, `kind`
  and `name` to pass to `schema` next. Exit 1 means nothing matched; a server that
  could not be dialed is under `skipped` rather than an error.
- Use `shell` for servers whose state matters across calls, such as a browser: each
  plain `call` is a fresh process. When every command must be its own invocation,
  `start NAME` keeps a saved stdio server running and later `call`s share it (Unix
  only); `stop NAME` when done, or start with `--idle 300` so it ends itself.
- Never run `login` without `--no-browser`; relay the printed URL to the user. Never
  put a token on the command line; use `--token-env VAR`.
- If a saved server is not there, add it: `mcpdial add NAME --http URL` or
  `mcpdial add NAME --stdio "cmd args"`, or `mcpdial import` to pull from host configs.
  `mcpdial search QUERY --json` finds a server in the MCP registry, ranked, and
  `mcpdial add NAME --registry io.github.owner/server` saves the entry by its
  `server.name`; exit 2 with a hint means it needs `--arg VALUE`, and required
  `${VAR}` placeholders must be set before dialing.
