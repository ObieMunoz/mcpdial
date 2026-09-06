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

Run the **release** workflow from the Actions tab. Leave the version blank and
[git-cliff](https://git-cliff.org) reads the conventional commit subjects since the
last tag and decides whether they add up to a patch, a minor or a major; fill it in
to overrule that. Nothing else is asked of you, and nothing is edited by hand.

The workflow then bumps `Cargo.toml`, rewrites this package's line in `Cargo.lock`,
prepends the new `CHANGELOG.md` section and opens a `chore: release X.Y.Z` pull
request. Merging that pull request is what releases: the same workflow tags the merge
commit, builds every target in the README's platform table, checks each binary
reports the tagged version, publishes a GitHub Release whose notes are that changelog
section with the archives and their `SHA256SUMS` attached, and finally publishes the
crate to crates.io.

The bump goes through a pull request rather than straight to `main` because the "main
requires ci" ruleset has no bypass actors — the release commit is tested like every
other commit. What marks a release is simply `main` naming a version that has no tag
yet, so the tag is derived rather than remembered: a re-run cannot tag twice, and a
merge that changed no version does nothing. Pushing a `vX.Y.Z` tag by hand still
works and still has to agree with `Cargo.toml`.

crates.io is last because it is the only step that cannot be taken back: a version
there can be yanked, but the number is never free again, so everything that might
fail runs before it. A pull request touching `Cargo.toml`, `Cargo.lock` or the
workflow builds the packaged tarball too, which is how a file the package leaves out
is caught before a tag exists — though a dry run never reaches the registry, so it
cannot tell you whether crates.io will accept the upload.

#### `RELEASE_TOKEN`

With a `RELEASE_TOKEN` secret — a fine-grained PAT for this repository with contents
and pull-requests write — the release is that one click and nothing more: the release
pull request auto-merges when CI goes green and ships itself.

Without one it still works, but two steps are yours. GitHub raises no workflow run
for anything `GITHUB_TOKEN` did, so the release pull request opens with its checks
held for "Approve workflows to run", and the merge has to be performed by a person —
an auto-merge on `GITHUB_TOKEN` would land the release commit and then quietly stop,
leaving a bumped `main` with no tag. The workflow knows the difference and only
offers auto-merge when there is a real token behind it.

The workflow holds no crates.io token either. It authenticates by [Trusted
Publishing](https://crates.io/docs/trusted-publishing), trading the run's GitHub OIDC
token for one scoped to that run and revoked when the job ends, which is why the
`publish` job asks for `id-token: write`. The trust is configured once on crates.io,
under mcpdial -> Settings -> Trusted Publishing, naming this repository and
`release.yml`. crates.io only accepts that configuration for a crate that already
exists, so the first version was published by hand from a maintainer's machine; every
version since has come from a tag.

