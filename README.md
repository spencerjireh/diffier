# diffier

[![CI](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml/badge.svg)](https://github.com/spencerjireh/diffier/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A live diff feed for Claude Code. Run it in a second pane and every Edit, Write,
MultiEdit, and NotebookEdit shows up as a full diff the moment it lands,
rendered through [delta](https://github.com/dandavison/delta) when it is
installed.

## Install

Requires `jq`. `delta` is optional. Prebuilt macOS and Linux binaries are on the
[releases page](https://github.com/spencerjireh/diffier/releases), or build it:

```sh
cargo install --git https://github.com/spencerjireh/diffier
diffier install
```

`diffier install` writes `~/.claude/hooks/diffier.sh` and registers it in
`~/.claude/settings.json`, preserving everything else in that file and writing a
one-time `settings.json.bak`. Restart any running Claude Code session so the
hooks load.

## Use

```sh
cd ~/Projects/some-repo
tmux split-window -h diffier   # then run claude in the other pane
```

It follows only sessions whose working directory matches the one it started in,
replaying that directory's recent edits before following new ones.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, wheel | scroll |
| `Ctrl-d` / `Ctrl-u`, PgDn / PgUp | half page / page |
| `g` / `G` | top / bottom, and resume follow |
| `p` | toggle follow |
| `q`, Esc, `Ctrl-c` | quit |

Scrolling up pauses follow. Mouse capture is on for the wheel, so select text
with tmux copy mode or shift-drag.

```sh
diffier dump             # print the current session's cards and exit
diffier run --no-delta   # force the plain renderer
diffier uninstall        # remove the hook and its settings entries
```

`diffier --help` covers the rest.

## More

- [docs/design.md](docs/design.md) — how the hook, spool, and snapshots fit together
- [CONTRIBUTING.md](CONTRIBUTING.md) — running the checks CI runs
- MIT licensed. See [LICENSE](LICENSE).
