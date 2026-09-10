# diffier

[![CI](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml/badge.svg)](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A live diff feed for Claude Code. Run it in a tmux split next to a Claude Code
session and every Edit, Write, MultiEdit, and NotebookEdit shows up as a full
diff the moment it lands, rendered through [delta](https://github.com/dandavison/delta)
when it is installed.

Claude Code collapses edit results in its transcript and that rendering is not
configurable. This tool works around it from the outside: Claude Code hooks
snapshot each file before an edit and append the hook payload to a JSONL spool;
the TUI tails the spool and diffs the snapshot against the file on disk.

## Install

Requirements: `jq`, and optionally `delta`. Building from source also needs a
Rust toolchain (1.88 or later).

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 gnu and
musl, aarch64 gnu) are attached to each
[release](https://github.com/spencerjireh/diffier/releases):

```sh
tar -xzf diffier-<version>-<target>.tar.gz
install -m 755 diffier-<version>-<target>/diffier ~/.local/bin/
diffier install
```

Or build it yourself:

```sh
cargo install --git https://github.com/spencerjireh/diffier
diffier install
```

`install` writes `~/.claude/hooks/diffier.sh` and registers it under
`hooks.PreToolUse`, `hooks.PostToolUse` (matcher `^(Edit|Write|MultiEdit|NotebookEdit)$`),
and `hooks.SessionStart` in `~/.claude/settings.json`. The command is registered
as a single-quoted path, so a home directory with a space still works. Everything else in the file
is preserved, a one-time `settings.json.bak` is written, and running `install`
again is a no-op. Restart any running Claude Code session so the hooks load.

## Use

```sh
cd ~/Projects/some-repo
tmux split-window -h diffier    # or run it in any second pane
claude                                  # in the other pane
```

The monitor shows only events from Claude Code sessions whose working directory
matches the directory it was started in. On startup it replays the most recent
session's edits, plus those of any other session still active in that directory,
then follows new ones.

Keys:

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, wheel | scroll |
| `Ctrl-d` / `Ctrl-u`, PgDn / PgUp | half page / page |
| `g` / `G` | top / bottom (and resume follow) |
| `p` | toggle follow |
| `q`, Esc, `Ctrl-c` | quit |

Scrolling up pauses follow. Mouse capture is enabled for the wheel, so use
tmux copy mode or shift-drag to select text.

Other commands:

```sh
diffier dump            # print the current session's cards and exit
diffier dump --ansi     # same, with delta's colored output
diffier run --no-delta  # force the built-in plain renderer
diffier uninstall       # remove the hook and its settings entries
diffier uninstall --purge   # also delete the spool and snapshots
```

## How it works

- `PreToolUse` copies the target file to `~/.cache/diffier/<session>/<tool_use_id>`
  (or writes an `.absent` marker when the file does not exist yet). `PostToolUse`
  copies it again to `<tool_use_id>.after`, so an edit that lands before the
  monitor gets to the event does not leak into the earlier card. Files over 5 MB
  are not copied.
- Every hook event is appended to `~/.local/state/diffier/events.jsonl`
  with the large `tool_response.content`, `structuredPatch`, and
  `tool_input.content` fields dropped, plus `originalFile` whenever a snapshot
  made it redundant. Appends are serialized with a lock directory so that
  concurrent subagent edits cannot interleave into a corrupt line.
- The monitor diffs the pre-edit snapshot against the post-edit one, falling back
  to the file on disk. When no pre-edit snapshot exists it uses
  `tool_response.originalFile`, then the edit's `old_string`/`new_string`.
- The spool rotates to `events.jsonl.1` at 50 MB, on whichever event crosses the
  threshold; replay reads both files. `SessionStart` prunes snapshot directories
  older than two days. A `compact` SessionStart does not reset replay history.
- Diffs are cut at 500 lines with a count of the remainder. Binary files and
  files missing after the edit show a one-line notice.
- Subagent edits appear in the same feed, labeled with the agent type.

The hook script always exits 0, including when `HOME` and the XDG variables are
all unset. A non-zero exit from a PreToolUse hook would block Claude's tool call.
Payload fields are constrained to strings before they reach the shell, so a
non-string `file_path` cannot become a command.

## Development

```sh
cargo test                      # unit tests plus the fixture replay test
INSTA_UPDATE=always cargo test  # refresh the replay snapshot after intended changes
```

Fixtures live in `tests/fixtures`: a spool with `{{CWD}}` placeholders, the
pre-edit snapshots, and the post-edit file tree.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full check list that CI runs and
the rules that apply when changing `hook/diffier.sh`.

## License

MIT. See [LICENSE](LICENSE).
