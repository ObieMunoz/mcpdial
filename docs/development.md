# Developing mcpdial

```bash
cargo test            # unit tests plus end-to-end tests against a fake MCP + OAuth server
cargo clippy --all-targets
cargo run --example echo_server   # the smallest real stdio MCP server, used by the tests
cargo build --no-default-features # the agent-only binary: no terminal presentation at all
```

The terminal presentation lives behind the default-on `rich` cargo feature, which is
where any crate it comes to need belongs; `--no-default-features` builds a binary that
prints the piped form everywhere.

`tests/contract.rs` runs every command mcpdial has, under a pipe and under
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
target in the README's platform table, checks that each binary reports the tagged
version, and publishes a GitHub Release whose notes are that changelog section, with
the archives and their `SHA256SUMS` attached. Nothing is published to crates.io.

