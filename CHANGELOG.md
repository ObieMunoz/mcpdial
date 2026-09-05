# Changelog

All notable changes to this project are documented in this file. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Each tagged release
carries its section from here as its release notes.

## [Unreleased]

### Added

- `add NAME --registry <name>` saves an entry of the MCP registry without running it:
  an HTTP remote as is, an npm, PyPI or OCI package as the `npx`, `uvx` or
  `docker run` line it needs, required environment variables as `${VAR}`
  placeholders, and values the entry leaves to the user from `--arg`.
  `MCPDIAL_REGISTRY` points at a private registry.
- `${VAR}` and `${VAR:-default}` in a saved server's headers, env, cwd, URL and
  command line are read from the environment when the server is dialed, so
  `servers.json` can name a secret without holding it. `$$` is a literal `$`.
  A variable that is unset, with no default, is exit 2 before anything is sent.

## [0.1.0] - 2026-09-05

### Added

- Saved servers: `add`, `rm`, and `ls` with live connection status, plus `import` from
  Claude Code, Claude Desktop, Cursor, and Windsurf configurations.
- Streamable HTTP and stdio transports, including SSE-framed replies, server-initiated
  requests on stdio, session termination on close, and a clear report when the older
  HTTP+SSE transport is found instead.
- `tools`, `info`, `call`, `schema`, `resources`, `read`, `prompts`, `prompt`, and
  `raw`, with arguments from the command line, a file, or stdin.
- `shell`: one session for many commands, with line editing, history, and tab
  completion at a terminal.
- OAuth 2.1 `login` with discovery, dynamic registration, PKCE, a loopback redirect,
  automatic refresh, and confidential clients registered out of band; `logout`; and
  `token set`, `token show`, and `token rm` for tokens obtained elsewhere.
- `--json` on every command, with structured errors for programs and agents, and the
  agent guide embedded in the binary as `mcpdial guide`.
- Shell completions for bash, zsh, fish, elvish, and PowerShell.
- Credentials readable only by their owner, on unix and on Windows.
- Remembered probe results, so a routine `ls` dials nothing.
- Prebuilt binaries for macOS, Linux, and Windows attached to every tagged release.

[Unreleased]: https://github.com/ObieMunoz/mcpdial/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ObieMunoz/mcpdial/releases/tag/v0.1.0
