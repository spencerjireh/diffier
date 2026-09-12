//! The `diffier hook` subcommand: consume one Claude Code hook payload.
//!
//! Claude Code runs this once per registered event with the payload on stdin.
//! It snapshots the target file around an edit and appends the payload to the
//! spool. Nothing here returns an error: a non-zero exit from a `PreToolUse`
//! hook blocks Claude's tool call, so every failure is swallowed and the
//! caller exits 0 regardless. It also never writes to stdout, which Claude
//! Code would try to parse.

use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::event::FILE_TOOLS;
use crate::paths::{Paths, rotated};
use crate::snapshot::{marker_path, snapshot_path};

/// Files larger than this get a `.toolarge` marker instead of a copy.
pub const MAX_SNAPSHOT_BYTES: u64 = 5 * 1024 * 1024;
/// The spool rotates to `<spool>.1` once it is strictly larger than this.
pub const MAX_SPOOL_BYTES: u64 = 50 * 1024 * 1024;
/// Snapshot directories idle longer than this are pruned at `SessionStart`.
pub const PRUNE_AFTER: Duration = Duration::from_secs(2 * 24 * 60 * 60);

const LOCK_TRIES: u32 = 20;
const LOCK_NAP: Duration = Duration::from_millis(50);

/// Locate the spool and snapshot root from the environment the hook runs in.
///
/// Mirrors the old shell hook: with `HOME` unset or empty and either XDG
/// variable missing there is nowhere sensible to write, so do nothing. `HOME`
/// is read directly rather than through `dirs` so an unset `HOME` does not
/// fall back to the passwd entry and write into the real home directory.
pub fn paths_from_env() -> Option<Paths> {
    let non_empty = |v: Option<std::ffi::OsString>| v.filter(|v| !v.is_empty());
    let home = non_empty(env::var_os("HOME"));
    let state = non_empty(env::var_os("XDG_STATE_HOME"));
    let cache = non_empty(env::var_os("XDG_CACHE_HOME"));
    if home.is_none() && (state.is_none() || cache.is_none()) {
        return None;
    }
    let home = home.map(PathBuf::from).unwrap_or_default();
    Some(Paths::from_parts(home, state, cache))
}

/// The string-valued fields the hook acts on. A non-string value counts as
/// absent, so an array-valued `file_path` can never name a path.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fields {
    pub event: String,
    pub tool_use_id: String,
    pub session_id: String,
    pub tool_name: String,
    pub file_path: String,
}

pub fn extract_fields(payload: &Value) -> Fields {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    let input = payload.get("tool_input").filter(|v| v.is_object());
    Fields {
        event: s(payload.get("hook_event_name")),
        tool_use_id: s(payload.get("tool_use_id")),
        session_id: s(payload.get("session_id")),
        tool_name: s(payload.get("tool_name")),
        file_path: s(input
            .and_then(|i| i.get("file_path"))
            .or_else(|| input.and_then(|i| i.get("notebook_path")))),
    }
}

impl Fields {
    fn snapshots_file(&self) -> bool {
        FILE_TOOLS.contains(&self.tool_name.as_str())
            && !self.tool_use_id.is_empty()
            && !self.session_id.is_empty()
            && !self.file_path.is_empty()
    }
}

/// Handle one payload end to end. Returns `false` when nothing was written,
/// which only the tests look at.
pub fn run(paths: &Paths, payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let Some(state_dir) = paths.spool.parent() else {
        return false;
    };
    if fs::create_dir_all(state_dir).is_err() || fs::create_dir_all(&paths.snapshot_root).is_err() {
        return false;
    }

    let parsed: Option<Value> = serde_json::from_slice(payload).ok();
    let fields = parsed.as_ref().map(extract_fields).unwrap_or_default();
    let base = snapshot_path(
        &paths.snapshot_root,
        &fields.session_id,
        &fields.tool_use_id,
    );

    match fields.event.as_str() {
        "PreToolUse" if fields.snapshots_file() => {
            snapshot_pre(&base, Path::new(&fields.file_path))
        }
        "PostToolUse" if fields.snapshots_file() => {
            snapshot_post(&base, Path::new(&fields.file_path))
        }
        "SessionStart" => prune_sessions(&paths.snapshot_root, SystemTime::now()),
        _ => {}
    }

    let line = match parsed {
        // A pre-edit snapshot makes originalFile redundant; markers do not.
        Some(Value::Object(obj)) => {
            let keep_original = !(fields.snapshots_file() && base.is_file());
            build_line(obj, keep_original, now_ms())
        }
        _ => raw_line(payload),
    };
    append_line(&paths.spool, state_dir, &line)
}

fn snapshot_pre(base: &Path, file: &Path) {
    let Some(dir) = base.parent() else {
        return;
    };
    let _ = fs::create_dir_all(dir);
    match fs::metadata(file) {
        Ok(m) if m.is_file() && m.len() <= MAX_SNAPSHOT_BYTES => {
            if fs::copy(file, base).is_err() {
                let _ = fs::remove_file(base);
                touch(&marker_path(base, "error"));
            }
        }
        Ok(m) if m.is_file() => touch(&marker_path(base, "toolarge")),
        _ => touch(&marker_path(base, "absent")),
    }
}

/// Snapshot the post-edit content too, so a later edit to the same file that
/// lands before the monitor reaches this event does not leak into its card.
fn snapshot_post(base: &Path, file: &Path) {
    let Some(dir) = base.parent() else {
        return;
    };
    let _ = fs::create_dir_all(dir);
    let after = marker_path(base, "after");
    match fs::metadata(file) {
        Ok(m) if m.is_file() && m.len() <= MAX_SNAPSHOT_BYTES => {
            if fs::copy(file, &after).is_err() {
                let _ = fs::remove_file(&after);
            }
        }
        Ok(m) if m.is_file() => {}
        _ => touch(&marker_path(&after, "absent")),
    }
}

fn touch(path: &Path) {
    let _ = File::create(path);
}

/// Remove per-session snapshot directories whose mtime is older than
/// `PRUNE_AFTER` relative to `now`.
pub fn prune_sessions(root: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > PRUNE_AFTER);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drop the bulky fields the monitor never reads and stamp the event.
pub fn build_line(mut obj: Map<String, Value>, keep_original_file: bool, ts: u64) -> Vec<u8> {
    if let Some(resp) = obj.get_mut("tool_response").and_then(Value::as_object_mut) {
        resp.remove("content");
        resp.remove("structuredPatch");
        if !keep_original_file {
            resp.remove("originalFile");
        }
    }
    if let Some(input) = obj.get_mut("tool_input").and_then(Value::as_object_mut) {
        input.remove("content");
    }
    obj.insert("ts".into(), Value::from(ts));
    let mut line = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
    line.push(b'\n');
    line
}

/// An unparsable payload still produces one line, so a broken event is
/// visible in the spool rather than silently gone. The reader skips it.
fn raw_line(payload: &[u8]) -> Vec<u8> {
    let mut line: Vec<u8> = payload.iter().copied().filter(|b| *b != b'\n').collect();
    line.push(b'\n');
    line
}

/// Rotate if needed and append `line` under the spool lock. The lock is
/// advisory and released by the kernel when the process exits, so a hook that
/// dies mid-write cannot wedge the others. If it cannot be taken within the
/// budget the append happens anyway; a torn line beats a lost one.
fn append_line(spool: &Path, state_dir: &Path, line: &[u8]) -> bool {
    let _guard = SpoolLock::acquire(&state_dir.join("spool.lock"));
    if let Ok(m) = fs::metadata(spool)
        && m.len() > MAX_SPOOL_BYTES
    {
        let _ = fs::rename(spool, rotated(spool));
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(spool)
        .and_then(|mut f| f.write_all(line))
        .is_ok()
}

struct SpoolLock(File);

impl SpoolLock {
    fn acquire(path: &Path) -> Option<Self> {
        let file = File::create(path).ok()?;
        for _ in 0..LOCK_TRIES {
            match file.try_lock() {
                Ok(()) => return Some(Self(file)),
                Err(TryLockError::WouldBlock) => thread::sleep(LOCK_NAP),
                Err(TryLockError::Error(_)) => return None,
            }
        }
        None
    }
}

impl Drop for SpoolLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_string_fields_are_absent() {
        let f = extract_fields(&json!({
            "hook_event_name": "PreToolUse",
            "tool_name": ["Edit", "touch", "x"],
            "tool_use_id": 7,
            "session_id": "s",
            "tool_input": {"file_path": ["a", "b"]}
        }));
        assert_eq!(f.event, "PreToolUse");
        assert_eq!(f.tool_name, "");
        assert_eq!(f.tool_use_id, "");
        assert_eq!(f.file_path, "");
        assert!(!f.snapshots_file());
    }

    #[test]
    fn notebook_path_is_the_fallback() {
        let f = extract_fields(&json!({
            "tool_input": {"notebook_path": "/n.ipynb"}
        }));
        assert_eq!(f.file_path, "/n.ipynb");
        let f = extract_fields(&json!({"tool_input": "not an object"}));
        assert_eq!(f.file_path, "");
    }

    #[test]
    fn build_line_drops_bulk_and_stamps_ts() {
        let obj = json!({
            "tool_input": {"file_path": "/a", "content": "big"},
            "tool_response": {"content": "big", "structuredPatch": [], "originalFile": "old"}
        });
        let Value::Object(obj) = obj else {
            unreachable!()
        };
        let line = build_line(obj.clone(), false, 1234);
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["tool_input"], json!({"file_path": "/a"}));
        assert_eq!(v["tool_response"], json!({}));
        assert_eq!(v["ts"], 1234);
        assert_eq!(line.last(), Some(&b'\n'));

        let v: Value = serde_json::from_slice(&build_line(obj, true, 1)).unwrap();
        assert_eq!(v["tool_response"], json!({"originalFile": "old"}));
    }

    #[test]
    fn raw_line_strips_newlines() {
        assert_eq!(raw_line(b"{not json\nat all"), b"{not jsonat all\n");
    }

    #[test]
    fn prune_removes_only_stale_directories() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let fresh = dir.path().join("fresh");
        let file = dir.path().join("file");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&fresh).unwrap();
        fs::write(&file, "x").unwrap();
        let now = SystemTime::now();
        File::open(&old)
            .unwrap()
            .set_modified(now - PRUNE_AFTER - Duration::from_secs(60))
            .unwrap();
        File::open(&file)
            .unwrap()
            .set_modified(now - PRUNE_AFTER - Duration::from_secs(60))
            .unwrap();
        prune_sessions(dir.path(), now);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(file.exists());
    }

    #[test]
    fn empty_payload_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            spool: dir.path().join("state/events.jsonl"),
            snapshot_root: dir.path().join("cache"),
            hook_script: dir.path().join("hook.sh"),
            settings: dir.path().join("settings.json"),
        };
        assert!(!run(&paths, b""));
        assert!(!paths.spool.exists());
    }
}
