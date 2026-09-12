# diffier

[![CI](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml/badge.svg)](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A live diff feed for Claude Code. Run it in a second pane and every Edit, Write,
MultiEdit, and NotebookEdit shows up as a full diff the moment it lands,
rendered through [delta](https://github.com/dandavison/delta) when it is
installed.

## Install

`delta` is optional; without it diffs render in a plain built-in style.

```sh
cargo install diffier
diffier install
```

`cargo binstall diffier` downloads the prebuilt binary for your platform from
the [releases page](https://github.com/spencerjireh/diffier/releases) instead of
compiling; the tarballs there can also be unpacked by hand. To build from the
development branch, use `cargo install --git https://github.com/spencerjireh/diffier`.

The binary lands in `~/.cargo/bin`. If `diffier` is not found afterwards, add
that directory to your `PATH`.

`diffier install` registers `diffier hook` in `~/.claude/settings.json` by the
absolute path of the binary, preserving everything else in that file and
writing a one-time `settings.json.bak`. Restart any running Claude Code session
so the hooks load. Run it again if you move the binary; it also removes the
`~/.claude/hooks/diffier.sh` script that versions before 0.2 installed.

## Use

Run `diffier` in any second terminal, in the repository you run `claude` in. A
tmux pane is one way to get that terminal; a separate window, tab, or editor
terminal panel works the same, because the monitor reads a spool file and does
not talk to the multiplexer.

```sh
cd ~/Projects/some-repo
tmux split-window -h diffier   # then run claude in the other pane
```

It follows every session in the git repository it started in, including other
worktrees and subdirectories, replaying their recent edits before following new
ones. Once a second session has made an edit, each card carries a session tag
(the last six characters of the session id, prefixed with the worktree name
when it differs). Outside a repository, or with `--cwd-only`, it follows only
sessions whose working directory matches its own.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, wheel | scroll |
| `Ctrl-d` / `Ctrl-u`, PgDn / PgUp | half page / page |
| `g` / `G` | top / bottom, and resume follow |
| `p` | toggle follow |
| `Tab` | cycle the session filter: all, then each session |
| `q`, Esc, `Ctrl-c` | quit |

Scrolling up pauses follow. Mouse capture is on for the wheel, so select text
with tmux copy mode or shift-drag.

The hook records edits whether or not a monitor is running, so you can also skip
the live view and print the cards after the fact:

```sh
diffier dump             # print the current sessions' cards and exit
diffier dump --session ab12cd   # only one session, by tag or id prefix
diffier run --no-delta   # force the plain renderer
diffier run --cwd-only   # ignore other worktrees of this repository
diffier uninstall        # remove the hook and its settings entries
```

`diffier --help` covers the rest.

## More

- [docs/design.md](docs/design.md) — how the hook, spool, and snapshots fit together
- [CONTRIBUTING.md](CONTRIBUTING.md) — running the checks CI runs
- MIT licensed. See [LICENSE](LICENSE).
