//! Diff computation and classification: hunks of rows with word-level marks.

use std::path::Path;

use similar::{ChangeTag, DiffOp, TextDiff};

use crate::snapshot::Before;

/// Diff rows kept per card; the rest is summarized.
pub const MAX_LINES: usize = 500;
const CONTEXT: usize = 3;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Equal,
    Delete,
    Insert,
}

/// One line of one side of the diff, or a context line on both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// 1-based line numbers; `None` on the side the row does not exist on.
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// `(emphasized, text)` pieces; emphasized pieces are the words that
    /// changed within a replaced block. No trailing newline.
    pub segments: Vec<(bool, String)>,
}

impl Row {
    pub fn text(&self) -> String {
        self.segments.iter().map(|(_, s)| s.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub rows: Vec<Row>,
}

impl Hunk {
    /// `@@ -a,b +c,d @@`
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffResult {
    pub kind: DiffKind,
    /// Hunks with three lines of context, cut at `MAX_LINES` rows. Empty when
    /// nothing to show.
    pub hunks: Vec<Hunk>,
    /// Inserted and deleted line counts over the whole diff.
    pub added: usize,
    pub removed: usize,
    /// Row count of the full diff before truncation.
    pub total_rows: usize,
    pub truncated: bool,
    /// First line of the new text (old text for a deletion), for syntax
    /// detection by shebang or mode line.
    pub first_line: String,
}

impl DiffResult {
    fn bare(kind: DiffKind) -> Self {
        Self {
            kind,
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            total_rows: 0,
            truncated: false,
            first_line: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.hunks.iter().flat_map(|h| h.rows.iter())
    }

    /// Rows as ` `/`-`/`+` prefixed lines, for assertions.
    #[cfg(test)]
    pub(crate) fn unified_text(&self) -> String {
        self.rows()
            .map(|r| {
                let m = match r.kind {
                    RowKind::Equal => ' ',
                    RowKind::Delete => '-',
                    RowKind::Insert => '+',
                };
                format!("{m}{}\n", r.text())
            })
            .collect()
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

pub fn compute(before: &Before, after: Option<&[u8]>) -> DiffResult {
    match (before, after) {
        (Before::Unavailable(msg), Some(_)) => DiffResult::bare(DiffKind::Error(msg.clone())),
        (Before::Unavailable(_), None) => DiffResult::bare(DiffKind::Missing),
        (Before::Absent, None) => DiffResult::bare(DiffKind::Missing),
        (Before::Synthetic { old, new, note }, _) => {
            let mut r = structured(old.as_bytes(), new.as_bytes());
            r.kind = DiffKind::Error(note.clone());
            r
        }
        (Before::Absent, Some(new)) => {
            if looks_binary(new) {
                return DiffResult::bare(DiffKind::Binary);
            }
            let mut r = structured(b"", new);
            r.kind = DiffKind::Created;
            r
        }
        (Before::Content(old), None) => {
            if looks_binary(old) {
                return DiffResult::bare(DiffKind::Binary);
            }
            let mut r = structured(old, b"");
            r.kind = DiffKind::Deleted;
            r
        }
        (Before::Content(old), Some(new)) => {
            if looks_binary(old) || looks_binary(new) {
                return DiffResult::bare(DiffKind::Binary);
            }
            structured(old, new)
        }
    }
}

fn strip_newline(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .or_else(|| s.strip_suffix('\r'))
        .unwrap_or(s)
}

fn structured(old: &[u8], new: &[u8]) -> DiffResult {
    let old_s = String::from_utf8_lossy(old);
    let new_s = String::from_utf8_lossy(new);
    if old_s == new_s {
        return DiffResult::bare(DiffKind::Modified);
    }
    let first_line = if new_s.is_empty() { &old_s } else { &new_s };
    let first_line = strip_newline(first_line.lines().next().unwrap_or("")).to_string();
    let diff = TextDiff::from_lines(old_s.as_ref(), new_s.as_ref());
    let (mut added, mut removed) = (0, 0);
    for op in diff.ops() {
        match op {
            DiffOp::Delete { old_len, .. } => removed += old_len,
            DiffOp::Insert { new_len, .. } => added += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                removed += old_len;
                added += new_len;
            }
            DiffOp::Equal { .. } => {}
        }
    }
    let mut hunks = Vec::new();
    let mut total_rows = 0;
    let mut kept = 0;
    let mut truncated = false;
    for group in diff.grouped_ops(CONTEXT) {
        let mut rows = Vec::new();
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => RowKind::Equal,
                    ChangeTag::Delete => RowKind::Delete,
                    ChangeTag::Insert => RowKind::Insert,
                };
                let mut segments: Vec<(bool, String)> = Vec::new();
                for (emph, s) in change.iter_strings_lossy() {
                    let s = strip_newline(&s);
                    if s.is_empty() {
                        continue;
                    }
                    match segments.last_mut() {
                        Some((e, last)) if *e == emph => last.push_str(s),
                        _ => segments.push((emph, s.to_string())),
                    }
                }
                if segments.is_empty() {
                    segments.push((false, String::new()));
                }
                rows.push(Row {
                    kind,
                    old_no: change.old_index().map(|i| i + 1),
                    new_no: change.new_index().map(|i| i + 1),
                    segments,
                });
            }
        }
        total_rows += rows.len();
        if kept >= MAX_LINES {
            truncated = true;
            continue;
        }
        if kept + rows.len() > MAX_LINES {
            rows.truncate(MAX_LINES - kept);
            truncated = true;
        }
        kept += rows.len();
        let (old_start, old_len) = range(rows.iter().filter_map(|r| r.old_no));
        let (new_start, new_len) = range(rows.iter().filter_map(|r| r.new_no));
        hunks.push(Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            rows,
        });
    }
    DiffResult {
        kind: DiffKind::Modified,
        hunks,
        added,
        removed,
        total_rows,
        truncated,
        first_line,
    }
}

/// `(start, len)` of a run of line numbers; `(0, 0)` when empty, as in a
/// unified diff header for a side with no lines.
fn range(nos: impl Iterator<Item = usize>) -> (usize, usize) {
    let (mut first, mut count) = (None, 0);
    for n in nos {
        first.get_or_insert(n);
        count += 1;
    }
    (first.unwrap_or(0), count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn texts(r: &DiffResult) -> Vec<(RowKind, String)> {
        r.rows().map(|row| (row.kind, row.text())).collect()
    }

    #[test]
    fn modified_has_one_hunk_with_context() {
        let r = compute(&Before::Content(b"a\nb\nc\n".to_vec()), Some(b"a\nB\nc\n"));
        assert_eq!(r.kind, DiffKind::Modified);
        assert_eq!(r.hunks.len(), 1);
        assert_eq!(r.hunks[0].header(), "@@ -1,3 +1,3 @@");
        assert_eq!(
            texts(&r),
            vec![
                (RowKind::Equal, "a".into()),
                (RowKind::Delete, "b".into()),
                (RowKind::Insert, "B".into()),
                (RowKind::Equal, "c".into()),
            ]
        );
        let del = &r.hunks[0].rows[1];
        assert_eq!((del.old_no, del.new_no), (Some(2), None));
        let ins = &r.hunks[0].rows[2];
        assert_eq!((ins.old_no, ins.new_no), (None, Some(2)));
        assert_eq!((r.added, r.removed), (1, 1));
        assert!(!r.truncated);
        assert_eq!(r.first_line, "a");
    }

    #[test]
    fn inline_segments_mark_changed_words() {
        let r = compute(
            &Before::Content(b"hello world\n".to_vec()),
            Some(b"hello there\n"),
        );
        let rows = &r.hunks[0].rows;
        assert_eq!(
            rows[0].segments,
            vec![(false, "hello ".to_string()), (true, "world".to_string())]
        );
        assert_eq!(
            rows[1].segments,
            vec![(false, "hello ".to_string()), (true, "there".to_string())]
        );
    }

    #[test]
    fn created_deleted_missing() {
        let c = compute(&Before::Absent, Some(b"new\n"));
        assert_eq!(c.kind, DiffKind::Created);
        assert_eq!(c.hunks[0].header(), "@@ -0,0 +1,1 @@");
        assert_eq!(texts(&c), vec![(RowKind::Insert, "new".into())]);
        let d = compute(&Before::Content(b"gone\n".to_vec()), None);
        assert_eq!(d.kind, DiffKind::Deleted);
        assert_eq!(texts(&d), vec![(RowKind::Delete, "gone".into())]);
        assert_eq!(d.first_line, "gone");
        assert_eq!(compute(&Before::Absent, None).kind, DiffKind::Missing);
        assert_eq!(
            compute(&Before::Unavailable("x".into()), None).kind,
            DiffKind::Missing
        );
    }

    #[test]
    fn binary_detected_on_either_side() {
        let bin = vec![0u8, 1, 2];
        assert_eq!(
            compute(&Before::Content(bin.clone()), Some(b"txt")).kind,
            DiffKind::Binary
        );
        assert_eq!(
            compute(&Before::Content(b"txt".to_vec()), Some(&bin)).kind,
            DiffKind::Binary
        );
        assert_eq!(compute(&Before::Absent, Some(&bin)).kind, DiffKind::Binary);
    }

    #[test]
    fn truncates_at_max_lines_and_counts_the_full_diff() {
        let new: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let r = compute(&Before::Content(Vec::new()), Some(new.as_bytes()));
        assert!(r.truncated);
        assert_eq!(r.rows().count(), MAX_LINES);
        assert_eq!(r.total_rows, 1000);
        assert_eq!((r.added, r.removed), (1000, 0));
    }

    #[test]
    fn far_apart_changes_make_separate_hunks() {
        let old: String = (0..40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l3\n", "three\n").replace("l30\n", "thirty\n");
        let r = compute(&Before::Content(old.into_bytes()), Some(new.as_bytes()));
        assert_eq!(r.hunks.len(), 2);
        assert_eq!(r.hunks[0].header(), "@@ -1,7 +1,7 @@");
        assert_eq!(r.hunks[1].header(), "@@ -28,7 +28,7 @@");
    }

    #[test]
    fn identical_content_is_empty_modified() {
        let r = compute(&Before::Content(b"same\n".to_vec()), Some(b"same\n"));
        assert_eq!(r.kind, DiffKind::Modified);
        assert!(r.is_empty());
    }

    #[test]
    fn synthetic_and_unavailable() {
        let s = Before::Synthetic {
            old: "a\n".into(),
            new: "b\n".into(),
            note: "why".into(),
        };
        let r = compute(&s, Some(b"whatever"));
        assert_eq!(r.kind, DiffKind::Error("why".into()));
        assert_eq!(
            texts(&r),
            vec![(RowKind::Delete, "a".into()), (RowKind::Insert, "b".into())]
        );
        let u = compute(&Before::Unavailable("nope".into()), Some(b"x"));
        assert_eq!(u.kind, DiffKind::Error("nope".into()));
        assert!(u.is_empty());
    }

    #[test]
    fn crlf_and_missing_trailing_newline_are_stripped() {
        let r = compute(&Before::Content(b"a\r\nb".to_vec()), Some(b"a\r\nc"));
        assert_eq!(
            texts(&r),
            vec![
                (RowKind::Equal, "a".into()),
                (RowKind::Delete, "b".into()),
                (RowKind::Insert, "c".into()),
            ]
        );
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
