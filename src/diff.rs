//! Unified diff computation and classification.

use std::path::Path;

use similar::TextDiff;

use crate::snapshot::Before;

/// Unified-diff lines kept per card; the rest is summarized.
pub const MAX_LINES: usize = 500;
const BINARY_PROBE: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Modified,
    Created,
    Deleted,
    Binary,
    /// File absent after the edit and no snapshot existed.
    Missing,
    /// Diff shown (possibly synthetic) with a caveat, or nothing to show.
    Error(String),
}

impl DiffKind {
    pub fn label(&self) -> String {
        match self {
            DiffKind::Modified => "Modified".into(),
            DiffKind::Created => "Created".into(),
            DiffKind::Deleted => "Deleted".into(),
            DiffKind::Binary => "Binary".into(),
            DiffKind::Missing => "Missing".into(),
            DiffKind::Error(m) => format!("Error: {m}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffResult {
    pub kind: DiffKind,
    /// Unified diff text, cut at `MAX_LINES`. Empty when nothing to show.
    pub unified: String,
    /// Line count of the full unified diff before truncation.
    pub total_lines: usize,
    pub truncated: bool,
}

impl DiffResult {
    fn bare(kind: DiffKind) -> Self {
        Self {
            kind,
            unified: String::new(),
            total_lines: 0,
            truncated: false,
        }
    }
}

/// Path shown on the card: relative when inside `cwd`, absolute otherwise.
pub fn display_path(file: &Path, cwd: &Path) -> String {
    if let Ok(r) = file.strip_prefix(cwd) {
        return r.to_string_lossy().into_owned();
    }
    if let Ok(canon) = std::fs::canonicalize(cwd)
        && let Ok(r) = file.strip_prefix(&canon)
    {
        return r.to_string_lossy().into_owned();
    }
    file.to_string_lossy().into_owned()
}

fn looks_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(BINARY_PROBE)];
    probe.contains(&0) || std::str::from_utf8(bytes).is_err()
}

pub fn compute(before: &Before, after: Option<&[u8]>, rel_path: &str) -> DiffResult {
    match (before, after) {
        (Before::Unavailable(msg), Some(_)) => DiffResult::bare(DiffKind::Error(msg.clone())),
        (Before::Unavailable(_), None) => DiffResult::bare(DiffKind::Missing),
        (Before::Absent, None) => DiffResult::bare(DiffKind::Missing),
        (Before::Synthetic { old, new, note }, _) => {
            let mut r = unified(
                old.as_bytes(),
                new.as_bytes(),
                "/dev/null",
                &format!("b/{rel_path}"),
            );
            r.kind = DiffKind::Error(note.clone());
            r
        }
        (Before::Absent, Some(new)) => {
            if looks_binary(new) {
                return DiffResult::bare(DiffKind::Binary);
            }
            let mut r = unified(b"", new, "/dev/null", &format!("b/{rel_path}"));
            r.kind = DiffKind::Created;
            r
        }
        (Before::Content(old), None) => {
            if looks_binary(old) {
                return DiffResult::bare(DiffKind::Binary);
            }
            let mut r = unified(old, b"", &format!("a/{rel_path}"), "/dev/null");
            r.kind = DiffKind::Deleted;
            r
        }
        (Before::Content(old), Some(new)) => {
            if looks_binary(old) || looks_binary(new) {
                return DiffResult::bare(DiffKind::Binary);
            }
            unified(old, new, &format!("a/{rel_path}"), &format!("b/{rel_path}"))
        }
    }
}

fn unified(old: &[u8], new: &[u8], a: &str, b: &str) -> DiffResult {
    let old_s = String::from_utf8_lossy(old);
    let new_s = String::from_utf8_lossy(new);
    if old_s == new_s {
        return DiffResult::bare(DiffKind::Modified);
    }
    let diff = TextDiff::from_lines(old_s.as_ref(), new_s.as_ref());
    let full = diff
        .unified_diff()
        .context_radius(3)
        .header(a, b)
        .to_string();
    let total_lines = full.lines().count();
    let truncated = total_lines > MAX_LINES;
    let text = if truncated {
        let mut s: String = full.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n");
        s.push('\n');
        s
    } else {
        full
    };
    DiffResult {
        kind: DiffKind::Modified,
        unified: text,
        total_lines,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn modified_has_headers_and_hunk() {
        let r = compute(
            &Before::Content(b"a\nb\nc\n".to_vec()),
            Some(b"a\nB\nc\n"),
            "src/x.rs",
        );
        assert_eq!(r.kind, DiffKind::Modified);
        assert!(r.unified.starts_with("--- a/src/x.rs\n+++ b/src/x.rs\n@@"));
        assert!(r.unified.contains("-b\n+B\n"));
        assert!(!r.truncated);
    }

    #[test]
    fn created_deleted_missing() {
        let c = compute(&Before::Absent, Some(b"new\n"), "n.txt");
        assert_eq!(c.kind, DiffKind::Created);
        assert!(c.unified.starts_with("--- /dev/null\n+++ b/n.txt\n"));
        let d = compute(&Before::Content(b"gone\n".to_vec()), None, "g.txt");
        assert_eq!(d.kind, DiffKind::Deleted);
        assert!(d.unified.contains("+++ /dev/null"));
        assert_eq!(compute(&Before::Absent, None, "m").kind, DiffKind::Missing);
        assert_eq!(
            compute(&Before::Unavailable("x".into()), None, "m").kind,
            DiffKind::Missing
        );
    }

    #[test]
    fn binary_detected_on_either_side() {
        let bin = vec![0u8, 1, 2];
        assert_eq!(
            compute(&Before::Content(bin.clone()), Some(b"txt"), "b").kind,
            DiffKind::Binary
        );
        assert_eq!(
            compute(&Before::Content(b"txt".to_vec()), Some(&bin), "b").kind,
            DiffKind::Binary
        );
        assert_eq!(
            compute(&Before::Absent, Some(&bin), "b").kind,
            DiffKind::Binary
        );
    }

    #[test]
    fn truncates_at_max_lines() {
        let old = String::new();
        let new: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let r = compute(
            &Before::Content(old.into_bytes()),
            Some(new.as_bytes()),
            "big",
        );
        assert!(r.truncated);
        assert_eq!(r.unified.lines().count(), MAX_LINES);
        // headers (2) + hunk header (1) + 1000 added lines
        assert_eq!(r.total_lines, 1003);
    }

    #[test]
    fn identical_content_is_empty_modified() {
        let r = compute(&Before::Content(b"same\n".to_vec()), Some(b"same\n"), "s");
        assert_eq!(r.kind, DiffKind::Modified);
        assert!(r.unified.is_empty());
    }

    #[test]
    fn synthetic_and_unavailable() {
        let s = Before::Synthetic {
            old: "a\n".into(),
            new: "b\n".into(),
            note: "why".into(),
        };
        let r = compute(&s, Some(b"whatever"), "f");
        assert_eq!(r.kind, DiffKind::Error("why".into()));
        assert!(r.unified.contains("-a\n+b\n"));
        let u = compute(&Before::Unavailable("nope".into()), Some(b"x"), "f");
        assert_eq!(u.kind, DiffKind::Error("nope".into()));
        assert!(u.unified.is_empty());
    }

    #[test]
    fn display_path_relative_inside_cwd_else_absolute() {
        let cwd = PathBuf::from("/p/proj");
        assert_eq!(
            display_path(Path::new("/p/proj/src/a.rs"), &cwd),
            "src/a.rs"
        );
        assert_eq!(
            display_path(Path::new("/tmp/other.rs"), &cwd),
            "/tmp/other.rs"
        );
    }
}
