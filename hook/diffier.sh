#!/bin/sh
# diffier hook: snapshots files before and after Edit/Write tools and
# appends every event to a JSONL spool. Installed by `diffier install`.
#
# This script must NEVER exit non-zero. Exit code 2 on PreToolUse would block
# Claude's tool call, and any other non-zero code shows a notice in the session.
set -u

# HOME may be unset (systemd units, env -i). Bail before `set -u` trips on it.
if [ -z "${HOME:-}" ] && { [ -z "${XDG_STATE_HOME:-}" ] || [ -z "${XDG_CACHE_HOME:-}" ]; }; then
  exit 0
fi

STATE_DIR="${XDG_STATE_HOME:-${HOME:-}/.local/state}/diffier"
CACHE_DIR="${XDG_CACHE_HOME:-${HOME:-}/.cache}/diffier"
SPOOL="$STATE_DIR/events.jsonl"
LOCK="$STATE_DIR/spool.lock"
MAX_SNAPSHOT_BYTES=5242880   # 5 MB
MAX_SPOOL_BYTES=52428800     # 50 MB

payload=$(cat) || exit 0
[ -n "$payload" ] || exit 0
mkdir -p "$STATE_DIR" "$CACHE_DIR" 2>/dev/null || exit 0

ev=; id=; sid=; tn=; fp=
# @sh single-quotes each value, so eval is safe for paths with spaces or quotes.
# `strings` restricts every field to a string: a non-string (array, object)
# would otherwise expand to extra words that eval runs as a command.
eval "$(printf '%s' "$payload" | jq -r '@sh "ev=\(.hook_event_name | strings // "") id=\(.tool_use_id | strings // "") sid=\(.session_id | strings // "") tn=\(.tool_name | strings // "") fp=\((.tool_input | objects | (.file_path // .notebook_path) | strings) // "")"' 2>/dev/null)" 2>/dev/null

file_tool=0
case "$tn" in
  Edit|Write|MultiEdit|NotebookEdit) file_tool=1 ;;
esac

# Size of a regular file, or empty when it cannot be measured.
file_size() {
  wc -c < "$1" 2>/dev/null | tr -d ' '
}

dir="$CACHE_DIR/$sid"
case "$ev" in
  PreToolUse)
    if [ "$file_tool" = 1 ] && [ -n "$id" ] && [ -n "$sid" ] && [ -n "$fp" ]; then
      mkdir -p "$dir" 2>/dev/null
      if [ -f "$fp" ]; then
        size=$(file_size "$fp")
        if [ "${size:-0}" -le "$MAX_SNAPSHOT_BYTES" ]; then
          cp -p -- "$fp" "$dir/$id" 2>/dev/null || : > "$dir/$id.error"
        else
          : > "$dir/$id.toolarge"
        fi
      else
        : > "$dir/$id.absent"
      fi
    fi
    ;;
  PostToolUse)
    # Snapshot the post-edit content too, so a later edit to the same file does
    # not leak into this card when the monitor processes it afterwards.
    if [ "$file_tool" = 1 ] && [ -n "$id" ] && [ -n "$sid" ] && [ -n "$fp" ]; then
      mkdir -p "$dir" 2>/dev/null
      if [ -f "$fp" ]; then
        size=$(file_size "$fp")
        if [ "${size:-0}" -le "$MAX_SNAPSHOT_BYTES" ]; then
          cp -p -- "$fp" "$dir/$id.after" 2>/dev/null || rm -f "$dir/$id.after" 2>/dev/null
        fi
      else
        : > "$dir/$id.after.absent"
      fi
    fi
    ;;
  SessionStart)
    find "$CACHE_DIR" -mindepth 1 -maxdepth 1 -type d -mtime +2 -exec rm -rf {} + 2>/dev/null
    ;;
esac

# Drop the large fields the monitor never reads. `originalFile` is only used as
# a fallback when no pre-edit snapshot exists, so keep it just in that case.
drop='.tool_response.content, .tool_response.structuredPatch, .tool_input.content'
if [ -n "$id" ] && [ -n "$sid" ] && [ -f "$dir/$id" ]; then
  drop="$drop, .tool_response.originalFile"
fi
line=$(printf '%s' "$payload" | jq -c "del($drop) + {ts: (now * 1000 | floor)}" 2>/dev/null) || line=
[ -n "$line" ] || line=$(printf '%s' "$payload" | tr -d '\n')

# Serialize appends across concurrent hook processes: a multi-KB line can be
# written in several write() calls, which interleave under O_APPEND. mkdir is
# atomic on every POSIX filesystem. A lock whose owner is gone is stale.
# Each attempt costs a sleep plus a few small processes, so keep the whole wait
# well inside the 5s hook timeout. A holder keeps the lock for milliseconds;
# this budget is only ever reached when something is genuinely wedged, and even
# then the append below still happens.
if sleep 0.05 2>/dev/null; then
  nap="sleep 0.05"
  max_tries=20
else
  nap="sleep 1"
  max_tries=2
fi
locked=0
tries=0
while [ "$tries" -lt "$max_tries" ]; do
  if mkdir "$LOCK" 2>/dev/null; then
    printf '%s' "$$" > "$LOCK/pid" 2>/dev/null
    locked=1
    break
  fi
  tries=$((tries + 1))
  # Stale when its owner is gone, or when it is old enough that no live hook
  # could still hold it (the timeout is 5s) -- covers a process killed between
  # the mkdir and the pid write, which leaves a lock nobody owns.
  owner=$(cat "$LOCK/pid" 2>/dev/null)
  if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then
    rm -rf "$LOCK" 2>/dev/null
  elif [ -n "$(find "$LOCK" -maxdepth 0 -mmin +1 2>/dev/null)" ]; then
    rm -rf "$LOCK" 2>/dev/null
  else
    $nap
  fi
done

if [ -f "$SPOOL" ]; then
  size=$(file_size "$SPOOL")
  [ "${size:-0}" -gt "$MAX_SPOOL_BYTES" ] && mv -f "$SPOOL" "$SPOOL.1" 2>/dev/null
fi
printf '%s\n' "$line" >> "$SPOOL" 2>/dev/null

[ "$locked" = 1 ] && rm -rf "$LOCK" 2>/dev/null
exit 0
