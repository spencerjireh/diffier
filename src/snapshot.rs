//! Resolve the pre-edit content for a PostToolUse event.
//!
//! Lookup order for `<root>/<session_id>/<tool_use_id>`:
//! 1. snapshot file            -> `Content`
//! 2. `<id>.absent` marker     -> `Absent` (file did not exist before the edit)
//! 3. anything else            -> `tool_response.originalFile` if present,
//!    else the edit strings for Edit/MultiEdit, else `Unavailable`.
//!
//! The hook also writes `<id>.after` (and `<id>.after.absent`) at PostToolUse;
//! see `resolve_after`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::event::HookEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Before {
    Content(Vec<u8>),
    Absent,
    /// No file snapshot; diff `old` against `new` and label it with `note`.
    Synthetic {
        old: String,
        new: String,
        note: String,
    },
    Unavailable(String),
}

pub fn snapshot_path(root: &Path, session_id: &str, tool_use_id: &str) -> PathBuf {
    root.join(session_id).join(tool_use_id)
}

/// `<base>.<suffix>`. Appends rather than replacing an extension: a tool_use_id
/// like `toolu_01.x` would otherwise have its own suffix overwritten.
pub fn marker_path(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

pub fn resolve(root: &Path, ev: &HookEvent) -> Before {
    if let (Some(sid), Some(id)) = (ev.session_id.as_deref(), ev.tool_use_id.as_deref()) {
        let base = snapshot_path(root, sid, id);
        if let Ok(bytes) = fs::read(&base) {
            return Before::Content(bytes);
        }
        if marker_path(&base, "absent").exists() {
            return Before::Absent;
        }
        let reason = if marker_path(&base, "toolarge").exists() {
            "file over snapshot size limit"
        } else if marker_path(&base, "error").exists() {
            "snapshot copy failed"
        } else {
            "no snapshot"
        };
        return fallback(ev, reason);
    }
    fallback(ev, "no snapshot")
}

/// Post-edit content as captured by the hook at PostToolUse.
///
/// `Some(Some(bytes))` when the file was snapshotted, `Some(None)` when it was
/// gone, and `None` when no after-snapshot exists and the caller should read
/// the file from disk. Reading from disk is only correct when nothing has
/// touched the file since: during replay, and whenever several edits to one
/// file are processed in the same tick, the file already holds later edits.
pub fn resolve_after(root: &Path, ev: &HookEvent) -> Option<Option<Vec<u8>>> {
    let (sid, id) = (ev.session_id.as_deref()?, ev.tool_use_id.as_deref()?);
    let base = snapshot_path(root, sid, id);
    let after = marker_path(&base, "after");
    if let Ok(bytes) = fs::read(&after) {
        return Some(Some(bytes));
    }
    if marker_path(&after, "absent").exists() {
        return Some(None);
    }
    None
}

fn fallback(ev: &HookEvent, reason: &str) -> Before {
    if let Some(orig) = ev.response_str("originalFile") {
        return Before::Content(orig.as_bytes().to_vec());
    }
    match ev.tool_name.as_deref() {
        Some("Edit") => {
            let old = ev.input_str("old_string").unwrap_or_default().to_string();
            let new = ev.input_str("new_string").unwrap_or_default().to_string();
            Before::Synthetic {
                old,
                new,
                note: format!("{reason}; showing edit strings"),
            }
        }
        Some("MultiEdit") => {
            let edits = ev
                .tool_input
                .as_ref()
                .and_then(|i| i.get("edits"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut old = String::new();
            let mut new = String::new();
            for (i, e) in edits.iter().enumerate() {
                if i > 0 {
                    old.push_str("\n...\n");
                    new.push_str("\n...\n");
                }
                old.push_str(
                    e.get("old_string")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                new.push_str(
                    e.get("new_string")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            Before::Synthetic {
                old,
                new,
                note: format!("{reason}; showing edit strings"),
            }
        }
        _ => Before::Unavailable(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(tool: &str, input: &str, response: &str) -> HookEvent {
        HookEvent::parse_line(&format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"t","tool_name":"{tool}","tool_input":{input},"tool_response":{response}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn snapshot_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("s")).unwrap();
        fs::write(dir.path().join("s/t"), b"old").unwrap();
        let ev = post(
            "Write",
            r#"{"file_path":"/x"}"#,
            r#"{"originalFile":"ignored"}"#,
        );
        assert_eq!(resolve(dir.path(), &ev), Before::Content(b"old".to_vec()));
    }

    #[test]
    fn absent_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("s")).unwrap();
        fs::write(dir.path().join("s/t.absent"), b"").unwrap();
        let ev = post("Write", r#"{"file_path":"/x"}"#, r#"{}"#);
        assert_eq!(resolve(dir.path(), &ev), Before::Absent);
    }

    #[test]
    fn original_file_fallback_then_synthetic_then_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let w = post("Write", r#"{"file_path":"/x"}"#, r#"{"originalFile":"o"}"#);
        assert_eq!(resolve(dir.path(), &w), Before::Content(b"o".to_vec()));
        let e = post(
            "Edit",
            r#"{"file_path":"/x","old_string":"a","new_string":"b"}"#,
            r#"{}"#,
        );
        match resolve(dir.path(), &e) {
            Before::Synthetic { old, new, note } => {
                assert_eq!((old.as_str(), new.as_str()), ("a", "b"));
                assert!(note.starts_with("no snapshot"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let n = post("NotebookEdit", r#"{"notebook_path":"/n"}"#, r#"{}"#);
        assert_eq!(
            resolve(dir.path(), &n),
            Before::Unavailable("no snapshot".into())
        );
    }

    #[test]
    fn after_snapshot_and_absent_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("s")).unwrap();
        let ev = post("Write", r#"{"file_path":"/x"}"#, r#"{}"#);
        assert_eq!(resolve_after(dir.path(), &ev), None);
        fs::write(dir.path().join("s/t.after"), b"new").unwrap();
        assert_eq!(resolve_after(dir.path(), &ev), Some(Some(b"new".to_vec())));
        fs::remove_file(dir.path().join("s/t.after")).unwrap();
        fs::write(dir.path().join("s/t.after.absent"), b"").unwrap();
        assert_eq!(resolve_after(dir.path(), &ev), Some(None));
    }

    #[test]
    fn markers_are_appended_not_substituted_for_dotted_ids() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("s")).unwrap();
        // A tool_use_id with a dot: `with_extension` would look for `toolu_01.absent`.
        fs::write(dir.path().join("s/toolu_01.x.absent"), b"").unwrap();
        let ev = HookEvent::parse_line(
            r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"toolu_01.x","tool_name":"Write","tool_input":{"file_path":"/x"}}"#,
        )
        .unwrap();
        assert_eq!(resolve(dir.path(), &ev), Before::Absent);
    }

    #[test]
    fn toolarge_marker_reason() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("s")).unwrap();
        fs::write(dir.path().join("s/t.toolarge"), b"").unwrap();
        let n = post("NotebookEdit", r#"{"notebook_path":"/n"}"#, r#"{}"#);
        assert_eq!(
            resolve(dir.path(), &n),
            Before::Unavailable("file over snapshot size limit".into())
        );
    }
}
