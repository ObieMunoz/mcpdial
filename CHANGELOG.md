# Changelog

All notable changes to this project are documented in this file. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Each tagged release
carries its section from here as its release notes. Sections are generated from
commit subjects with `git cliff`; nothing here is written by hand.

## [Unreleased]

Every pull request title is a conventional commit subject, and the next section is
built from those at release time.


## [0.2.2] - 2026-09-07

### Fixed

- Never dial from a bare mcpdial (#169)


## [0.2.1] - 2026-09-06

### Fixed

- Shorten the model-context-protocol keyword so crates.io accepts it

## [0.2.0] - 2026-09-06

### Added

- Add servers from the MCP registry
- Expand ${VAR} placeholders when a server is dialed
- Record where a server came from in servers.json
- Offer protocol 2025-11-25 and keep the version the server agrees to
- Client ID metadata documents ahead of dynamic registration
- A curated server catalog, embedded and refreshed from main (#102)
- Login --grant client-credentials for unattended access (#101)
- Per-server timeout saved by add, honoured when dialing, read by import (#106)
- Import VS Code, Codex and OpenCode server configs
- Ranked registry search over a local copy of the whole list
- Keep a stdio server alive across invocations with start and stop (#99)
- Retry once on a transient HTTP failure where it is provably safe (#97)
- Per-server tool allow and deny lists (#108)
- Key=value arguments for call and prompt, typed by the tool's schema
- Speak revision 2026-07-28, detecting it with server/discover
- Serve a saved server without handing over its credentials
- Export saved servers in a host's own shape, without their credentials (#115)
- --trace FILE writes the wire exchange as JSON Lines, secrets redacted (#116)
- A presenter split with --plain, a rich feature and agent-contract snapshots (#117)
- MCPDIAL_JSON, MCPDIAL_TIMEOUT and MCPDIAL_USER_AGENT as environment defaults (#124)
- Highlight JSON at a terminal, honouring NO_COLOR (#120)
- Page long output at a terminal, git-style (#123)
- **Breaking:** Cut --json listings to names and first lines, schemas under --long (#125)
- Surface progress and server log notifications during a call (#131)
- --max-chars and --output to keep a large result out of a context window (#129)
- Tag a tool listing with its annotations, title and task support (#133)
- Browse, a checklist of catalog servers to save and dial (#130)
- Read the error codes 2026-07-28 renumbered, in both numberings (#132)
- Answer elicitation/create instead of refusing it (#118)
- Colour the ls status column and the error prefix at a terminal, honouring NO_COLOR (#121)
- Tab-complete a call's JSON keys and enum values in the shell (#135)
- Check the authorization response's iss and key credentials by issuer (#138)
- Draw a PNG inline on iTerm2, kitty, WezTerm and Ghostty (#141)
- Pin a server's tools to a file, and say what moved (#144)
- Grep tools, resources, prompts and instructions across saved servers (#145)
- Render markdown tool results at a terminal (#142)
- Prompt for a missing required argument from the tool's schema at a terminal (#146)
- Pick a server and a tool at a terminal, then print the call it made (#147)
- Answer a 2026-07-28 demand for input by sending the call again (#149)
- Keep credentials in the OS keychain when asked (#148)
- Number the shell's results, and show, save, retry or edit one (#150)
- Let the server run a call in the background and come back for it (#152)
- Complete a prompt argument and a template variable from the server (#153)
- Follow what a server changes from a shell session (#151)
- Filter a shell result with a path expression, or jq (#154)
- Show a shell session's state in its prompt and one line at a time (#156)

### Fixed

- Check and confirm what add, call and the other mutations do
- File or placeholder media blocks instead of printing their base64 (#98)
- Give a status probe ten seconds, not the call timeout
- Suggest a tool name after any failed call, and count a transposition as one edit
- Export the VS Code reference syntax VS Code actually expands
- **Breaking:** Reconcile the --json shapes of tools, ls and a missing schema (#128)
- Parameter summaries that name a union, an item type and an object's fields (#127)
- **Breaking:** Logout refuses an unknown name, AUTH names the variable, exit joins quit (#137)
- Open a call's arguments object from Tab, not by hand (#139)
- Get the updating line out of the way of a server's question (#140)
- Erase the rest of the line when a progress report gets shorter (#143)
- Search a picker by name, and say what the picker is for (#162)

### Changed

- Send export's output through the presenter like every other command (#122)
- Remember a server's protocol era so an older server costs no extra POST (#157)
- Ask the presenter about the terminal, not the streams (#160)

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

[Unreleased]: https://github.com/ObieMunoz/mcpdial/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ObieMunoz/mcpdial/releases/tag/v0.2.0
[0.1.0]: https://github.com/ObieMunoz/mcpdial/releases/tag/v0.1.0
