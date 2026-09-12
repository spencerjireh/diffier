# Contributing

## Setup

Requirements: a Rust toolchain (1.89 or later, edition 2024) and optionally
[delta](https://github.com/dandavison/delta).

```sh
git clone https://github.com/spencerjireh/diffier
cd diffier
cargo test
```

## Before you open a pull request

Run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --locked
```

CI also runs `cargo deny check` and builds the docs with warnings denied. All of
it must pass.

## Branches and merging

`main` accepts squash merges from pull requests only, and its history is linear.
Cut a branch from `main` named `<type>/<slug>` with a Conventional Commits type
(`feat/session-filter`, `fix/spool-race`). Give the pull request a Conventional
Commits title: it becomes the commit subject on `main`, and the PR body becomes
the commit body. CI must pass before the merge button is enabled, and the branch
is deleted on merge.

## Tests

There are three layers:

- Unit tests in `#[cfg(test)] mod tests` next to the code they cover.
- `tests/replay.rs` — a golden test. It copies `tests/fixtures/tree` into a
  temporary directory, substitutes `{{CWD}}` in `tests/fixtures/spool.jsonl`,
  and asserts the rendered cards against an `insta` snapshot.
- `tests/hook.rs` — behavioral tests of `diffier hook` run as a child process
  the way Claude Code runs it: exit code, spool contents, snapshot files, and
  concurrent appends.

After an intended change to rendering, refresh the snapshot and read the diff
before you commit it:

```sh
INSTA_UPDATE=always cargo test
git diff tests/snapshots/
```

## Changing the hook subcommand

`src/hook.rs` is what Claude Code runs on every edit. Three rules:

- **It must never exit non-zero.** Exit code 2 on `PreToolUse` blocks Claude's
  tool call, and any other non-zero code puts a notice in the user's session.
  `hook::run` returns `()`, swallows every error, and `main` wraps it in
  `catch_unwind` before calling `process::exit(0)`.
- **It must never write to stdout.** Claude Code parses hook stdout as JSON
  on some events. Diagnostics go to stderr or nowhere.
- **Payload fields are read as JSON strings only.** `extract_fields` treats an
  array- or object-valued `file_path` as absent rather than as a path.

`tests/hook.rs` covers all three; keep them passing.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org): `feat:`,
`fix:`, `docs:`, `test:`, `refactor:`, `chore:`. Keep the subject under 72
characters.

## Releases

Maintainers tag `vX.Y.Z` on `main`. That triggers `.github/workflows/release.yml`,
which cross-builds five targets and attaches the tarballs and checksums to a
GitHub Release. Bump `version` in `Cargo.toml` in the same commit as the tag.
