//! Turn a `CardInput` into styled terminal lines, via delta or a plain fallback.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::diff::{DiffKind, MAX_LINES};
use crate::session::CardInput;

#[derive(Debug, Clone)]
pub enum Renderer {
    Delta(PathBuf),
    Plain,
}

impl Renderer {
    /// Use delta when it is on PATH, unless disabled.
    pub fn detect(no_delta: bool) -> Self {
        if no_delta {
            return Renderer::Plain;
        }
        find_in_path("delta")
            .map(Renderer::Delta)
            .unwrap_or(Renderer::Plain)
    }

    pub fn is_delta(&self) -> bool {
        matches!(self, Renderer::Delta(_))
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// How a card lays out its diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Old and new text in two columns.
    #[default]
    SideBySide,
    /// One column of `-`/`+` lines.
    Unified,
}

/// Below this many columns side by side renders as unified.
pub const SPLIT_MIN_WIDTH: u16 = 100;

impl ViewMode {
    pub fn toggle(self) -> Self {
        match self {
            ViewMode::SideBySide => ViewMode::Unified,
            ViewMode::Unified => ViewMode::SideBySide,
        }
    }

    /// The mode actually drawn at `width`: side by side needs room for two
    /// columns, so a narrow terminal gets unified regardless of the choice.
    pub fn effective(self, width: u16) -> Self {
        match self {
            ViewMode::SideBySide if width >= SPLIT_MIN_WIDTH => ViewMode::SideBySide,
            _ => ViewMode::Unified,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ViewMode::SideBySide => "split",
            ViewMode::Unified => "unified",
        }
    }
}

/// Run delta over a unified diff and return its ANSI output.
///
/// stdin is fed from a thread. Writing it inline would deadlock on any diff
/// whose delta output fills the stdout pipe before we start reading: delta
/// blocks writing, we block writing, and the whole TUI stops responding.
pub fn delta_ansi(
    bin: &Path,
    unified: &str,
    side_by_side: bool,
    width: u16,
    cwd: &Path,
) -> Option<Vec<u8>> {
    let mut args = vec![
        "--paging=never".to_string(),
        "--max-line-length".into(),
        "0".into(),
        "--width".into(),
        width.max(20).to_string(),
    ];
    if side_by_side {
        args.push("--side-by-side".into());
    }
    let mut child = Command::new(bin)
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = unified.to_string();
    // A broken pipe here just means delta exited early; the exit status below
    // decides whether the output is usable.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
        let _ = stdin.flush();
    });
    let out = child.wait_with_output().ok();
    let _ = writer.join();
    let out = out?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

fn plain_lines(unified: &str) -> Vec<Line<'static>> {
    unified
        .lines()
        .map(|l| {
            let style = if l.starts_with("+++") || l.starts_with("---") {
                Style::new().bold()
            } else if l.starts_with("@@") {
                Style::new().fg(Color::Cyan)
            } else if l.starts_with('+') {
                Style::new().fg(Color::Green)
            } else if l.starts_with('-') {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Line::from(Span::styled(l.to_string(), style))
        })
        .collect()
}

/// One line of a unified diff, classified for the split layout.
#[derive(Debug, PartialEq, Eq)]
enum Row {
    FileHeader {
        old: String,
        new: String,
    },
    Hunk(String),
    Equal {
        old_no: usize,
        new_no: usize,
        text: String,
    },
    Delete {
        old_no: usize,
        text: String,
    },
    Insert {
        new_no: usize,
        text: String,
    },
}

/// `@@ -a[,n] +b[,m] @@` -> (a, b).
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let start = |s: &str| s.split(',').next()?.parse::<usize>().ok();
    Some((start(old)?, start(new)?))
}

/// Classify the lines similar emits. Every line carries a prefix character,
/// so the first byte decides; only the leading `---`/`+++` pair is special.
fn parse_unified(unified: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut lines = unified.lines().peekable();
    if let Some(old) = lines.peek().and_then(|l| l.strip_prefix("--- ")) {
        let old = old.to_string();
        lines.next();
        if let Some(new) = lines.peek().and_then(|l| l.strip_prefix("+++ ")) {
            rows.push(Row::FileHeader {
                old,
                new: new.to_string(),
            });
            lines.next();
        }
    }
    let (mut old_no, mut new_no) = (0, 0);
    for line in lines {
        if let Some((o, n)) = parse_hunk_header(line) {
            old_no = o;
            new_no = n;
            rows.push(Row::Hunk(line.to_string()));
            continue;
        }
        let text = line.get(1..).unwrap_or("").to_string();
        match line.as_bytes().first() {
            Some(b'-') => {
                rows.push(Row::Delete { old_no, text });
                old_no += 1;
            }
            Some(b'+') => {
                rows.push(Row::Insert { new_no, text });
                new_no += 1;
            }
            Some(b'\\') => {}
            _ => {
                rows.push(Row::Equal {
                    old_no,
                    new_no,
                    text,
                });
                old_no += 1;
                new_no += 1;
            }
        }
    }
    rows
}

const TAB_STOP: usize = 4;

fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = TAB_STOP - col % TAB_STOP;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += c.width().unwrap_or(0);
        }
    }
    out
}

/// `text` padded or cut to exactly `col` columns; a cut ends in `…`.
fn fit(text: &str, col: usize) -> String {
    let text = expand_tabs(text);
    let width = text.width();
    if width <= col {
        return format!("{text}{}", " ".repeat(col - width));
    }
    let keep = col.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(keep - used));
    if col > 0 {
        out.push('…');
    }
    out
}

/// Column widths for the split layout at `width`.
struct SplitLayout {
    num_w: usize,
    col: usize,
}

impl SplitLayout {
    fn new(rows: &[Row], width: u16) -> Self {
        let max_no = rows
            .iter()
            .map(|r| match r {
                Row::Equal { old_no, new_no, .. } => (*old_no).max(*new_no),
                Row::Delete { old_no, .. } => *old_no,
                Row::Insert { new_no, .. } => *new_no,
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        let num_w = max_no.to_string().len().max(3);
        // Each side is `num_w + 1 + 1 + col`; the separator is 3 wide.
        let col = (width as usize).saturating_sub(2 * num_w + 7) / 2;
        Self {
            num_w,
            col: col.max(1),
        }
    }

    fn side_w(&self) -> usize {
        self.num_w + 2 + self.col
    }

    fn separator(&self) -> Span<'static> {
        Span::styled(" │ ", Style::new().dim())
    }

    fn blank(&self) -> Span<'static> {
        Span::raw(" ".repeat(self.side_w()))
    }

    /// `{no:>num_w} {marker}{text}` as two spans: a dim number and the text.
    fn cell(&self, no: usize, marker: char, text: &str, style: Style) -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{no:>w$} ", w = self.num_w), style.dim()),
            Span::styled(format!("{marker}{}", fit(text, self.col)), style),
        ]
    }

    fn row(&self, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
        let mut spans = left;
        spans.push(self.separator());
        spans.extend(right);
        Line::from(spans)
    }
}

/// Two columns: old text on the left, new on the right. Deleted and inserted
/// runs are paired line for line; the longer run faces blank cells.
fn split_lines(unified: &str, width: u16) -> Vec<Line<'static>> {
    let rows = parse_unified(unified);
    let layout = SplitLayout::new(&rows, width);
    let red = Style::new().fg(Color::Red);
    let green = Style::new().fg(Color::Green);
    let mut lines = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        match &rows[i] {
            Row::FileHeader { old, new } => {
                let bold = Style::new().bold();
                let w = layout.side_w();
                lines.push(layout.row(
                    vec![Span::styled(fit(&format!("--- {old}"), w), bold)],
                    vec![Span::styled(fit(&format!("+++ {new}"), w), bold)],
                ));
                i += 1;
            }
            Row::Hunk(h) => {
                lines.push(Line::from(Span::styled(
                    h.clone(),
                    Style::new().fg(Color::Cyan),
                )));
                i += 1;
            }
            Row::Equal {
                old_no,
                new_no,
                text,
            } => {
                let plain = Style::new();
                lines.push(layout.row(
                    layout.cell(*old_no, ' ', text, plain),
                    layout.cell(*new_no, ' ', text, plain),
                ));
                i += 1;
            }
            Row::Delete { .. } | Row::Insert { .. } => {
                let deletes: Vec<_> = rows[i..]
                    .iter()
                    .take_while(|r| matches!(r, Row::Delete { .. }))
                    .collect();
                let inserts: Vec<_> = rows[i + deletes.len()..]
                    .iter()
                    .take_while(|r| matches!(r, Row::Insert { .. }))
                    .collect();
                let n = deletes.len().max(inserts.len());
                for k in 0..n {
                    let left = match deletes.get(k) {
                        Some(Row::Delete { old_no, text }) => layout.cell(*old_no, '-', text, red),
                        _ => vec![layout.blank()],
                    };
                    let right = match inserts.get(k) {
                        Some(Row::Insert { new_no, text }) => {
                            layout.cell(*new_no, '+', text, green)
                        }
                        _ => vec![layout.blank()],
                    };
                    lines.push(layout.row(left, right));
                }
                i += deletes.len() + inserts.len();
            }
        }
    }
    lines
}

fn notice(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::new().dim().italic()))
}

pub fn format_time(ts_ms: u64) -> String {
    if ts_ms == 0 {
        return "--:--:--".into();
    }
    chrono::DateTime::from_timestamp_millis(ts_ms as i64)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "--:--:--".into())
}

/// Colors for session tags; a session keeps its color across cards.
const SESSION_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::LightRed,
];

/// The last six characters of a session id.
pub fn session_suffix(id: &str) -> String {
    let n = id.chars().count();
    id.chars().skip(n.saturating_sub(6)).collect()
}

/// Deterministic color for a session id. `DefaultHasher` is not guaranteed
/// stable across releases, so fold the bytes by hand.
pub fn session_color(id: &str) -> Color {
    let h = id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    SESSION_COLORS[h as usize % SESSION_COLORS.len()]
}

#[derive(Debug, Clone)]
pub struct EditCard {
    pub input: CardInput,
    pub lines: Vec<Line<'static>>,
}

impl EditCard {
    pub fn new(input: CardInput, renderer: &Renderer, mode: ViewMode, width: u16) -> Self {
        let lines = body_lines(&input, renderer, mode, width);
        Self { input, lines }
    }

    pub fn rerender(&mut self, renderer: &Renderer, mode: ViewMode, width: u16) {
        self.lines = body_lines(&self.input, renderer, mode, width);
    }

    /// `<worktree>:<suffix>` or `<suffix>`; None without a session id.
    pub fn tag(&self) -> Option<String> {
        let suffix = session_suffix(self.input.session_id.as_deref()?);
        Some(match &self.input.worktree {
            Some(wt) => format!("{wt}:{suffix}"),
            None => suffix,
        })
    }

    /// Header without the timestamp; stable across machines for tests.
    pub fn summary(&self, tags: bool) -> String {
        self.header_string(false, tags)
    }

    pub fn header_text(&self, tags: bool) -> String {
        self.header_string(true, tags)
    }

    fn header_parts(&self, with_time: bool) -> Vec<String> {
        let mut parts = vec![self.input.path.clone(), self.input.tool.clone()];
        if with_time {
            parts.push(format_time(self.input.ts));
        }
        parts.push(self.input.diff.kind.label());
        if let Some(a) = &self.input.agent {
            parts.push(format!("[{a}]"));
        }
        if self.input.user_modified {
            parts.push("(user modified)".into());
        }
        parts
    }

    fn header_string(&self, with_time: bool, tags: bool) -> String {
        let mut parts = self.header_parts(with_time);
        if tags && let Some(tag) = self.tag() {
            parts.insert(0, tag);
        }
        parts.join(" · ")
    }

    /// One reversed bar padded to `width`; the session tag, when shown, is a
    /// colored badge at its start.
    pub fn header_line(&self, width: u16, tags: bool) -> Line<'static> {
        let base = Style::new().bold().reversed();
        let rest = self.header_parts(true).join(" · ");
        let spans = match (tags, self.tag(), &self.input.session_id) {
            (true, Some(tag), Some(id)) => vec![
                Span::styled("── ", base),
                Span::styled(tag, base.fg(session_color(id))),
                Span::styled(format!(" · {rest} "), base),
            ],
            _ => vec![Span::styled(format!("── {rest} "), base)],
        };
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let fill = (width as usize).saturating_sub(used);
        let mut spans = spans;
        spans.push(Span::styled("─".repeat(fill), base));
        Line::from(spans)
    }

    /// Body with styling stripped, one string per line.
    pub fn body_plain(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }
}

fn body_lines(
    input: &CardInput,
    renderer: &Renderer,
    mode: ViewMode,
    width: u16,
) -> Vec<Line<'static>> {
    let d = &input.diff;
    let mode = mode.effective(width);
    let plain = |unified: &str| match mode {
        ViewMode::SideBySide => split_lines(unified, width),
        ViewMode::Unified => plain_lines(unified),
    };
    let mut lines = match &d.kind {
        DiffKind::Binary => return vec![notice("(binary file)")],
        DiffKind::Missing => return vec![notice("(file missing after edit)")],
        DiffKind::Error(m) if d.unified.is_empty() => {
            return vec![notice(format!("(no diff: {m})"))];
        }
        // An empty diff on a created or deleted file means the file itself is
        // empty; "(no changes)" next to a "Created" header reads as a bug.
        DiffKind::Created if d.unified.is_empty() => return vec![notice("(empty file created)")],
        DiffKind::Deleted if d.unified.is_empty() => return vec![notice("(empty file deleted)")],
        _ if d.unified.is_empty() => return vec![notice("(no changes)")],
        _ => match renderer {
            Renderer::Delta(bin) => delta_ansi(
                bin,
                &d.unified,
                mode == ViewMode::SideBySide,
                width,
                &input.root,
            )
            .and_then(|ansi| ansi.into_text().ok())
            .map(|t| t.lines)
            .unwrap_or_else(|| plain(&d.unified)),
            Renderer::Plain => plain(&d.unified),
        },
    };
    while lines.last().map(|l| l.width() == 0).unwrap_or(false) {
        lines.pop();
    }
    if d.truncated {
        lines.push(notice(format!(
            "... {} more lines",
            d.total_lines.saturating_sub(MAX_LINES)
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{self, DiffKind};
    use crate::snapshot::Before;

    fn input(kind_before: &Before, after: Option<&[u8]>) -> CardInput {
        CardInput {
            path: "f.txt".into(),
            file: PathBuf::from("/p/f.txt"),
            tool: "Edit".into(),
            ts: 0,
            agent: Some("Explore".into()),
            diff: diff::compute(kind_before, after, "f.txt"),
            user_modified: true,
            session_id: Some("sess-ab12cd".into()),
            worktree: None,
            root: PathBuf::from("/p"),
        }
    }

    #[test]
    fn plain_render_and_summary() {
        let card = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(
            card.summary(false),
            "f.txt · Edit · Modified · [Explore] · (user modified)"
        );
        let body = card.body_plain();
        assert_eq!(body[0], "--- a/f.txt");
        assert!(body.contains(&"-a".to_string()));
        assert!(body.contains(&"+b".to_string()));
        assert!(card.header_text(false).contains("--:--:--"));
    }

    #[test]
    fn notices_for_binary_missing_and_truncation() {
        let bin = EditCard::new(
            input(&Before::Content(vec![0, 1]), Some(b"x")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(bin.body_plain(), ["(binary file)"]);
        let missing = EditCard::new(
            input(&Before::Absent, None),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(missing.body_plain(), ["(file missing after edit)"]);
        let big: String = (0..800).map(|i| format!("{i}\n")).collect();
        let t = EditCard::new(
            input(&Before::Absent, Some(big.as_bytes())),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(t.input.diff.kind, DiffKind::Created);
        assert_eq!(t.body_plain().last().unwrap(), "... 303 more lines");
    }

    #[test]
    fn empty_created_and_deleted_files_say_so() {
        let created = EditCard::new(
            input(&Before::Absent, Some(b"")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(created.input.diff.kind, DiffKind::Created);
        assert_eq!(created.body_plain(), ["(empty file created)"]);
        let deleted = EditCard::new(
            input(&Before::Content(Vec::new()), None),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(deleted.input.diff.kind, DiffKind::Deleted);
        assert_eq!(deleted.body_plain(), ["(empty file deleted)"]);
        let same = EditCard::new(
            input(&Before::Content(b"x\n".to_vec()), Some(b"x\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(same.body_plain(), ["(no changes)"]);
    }

    #[test]
    fn summary_is_header_without_the_time() {
        let card = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(
            card.summary(false),
            "f.txt · Edit · Modified · [Explore] · (user modified)"
        );
        assert_eq!(
            card.header_text(false),
            "f.txt · Edit · --:--:-- · Modified · [Explore] · (user modified)"
        );
    }

    #[test]
    fn header_line_pads_to_width() {
        let card = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        let l = card.header_line(100, false);
        assert_eq!(l.width(), 100);
        assert_eq!(l.spans.len(), 2);
        let tagged = card.header_line(100, true);
        assert_eq!(tagged.width(), 100);
        assert!(tagged.spans.iter().any(|s| s.content == "ab12cd"));
    }

    #[test]
    fn tag_prepended_when_requested() {
        let mut card = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(
            card.summary(true),
            "ab12cd · f.txt · Edit · Modified · [Explore] · (user modified)"
        );
        card.input.worktree = Some("feat-x".into());
        assert_eq!(card.tag().as_deref(), Some("feat-x:ab12cd"));
        assert!(card.summary(true).starts_with("feat-x:ab12cd · f.txt"));
        card.input.session_id = None;
        assert_eq!(card.tag(), None);
        assert_eq!(card.summary(true), card.summary(false));
    }

    fn split(before: &[u8], after: &[u8], width: u16) -> EditCard {
        EditCard::new(
            input(&Before::Content(before.to_vec()), Some(after)),
            &Renderer::Plain,
            ViewMode::SideBySide,
            width,
        )
    }

    #[test]
    fn view_mode_effective_threshold() {
        assert_eq!(ViewMode::SideBySide.effective(99), ViewMode::Unified);
        assert_eq!(ViewMode::SideBySide.effective(100), ViewMode::SideBySide);
        assert_eq!(ViewMode::Unified.effective(500), ViewMode::Unified);
        assert_eq!(ViewMode::SideBySide.toggle().toggle(), ViewMode::SideBySide);
        assert_eq!(ViewMode::default(), ViewMode::SideBySide);
    }

    #[test]
    fn parse_hunk_header_variants() {
        assert_eq!(parse_hunk_header("@@ -1,3 +1,3 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1 @@"), Some((0, 1)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -7,2 +9,4 @@ fn x()"), Some((7, 9)));
        assert_eq!(parse_hunk_header("@@ nope"), None);
        assert_eq!(parse_hunk_header("-@@ -1 +1 @@"), None);
    }

    #[test]
    fn parse_unified_numbers_lines_and_skips_newline_hints() {
        let rows = parse_unified(
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n\\ No newline at end of file\n",
        );
        assert_eq!(
            rows,
            vec![
                Row::FileHeader {
                    old: "a/f".into(),
                    new: "b/f".into()
                },
                Row::Hunk("@@ -1,2 +1,2 @@".into()),
                Row::Equal {
                    old_no: 1,
                    new_no: 1,
                    text: "a".into()
                },
                Row::Delete {
                    old_no: 2,
                    text: "b".into()
                },
                Row::Insert {
                    new_no: 2,
                    text: "B".into()
                },
            ]
        );
    }

    #[test]
    fn split_pairs_deletes_with_inserts() {
        let card = split(b"a\nb\nc\n", b"a\nB\nc\n", 120);
        let body = card.body_plain();
        assert!(body[0].starts_with("--- a/f.txt"), "{}", body[0]);
        assert!(body[0].contains("+++ b/f.txt"), "{}", body[0]);
        assert!(body[1].starts_with("@@ "), "{}", body[1]);
        assert!(
            body[2].contains("  1  a") && body[2].contains("│   1  a"),
            "{}",
            body[2]
        );
        assert!(
            body[3].contains("  2 -b") && body[3].contains("│   2 +B"),
            "{}",
            body[3]
        );
        let data_w = card.lines[2].width();
        for l in &card.lines[2..] {
            assert!(l.width() <= 120);
            assert_eq!(l.width(), data_w, "{l}");
        }
        assert_eq!(card.lines[0].width(), data_w);
    }

    #[test]
    fn split_unbalanced_run_leaves_blank_side() {
        let card = split(b"a\nb\nc\n", b"x\n", 120);
        let body = card.body_plain();
        let rows: Vec<_> = body[2..]
            .iter()
            .map(|l| l.split('│').collect::<Vec<_>>())
            .collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0][0].contains("-a") && rows[0][1].contains("+x"));
        assert!(rows[1][0].contains("-b") && rows[1][1].trim().is_empty());
        assert!(rows[2][0].contains("-c") && rows[2][1].trim().is_empty());
    }

    #[test]
    fn split_fits_wide_text_with_ellipsis() {
        let long = format!("{}日本語\n", "x".repeat(200));
        let card = split(b"short\n", long.as_bytes(), 100);
        let w = card.lines[2].width();
        for l in &card.lines[2..] {
            assert!(l.width() <= 100);
            assert_eq!(l.width(), w, "{l}");
        }
        assert!(card.body_plain()[2].contains('…'));
        assert_eq!(fit("日本語", 4), "日 …");
        assert_eq!(fit("日本語", 5), "日本…");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 0), "");
    }

    #[test]
    fn split_expands_tabs() {
        assert_eq!(expand_tabs("a\tb"), "a   b");
        assert_eq!(expand_tabs("\ta"), "    a");
        assert_eq!(expand_tabs("abcd\tb"), "abcd    b");
        assert!(fit("\ta", 8).starts_with("    a"));
    }

    #[test]
    fn split_falls_back_below_threshold() {
        let narrow = split(b"a\n", b"b\n", 80);
        let unified = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
            ViewMode::Unified,
            80,
        );
        assert_eq!(narrow.body_plain(), unified.body_plain());
        assert!(!narrow.body_plain().iter().any(|l| l.contains('│')));
        let wide = split(b"a\n", b"b\n", 100);
        assert!(wide.body_plain().iter().any(|l| l.contains('│')));
    }

    #[test]
    fn split_notices_match_unified() {
        let big: String = (0..800).map(|i| format!("{i}\n")).collect();
        let t = EditCard::new(
            input(&Before::Absent, Some(big.as_bytes())),
            &Renderer::Plain,
            ViewMode::SideBySide,
            120,
        );
        let body = t.body_plain();
        // file header + hunk + 497 inserts + notice
        assert_eq!(body.len(), 500);
        assert_eq!(body.last().unwrap(), "... 303 more lines");
        assert!(body[0].starts_with("--- /dev/null"), "{}", body[0]);
        for (before, after, expect) in [
            (
                Before::Content(vec![0, 1]),
                Some(&b"x"[..]),
                "(binary file)",
            ),
            (Before::Absent, None, "(file missing after edit)"),
            (Before::Absent, Some(&b""[..]), "(empty file created)"),
            (
                Before::Content(b"x\n".to_vec()),
                Some(&b"x\n"[..]),
                "(no changes)",
            ),
        ] {
            let card = EditCard::new(
                input(&before, after),
                &Renderer::Plain,
                ViewMode::SideBySide,
                120,
            );
            assert_eq!(card.body_plain(), [expect]);
        }
    }

    #[test]
    fn session_color_is_deterministic_and_in_palette() {
        let c = session_color("sess-ab12cd");
        assert_eq!(c, session_color("sess-ab12cd"));
        assert!(SESSION_COLORS.contains(&c));
        assert_eq!(session_suffix("ab"), "ab");
        assert_eq!(session_suffix("sess-ab12cd"), "ab12cd");
    }
}
