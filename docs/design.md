# How diffier works

Claude Code collapses edit results in its transcript and that rendering is not
configurable. diffier works around it from the outside: hooks snapshot each file
around an edit and append the hook payload to a JSONL spool, and the TUI tails
that spool and diffs the snapshots.

## Snapshots

`PreToolUse` copies the target file to
`~/.cache/diffier/<session>/<tool_use_id>`, or writes an `.absent` marker when
the file does not exist yet. `PostToolUse` copies it again to
`<tool_use_id>.after`, so an edit that lands before the monitor reaches the
event does not leak into the earlier card. Files over 5 MB are not copied.

`SessionStart` prunes snapshot directories older than two days.

## The spool

Every hook event is appended to `~/.local/state/diffier/events.jsonl` with the
large `tool_response.content`, `structuredPatch`, and `tool_input.content`
fields dropped, plus `originalFile` whenever a snapshot made it redundant.

Appends are serialized with a file lock so that concurrent subagent edits
cannot interleave into a corrupt line. The spool rotates to `events.jsonl.1` at
50 MB, on whichever event crosses the threshold; replay reads both files. A
`compact` `SessionStart` does not reset replay history.

## Sessions and worktrees

The monitor accepts an event when its `cwd` is in the same repository: one
`git rev-parse --git-common-dir --show-toplevel` per distinct cwd string,
cached, plus one for the monitor's own directory at startup. When git is
missing, the directory is not a repository, or `--cwd-only` is set, only the
exact directory matches. Card paths are relative to the card's own worktree
root.

Every session with a card in the feed gets a tag once there is more than one
of them. Tab cycles the feed between all sessions and each one in the order
its first card appeared; cards are hidden, never dropped. The replay boundary
spans the repository, so a newer `SessionStart` in another worktree becomes
the boundary and sessions idle since then drop out, the same rule as within
one directory.

A `SessionStart` clears only its own session's pending `PreToolUse` entries;
another session in the same repository may have an edit in flight.

Matching is by `cwd`, so an edit to a file outside the repository from a
session inside it still produces a card, shown with its absolute path. Cards
whose file sits under a `claude-*` directory directly inside `/tmp`,
`/private/tmp`, or the platform temp dir (the scratchpad) are marked `temp`:
the TUI hides them until `o`, counting them in the status bar, and `dump`
skips them without `--all-files`. The check is lexical on the recorded path.
Other out-of-repository files are not filtered.

## Rendering

The monitor diffs the pre-edit snapshot against the post-edit one, falling back
to the file on disk. When no pre-edit snapshot exists it uses
`tool_response.originalFile`, then the edit's `old_string`/`new_string`.

The diff is a list of hunks with three lines of context, each hunk a list of
rows: equal, deleted, or inserted, with line numbers on the sides the row
exists on. Rows in a replaced block carry word-level marks from `similar`'s
inline diff. Diffs are cut at 500 rows with a count of the remainder; the
`+N -N` in the card header counts the whole diff. Binary files and files
missing after the edit show a one-line notice. Subagent edits appear in the
same feed, labeled with the agent type.

Rendering has two stages. `paint` runs once per card: syntect colors each row
(syntax chosen by extension, then file name, then the first line of the file)
and the colors are merged with the word marks. `layout` runs whenever the
width, view mode, or wrap setting changes and only arranges painted rows into
lines: side by side (deleted and inserted runs paired line for line) or
unified, with tinted backgrounds on changed rows, a stronger tint on changed
words, and long lines wrapped into continuation rows or cut with an ellipsis.
Side by side needs 100 columns; below that every card renders unified
regardless of the selected mode, re-evaluated on each resize. The palette is
fixed for dark terminals.

The feed is one flat list of lines with a table of where each visible card
starts. The current card is the one owning the top visible line; its header is
pinned above the body, `n`/`N` move between card starts, and `Enter`/`z`
collapse cards to their header line.

## Hook safety

The hook is `diffier hook`, a subcommand of the same binary, registered in
`~/.claude/settings.json` by absolute path so it runs without `~/.cargo/bin`
on PATH. It always exits 0, including when `HOME` and the XDG variables are
all unset (then it writes nothing) and when it panics: `main` wraps it in
`catch_unwind` and calls `process::exit(0)`. A non-zero exit from a
`PreToolUse` hook would block Claude's tool call. It never writes to stdout.

Spool appends take an advisory lock on `spool.lock` next to the spool with
`File::try_lock`, retried for about a second. The kernel drops the lock when
the process exits, so a hook killed mid-write cannot wedge the others. If the
lock is not acquired within the budget the append happens anyway.

No shell is involved: payload fields are read as JSON strings, and a non-string
`file_path` counts as absent. `tests/hook.rs` covers these properties.
