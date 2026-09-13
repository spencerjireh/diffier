//! Turn a `CardInput` into styled terminal lines.
//!
//! Rendering has two stages. `paint` runs once per card: it merges syntax
//! colors from syntect with the word-level marks from the diff. `layout` runs
//! whenever the width, view mode, or wrap setting changes and only arranges
//! painted rows into lines.

use std::sync::LazyLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::diff::{DiffKind, DiffResult, Hunk, MAX_LINES, Row, RowKind};
use crate::session::CardInput;

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

/// Everything `layout` needs besides the painted rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOpts {
    pub mode: ViewMode,
    pub width: u16,
    pub wrap: bool,
}

// Dark-terminal palette, after delta's defaults.
pub const REMOVED_BG: Color = Color::Rgb(0x3f, 0x00, 0x01);
pub const ADDED_BG: Color = Color::Rgb(0x00, 0x28, 0x00);
pub const REMOVED_EMPH_BG: Color = Color::Rgb(0x90, 0x10, 0x11);
pub const ADDED_EMPH_BG: Color = Color::Rgb(0x00, 0x60, 0x00);
pub const GUTTER_FG: Color = Color::Rgb(0x6c, 0x6c, 0x6c);
const TAB_STOP: usize = 4;

static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut themes = ThemeSet::load_defaults().themes;
    themes
        .remove("base16-ocean.dark")
        .expect("syntect ships base16-ocean.dark")
});

/// A run of text with one color and one emphasis state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub fg: Option<Color>,
    pub emph: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaintedRow {
    pub kind: RowKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub segs: Vec<Seg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaintedHunk {
    pub header: String,
    pub rows: Vec<PaintedRow>,
}

/// Syntax for the edited file: by extension, then by file name (Makefile,
/// Dockerfile), then by the first line (shebang, mode line). Plain text is
/// reported as `None` so the terminal's own foreground is used.
fn detect_syntax(input: &CardInput) -> Option<&'static SyntaxReference> {
    let ss = &*SYNTAX;
    let by_ext = input
        .file
        .extension()
        .and_then(|e| ss.find_syntax_by_extension(&e.to_string_lossy()));
    let by_name = || {
        input
            .file
            .file_name()
            .and_then(|n| ss.find_syntax_by_extension(&n.to_string_lossy()))
    };
    let by_line = || ss.find_syntax_by_first_line(&input.diff.first_line);
    by_ext
        .or_else(by_name)
        .or_else(by_line)
        .filter(|s| s.name != "Plain Text")
}

fn to_color(c: syntect::highlighting::Color) -> Option<Color> {
    (c.a != 0).then_some(Color::Rgb(c.r, c.g, c.b))
}

/// Color `row` with `hl` (when any) and cut it at the diff's word marks.
fn paint_row(row: &Row, hl: Option<&mut HighlightLines<'_>>) -> PaintedRow {
    let text = row.text();
    // Syntax runs as (end byte offset, color).
    let colored: Vec<(usize, Option<Color>)> = match hl {
        Some(hl) => {
            let mut runs = Vec::new();
            let mut end = 0;
            let line = format!("{text}\n");
            match hl.highlight_line(&line, &SYNTAX) {
                Ok(spans) => {
                    for (style, piece) in spans {
                        end += piece.len();
                        runs.push((end.min(text.len()), to_color(style.foreground)));
                    }
                }
                Err(_) => runs.push((text.len(), None)),
            }
            runs
        }
        None => vec![(text.len(), None)],
    };
    let mut marks = Vec::new();
    let mut end = 0;
    for (emph, s) in &row.segments {
        end += s.len();
        marks.push((end, *emph));
    }
    let mut segs: Vec<Seg> = Vec::new();
    let (mut ci, mut mi, mut pos) = (0, 0, 0);
    while pos < text.len() {
        while ci < colored.len() && colored[ci].0 <= pos {
            ci += 1;
        }
        while mi < marks.len() && marks[mi].0 <= pos {
            mi += 1;
        }
        let cend = colored.get(ci).map(|c| c.0).unwrap_or(text.len());
        let mend = marks.get(mi).map(|m| m.0).unwrap_or(text.len());
        let next = cend.min(mend).max(pos + 1);
        let next = if text.is_char_boundary(next) {
            next
        } else {
            (next..=text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len())
        };
        let fg = colored.get(ci).and_then(|c| c.1);
        let emph = marks.get(mi).map(|m| m.1).unwrap_or(false);
        match segs.last_mut() {
            Some(last) if last.fg == fg && last.emph == emph => {
                last.text.push_str(&text[pos..next])
            }
            _ => segs.push(Seg {
                text: text[pos..next].to_string(),
                fg,
                emph,
            }),
        }
        pos = next;
    }
    PaintedRow {
        kind: row.kind,
        old_no: row.old_no,
        new_no: row.new_no,
        segs,
    }
}

/// Stage one: syntax colors plus word marks, once per card. One highlighter
/// walks the old side and one the new side so multi-line constructs keep
/// their state across rows.
pub fn paint(input: &CardInput) -> Vec<PaintedHunk> {
    let syntax = detect_syntax(input);
    let mut old_hl = syntax.map(|s| HighlightLines::new(s, &THEME));
    let mut new_hl = syntax.map(|s| HighlightLines::new(s, &THEME));
    input
        .diff
        .hunks
        .iter()
        .map(|h: &Hunk| PaintedHunk {
            header: h.header(),
            rows: h
                .rows
                .iter()
                .map(|row| match row.kind {
                    RowKind::Delete => paint_row(row, old_hl.as_mut()),
                    RowKind::Insert => paint_row(row, new_hl.as_mut()),
                    RowKind::Equal => {
                        // Keep the old-side highlighter in step without
                        // spending a second merge on the same text.
                        if let Some(hl) = old_hl.as_mut() {
                            let _ = hl.highlight_line(&format!("{}\n", row.text()), &SYNTAX);
                        }
                        paint_row(row, new_hl.as_mut())
                    }
                })
                .collect(),
        })
        .collect()
}

/// `segs` cut into visual lines no wider than `col`, tabs expanded. Without
/// `wrap` there is exactly one line and an overflow ends in `…`.
fn chunks(segs: &[Seg], col: usize, wrap: bool) -> Vec<Vec<Seg>> {
    let mut lines: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut used = 0;
    let mut logical = 0;
    let limit = if wrap { col } else { col.saturating_sub(1) };
    let mut overflow = false;
    'outer: for seg in segs {
        for c in seg.text.chars() {
            let piece: String = if c == '\t' {
                let n = TAB_STOP - logical % TAB_STOP;
                " ".repeat(n)
            } else {
                c.to_string()
            };
            let w = piece.width();
            logical += w;
            if used + w > limit {
                if !wrap {
                    overflow = true;
                    break 'outer;
                }
                lines.push(Vec::new());
                used = 0;
            }
            used += w;
            let line = lines.last_mut().expect("at least one line");
            match line.last_mut() {
                Some(last) if last.fg == seg.fg && last.emph == seg.emph => {
                    last.text.push_str(&piece)
                }
                _ => line.push(Seg {
                    text: piece,
                    fg: seg.fg,
                    emph: seg.emph,
                }),
            }
        }
    }
    if !wrap {
        let total: usize = segs.iter().map(|s| s.text.width()).sum();
        if overflow || total > col {
            let line = lines.last_mut().expect("at least one line");
            let pad = limit.saturating_sub(used);
            line.push(Seg {
                text: format!("{}…", " ".repeat(pad)),
                fg: Some(GUTTER_FG),
                emph: false,
            });
        }
    }
    lines
}

fn line_width(segs: &[Seg]) -> usize {
    segs.iter().map(|s| s.text.width()).sum()
}

struct RowStyle {
    bg: Option<Color>,
    emph_bg: Option<Color>,
    marker: char,
}

impl RowStyle {
    fn for_kind(kind: RowKind) -> Self {
        match kind {
            RowKind::Equal => Self {
                bg: None,
                emph_bg: None,
                marker: ' ',
            },
            RowKind::Delete => Self {
                bg: Some(REMOVED_BG),
                emph_bg: Some(REMOVED_EMPH_BG),
                marker: '-',
            },
            RowKind::Insert => Self {
                bg: Some(ADDED_BG),
                emph_bg: Some(ADDED_EMPH_BG),
                marker: '+',
            },
        }
    }

    fn base(&self) -> Style {
        let s = Style::new();
        match self.bg {
            Some(bg) => s.bg(bg),
            None => s,
        }
    }

    fn seg_style(&self, seg: &Seg) -> Style {
        let mut s = self.base();
        if let Some(fg) = seg.fg {
            s = s.fg(fg);
        }
        if seg.emph
            && let Some(bg) = self.emph_bg
        {
            s = s.bg(bg);
        }
        s
    }
}

/// Column widths for one layout pass.
struct Geometry {
    width: usize,
    num_w: usize,
    /// Text columns per side (split) or for the line (unified).
    col: usize,
}

impl Geometry {
    fn new(hunks: &[PaintedHunk], opts: &LayoutOpts) -> Self {
        let max_no = hunks
            .iter()
            .flat_map(|h| h.rows.iter())
            .map(|r| r.old_no.unwrap_or(0).max(r.new_no.unwrap_or(0)))
            .max()
            .unwrap_or(0);
        let num_w = max_no.to_string().len().max(3);
        let width = opts.width as usize;
        let col = match opts.mode.effective(opts.width) {
            // Each side is `num_w + 1 + 1 + col`; the separator is 3 wide.
            ViewMode::SideBySide => width.saturating_sub(2 * num_w + 7) / 2,
            // `old new marker text`.
            ViewMode::Unified => width.saturating_sub(2 * num_w + 3),
        };
        Self {
            width,
            num_w,
            col: col.max(1),
        }
    }

    fn side_w(&self) -> usize {
        self.num_w + 2 + self.col
    }

    fn separator(&self) -> Span<'static> {
        Span::styled(" │ ", Style::new().fg(GUTTER_FG))
    }

    fn blank_side(&self) -> Vec<Span<'static>> {
        vec![Span::raw(" ".repeat(self.side_w()))]
    }

    fn gutter(&self, no: Option<usize>, style: Style) -> Span<'static> {
        let text = match no {
            Some(n) => format!("{n:>w$} ", w = self.num_w),
            None => " ".repeat(self.num_w + 1),
        };
        Span::styled(text, style.fg(GUTTER_FG))
    }

    /// `gutter marker text padding`, one visual line of one side.
    fn cell(
        &self,
        gutters: &[Option<usize>],
        first: bool,
        line: &[Seg],
        rs: &RowStyle,
    ) -> Vec<Span<'static>> {
        let base = rs.base();
        let mut spans = Vec::with_capacity(line.len() + 3);
        for no in gutters {
            spans.push(self.gutter(if first { *no } else { None }, base));
        }
        let marker = if first { rs.marker } else { ' ' };
        spans.push(Span::styled(marker.to_string(), base));
        for seg in line {
            spans.push(Span::styled(seg.text.clone(), rs.seg_style(seg)));
        }
        let pad = self.col.saturating_sub(line_width(line));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), base));
        }
        spans
    }

    fn hunk_rule(&self, header: &str) -> Line<'static> {
        let lead = format!("── {header} ");
        let fill = self.width.saturating_sub(lead.width());
        Line::from(Span::styled(
            format!("{lead}{}", "─".repeat(fill)),
            Style::new().fg(GUTTER_FG),
        ))
    }

    fn unified_row(&self, row: &PaintedRow, wrap: bool) -> Vec<Line<'static>> {
        let rs = RowStyle::for_kind(row.kind);
        chunks(&row.segs, self.col, wrap)
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let mut spans = self.cell(&[row.old_no, row.new_no], i == 0, line, &rs);
                if rs.bg.is_none() {
                    // Context rows have no tint to extend; drop the padding
                    // so plain output has no trailing spaces.
                    spans.pop();
                }
                Line::from(spans)
            })
            .collect()
    }

    fn split_row(
        &self,
        left: Option<&PaintedRow>,
        right: Option<&PaintedRow>,
        wrap: bool,
    ) -> Vec<Line<'static>> {
        struct Side {
            rs: RowStyle,
            lines: Vec<Vec<Seg>>,
            no: Option<usize>,
        }
        let side = |row: Option<&PaintedRow>, old: bool| {
            row.map(|r| Side {
                rs: RowStyle::for_kind(r.kind),
                lines: chunks(&r.segs, self.col, wrap),
                no: if old { r.old_no } else { r.new_no },
            })
        };
        let l = side(left, true);
        let r = side(right, false);
        let count = |s: &Option<Side>| s.as_ref().map(|s| s.lines.len()).unwrap_or(1);
        let n = count(&l).max(count(&r));
        (0..n)
            .map(|i| {
                let render = |s: &Option<Side>| match s {
                    Some(s) => match s.lines.get(i) {
                        Some(line) => self.cell(&[s.no], i == 0, line, &s.rs),
                        None => vec![Span::styled(" ".repeat(self.side_w()), s.rs.base())],
                    },
                    None => self.blank_side(),
                };
                let mut spans = render(&l);
                spans.push(self.separator());
                spans.extend(render(&r));
                Line::from(spans)
            })
            .collect()
    }
}

/// Stage two: painted hunks arranged for `opts`.
pub fn layout(hunks: &[PaintedHunk], opts: &LayoutOpts) -> Vec<Line<'static>> {
    let geo = Geometry::new(hunks, opts);
    let mode = opts.mode.effective(opts.width);
    let mut lines = Vec::new();
    for hunk in hunks {
        lines.push(geo.hunk_rule(&hunk.header));
        match mode {
            ViewMode::Unified => {
                for row in &hunk.rows {
                    lines.extend(geo.unified_row(row, opts.wrap));
                }
            }
            ViewMode::SideBySide => {
                let rows = &hunk.rows;
                let mut i = 0;
                while i < rows.len() {
                    match rows[i].kind {
                        RowKind::Equal => {
                            lines.extend(geo.split_row(Some(&rows[i]), Some(&rows[i]), opts.wrap));
                            i += 1;
                        }
                        _ => {
                            // A run of deletes followed by a run of inserts is
                            // paired line for line; the longer run faces blanks.
                            let dels = rows[i..]
                                .iter()
                                .take_while(|r| r.kind == RowKind::Delete)
                                .count();
                            let ins = rows[i + dels..]
                                .iter()
                                .take_while(|r| r.kind == RowKind::Insert)
                                .count();
                            for k in 0..dels.max(ins) {
                                let left = (k < dels).then(|| &rows[i + k]);
                                let right = (k < ins).then(|| &rows[i + dels + k]);
                                lines.extend(geo.split_row(left, right, opts.wrap));
                            }
                            i += dels + ins;
                        }
                    }
                }
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

fn kind_color(kind: &DiffKind) -> Option<Color> {
    match kind {
        DiffKind::Created => Some(Color::Green),
        DiffKind::Deleted => Some(Color::Red),
        DiffKind::Error(_) | DiffKind::Missing => Some(Color::Yellow),
        DiffKind::Modified | DiffKind::Binary => None,
    }
}

/// `+N -N`, or None when nothing was added or removed.
fn stats(d: &DiffResult) -> Option<String> {
    (d.added + d.removed > 0).then(|| format!("+{} -{}", d.added, d.removed))
}

/// `s` cut from the left to `max` columns, marked with `…`.
pub fn elide_left(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut tail = String::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        tail.insert(0, c);
        used += w;
    }
    format!("…{tail}")
}

#[derive(Debug, Clone)]
pub struct EditCard {
    pub input: CardInput,
    pub painted: Vec<PaintedHunk>,
    pub lines: Vec<Line<'static>>,
    pub collapsed: bool,
}

impl EditCard {
    pub fn new(input: CardInput, opts: &LayoutOpts) -> Self {
        let painted = paint(&input);
        let lines = body_lines(&input, &painted, opts);
        Self {
            input,
            painted,
            lines,
            collapsed: false,
        }
    }

    pub fn rerender(&mut self, opts: &LayoutOpts) {
        self.lines = body_lines(&self.input, &self.painted, opts);
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

    /// Everything after the path: tool, [time], kind, [stats], [agent], [note].
    fn trailing_parts(&self, with_time: bool) -> Vec<String> {
        let mut parts = vec![self.input.tool.clone()];
        if with_time {
            parts.push(format_time(self.input.ts));
        }
        parts.push(self.input.diff.kind.label());
        if let Some(s) = stats(&self.input.diff) {
            parts.push(s);
        }
        if let Some(a) = &self.input.agent {
            parts.push(format!("[{a}]"));
        }
        if self.input.user_modified {
            parts.push("(user modified)".into());
        }
        parts
    }

    fn header_string(&self, with_time: bool, tags: bool) -> String {
        let mut parts = vec![self.input.path.clone()];
        parts.extend(self.trailing_parts(with_time));
        if tags && let Some(tag) = self.tag() {
            parts.insert(0, tag);
        }
        parts.join(" · ")
    }

    /// One reversed bar padded to `width`: `── tag · path · tool · kind ·
    /// +N -N · [agent] ────── HH:MM:SS ──`. The tag and the kind are colored
    /// badges; a path that does not fit is cut from the left.
    pub fn header_line(&self, width: u16, tags: bool) -> Line<'static> {
        let base = Style::new().bold().reversed();
        let width = width as usize;
        let kind = self.input.diff.kind.label();
        let kind_style = match kind_color(&self.input.diff.kind) {
            Some(c) => base.fg(c),
            None => base,
        };
        let mut after_kind: Vec<String> = Vec::new();
        if let Some(s) = stats(&self.input.diff) {
            after_kind.push(s);
        }
        if let Some(a) = &self.input.agent {
            after_kind.push(format!("[{a}]"));
        }
        if self.input.user_modified {
            after_kind.push("(user modified)".into());
        }
        let after_kind: String = after_kind.iter().map(|p| format!(" · {p}")).collect();
        let right = format!(" {} ──", format_time(self.input.ts));

        let mut lead = vec![Span::styled("── ", base)];
        if tags && let (Some(tag), Some(id)) = (self.tag(), &self.input.session_id) {
            lead.push(Span::styled(tag, base.fg(session_color(id))));
            lead.push(Span::styled(" · ", base));
        }
        let fixed: usize = lead.iter().map(|s| s.content.width()).sum::<usize>()
            + format!(" · {} · ", self.input.tool).width()
            + kind.width()
            + after_kind.width()
            + 1
            + right.width();
        let path_max = width.saturating_sub(fixed);
        let path = elide_left(&self.input.path, path_max.max(4));

        let mut spans = lead;
        spans.push(Span::styled(path, base));
        spans.push(Span::styled(format!(" · {} · ", self.input.tool), base));
        spans.push(Span::styled(kind, kind_style));
        spans.push(Span::styled(format!("{after_kind} "), base));
        let used: usize = spans.iter().map(|s| s.content.width()).sum::<usize>() + right.width();
        let fill = width.saturating_sub(used);
        spans.push(Span::styled(format!("{}{right}", "─".repeat(fill)), base));
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

fn body_lines(input: &CardInput, painted: &[PaintedHunk], opts: &LayoutOpts) -> Vec<Line<'static>> {
    let d = &input.diff;
    let mut lines = match &d.kind {
        DiffKind::Binary => return vec![notice("(binary file)")],
        DiffKind::Missing => return vec![notice("(file missing after edit)")],
        DiffKind::Error(m) if d.is_empty() => {
            return vec![notice(format!("(no diff: {m})"))];
        }
        // An empty diff on a created or deleted file means the file itself is
        // empty; "(no changes)" next to a "Created" header reads as a bug.
        DiffKind::Created if d.is_empty() => return vec![notice("(empty file created)")],
        DiffKind::Deleted if d.is_empty() => return vec![notice("(empty file deleted)")],
        _ if d.is_empty() => return vec![notice("(no changes)")],
        _ => layout(painted, opts),
    };
    if d.truncated {
        lines.push(notice(format!(
            "... {} more lines",
            d.total_rows.saturating_sub(MAX_LINES)
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{self, DiffKind};
    use crate::snapshot::Before;
    use std::path::PathBuf;

    fn opts(mode: ViewMode, width: u16) -> LayoutOpts {
        LayoutOpts {
            mode,
            width,
            wrap: false,
        }
    }

    const UNIFIED_80: LayoutOpts = LayoutOpts {
        mode: ViewMode::Unified,
        width: 80,
        wrap: false,
    };
    const SPLIT_120: LayoutOpts = LayoutOpts {
        mode: ViewMode::SideBySide,
        width: 120,
        wrap: false,
    };

    fn input_named(name: &str, kind_before: &Before, after: Option<&[u8]>) -> CardInput {
        CardInput {
            path: name.into(),
            file: PathBuf::from(format!("/p/{name}")),
            tool: "Edit".into(),
            ts: 0,
            agent: Some("Explore".into()),
            diff: diff::compute(kind_before, after),
            user_modified: true,
            session_id: Some("sess-ab12cd".into()),
            worktree: None,
            root: PathBuf::from("/p"),
        }
    }

    fn input(kind_before: &Before, after: Option<&[u8]>) -> CardInput {
        input_named("f.txt", kind_before, after)
    }

    fn card(before: &[u8], after: &[u8], opts: LayoutOpts) -> EditCard {
        EditCard::new(input(&Before::Content(before.to_vec()), Some(after)), &opts)
    }

    fn spans_with_bg(line: &Line<'_>, bg: Color) -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| s.style.bg == Some(bg))
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn plain_render_and_summary() {
        let card = card(b"a\n", b"b\n", UNIFIED_80);
        assert_eq!(
            card.summary(false),
            "f.txt · Edit · Modified · +1 -1 · [Explore] · (user modified)"
        );
        let body = card.body_plain();
        assert!(body[0].starts_with("── @@ -1,1 +1,1 @@ ─"), "{}", body[0]);
        assert!(body[1].starts_with("  1     -a"), "{}", body[1]);
        assert!(body[2].starts_with("      1 +b"), "{}", body[2]);
        assert!(card.header_text(false).contains("--:--:--"));
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
    fn notices_are_mode_independent() {
        let big: String = (0..800).map(|i| format!("{i}\n")).collect();
        for o in [UNIFIED_80, SPLIT_120] {
            let bin = EditCard::new(input(&Before::Content(vec![0, 1]), Some(b"x")), &o);
            assert_eq!(bin.body_plain(), ["(binary file)"]);
            let missing = EditCard::new(input(&Before::Absent, None), &o);
            assert_eq!(missing.body_plain(), ["(file missing after edit)"]);
            let created = EditCard::new(input(&Before::Absent, Some(b"")), &o);
            assert_eq!(created.input.diff.kind, DiffKind::Created);
            assert_eq!(created.body_plain(), ["(empty file created)"]);
            let deleted = EditCard::new(input(&Before::Content(Vec::new()), None), &o);
            assert_eq!(deleted.input.diff.kind, DiffKind::Deleted);
            assert_eq!(deleted.body_plain(), ["(empty file deleted)"]);
            let same = EditCard::new(input(&Before::Content(b"x\n".to_vec()), Some(b"x\n")), &o);
            assert_eq!(same.body_plain(), ["(no changes)"]);
            let t = EditCard::new(input(&Before::Absent, Some(big.as_bytes())), &o);
            assert_eq!(t.input.diff.kind, DiffKind::Created);
            let body = t.body_plain();
            // hunk rule + 500 rows + notice
            assert_eq!(body.len(), 502);
            assert_eq!(body.last().unwrap(), "... 300 more lines");
        }
    }

    #[test]
    fn summary_is_header_without_the_time() {
        let card = card(b"a\n", b"b\n", UNIFIED_80);
        assert_eq!(
            card.header_text(false),
            "f.txt · Edit · --:--:-- · Modified · +1 -1 · [Explore] · (user modified)"
        );
        let same = EditCard::new(
            input(&Before::Content(b"x\n".to_vec()), Some(b"x\n")),
            &UNIFIED_80,
        );
        assert_eq!(
            same.summary(false),
            "f.txt · Edit · Modified · [Explore] · (user modified)"
        );
    }

    #[test]
    fn header_has_stats_and_right_aligned_time() {
        let card = card(b"a\n", b"b\n", UNIFIED_80);
        let l = card.header_line(100, false);
        assert_eq!(l.width(), 100);
        let text = l.to_string();
        assert!(
            text.starts_with("── f.txt · Edit · Modified · +1 -1 · [Explore] · (user modified) ─"),
            "{text}"
        );
        assert!(text.ends_with("─ --:--:-- ──"), "{text}");
        let tagged = card.header_line(100, true);
        assert_eq!(tagged.width(), 100);
        assert!(tagged.spans.iter().any(|s| s.content == "ab12cd"));
    }

    #[test]
    fn header_colors_kind_as_badge() {
        let created = EditCard::new(input(&Before::Absent, Some(b"x\n")), &UNIFIED_80);
        let l = created.header_line(80, false);
        let kind = l.spans.iter().find(|s| s.content == "Created").unwrap();
        assert_eq!(kind.style.fg, Some(Color::Green));
        let modified = card(b"a\n", b"b\n", UNIFIED_80).header_line(80, false);
        let kind = modified
            .spans
            .iter()
            .find(|s| s.content == "Modified")
            .unwrap();
        assert_eq!(kind.style.fg, None);
    }

    #[test]
    fn header_elides_long_path() {
        let long = format!("{}/deep/file.rs", "dir/".repeat(30));
        let card = EditCard::new(
            input_named(&long, &Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &UNIFIED_80,
        );
        let l = card.header_line(100, false);
        assert_eq!(l.width(), 100);
        let text = l.to_string();
        assert!(text.contains("…"), "{text}");
        assert!(text.contains("deep/file.rs · Edit"), "{text}");
        // Even a very narrow bar keeps the tail of the path.
        let narrow = card.header_line(60, false).to_string();
        assert!(narrow.contains("…"), "{narrow}");
        assert!(narrow.contains("rs · Edit"), "{narrow}");
        assert!(text.ends_with(" --:--:-- ──"), "{text}");
        assert_eq!(elide_left("abcdef", 4), "…def");
        assert_eq!(elide_left("abc", 4), "abc");
    }

    #[test]
    fn tag_prepended_when_requested() {
        let mut card = card(b"a\n", b"b\n", UNIFIED_80);
        assert_eq!(
            card.summary(true),
            "ab12cd · f.txt · Edit · Modified · +1 -1 · [Explore] · (user modified)"
        );
        card.input.worktree = Some("feat-x".into());
        assert_eq!(card.tag().as_deref(), Some("feat-x:ab12cd"));
        assert!(card.summary(true).starts_with("feat-x:ab12cd · f.txt"));
        card.input.session_id = None;
        assert_eq!(card.tag(), None);
        assert_eq!(card.summary(true), card.summary(false));
    }

    #[test]
    fn session_color_is_deterministic_and_in_palette() {
        let c = session_color("sess-ab12cd");
        assert_eq!(c, session_color("sess-ab12cd"));
        assert!(SESSION_COLORS.contains(&c));
        assert_eq!(session_suffix("ab"), "ab");
        assert_eq!(session_suffix("sess-ab12cd"), "ab12cd");
    }

    #[test]
    fn split_pairs_deletes_with_inserts() {
        let card = card(b"a\nb\nc\n", b"a\nB\nc\n", SPLIT_120);
        let body = card.body_plain();
        assert!(body[0].starts_with("── @@ -1,3 +1,3 @@ ─"), "{}", body[0]);
        assert!(
            body[1].contains("  1  a") && body[1].contains("│   1  a"),
            "{}",
            body[1]
        );
        assert!(
            body[2].contains("  2 -b") && body[2].contains("│   2 +B"),
            "{}",
            body[2]
        );
        let data_w = card.lines[1].width();
        for l in &card.lines[1..] {
            assert!(l.width() <= 120);
            assert_eq!(l.width(), data_w, "{l}");
        }
        assert_eq!(card.lines[0].width(), 120);
    }

    #[test]
    fn split_unbalanced_run_leaves_blank_side() {
        let card = card(b"a\nb\nc\n", b"x\n", SPLIT_120);
        let body = card.body_plain();
        let rows: Vec<Vec<&str>> = body[1..].iter().map(|l| l.split('│').collect()).collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0][0].contains("-a") && rows[0][1].contains("+x"));
        assert!(rows[1][0].contains("-b") && rows[1][1].trim().is_empty());
        assert!(rows[2][0].contains("-c") && rows[2][1].trim().is_empty());
        // The blank side carries no tint.
        assert!(card.lines[2].spans.last().unwrap().style.bg.is_none());
    }

    #[test]
    fn split_falls_back_below_threshold() {
        let narrow = card(b"a\n", b"b\n", opts(ViewMode::SideBySide, 80));
        let unified = card(b"a\n", b"b\n", UNIFIED_80);
        assert_eq!(narrow.body_plain(), unified.body_plain());
        assert!(!narrow.body_plain().iter().any(|l| l.contains('│')));
        let wide = card(b"a\n", b"b\n", opts(ViewMode::SideBySide, 100));
        assert!(wide.body_plain().iter().any(|l| l.contains('│')));
    }

    #[test]
    fn row_backgrounds_and_emphasis() {
        let words = card(b"hello world\n", b"hello there\n", UNIFIED_80);
        let del = &words.lines[1];
        let ins = &words.lines[2];
        assert!(
            spans_with_bg(del, REMOVED_BG)
                .iter()
                .any(|s| s.contains("hello "))
        );
        assert_eq!(spans_with_bg(del, REMOVED_EMPH_BG), ["world"]);
        assert_eq!(spans_with_bg(ins, ADDED_EMPH_BG), ["there"]);
        // The gutter and the padding carry the row tint too.
        assert_eq!(del.spans[0].style.bg, Some(REMOVED_BG));
        assert_eq!(del.spans.last().unwrap().style.bg, Some(REMOVED_BG));
        assert_eq!(del.width(), 80);
        // Context rows have no tint and no padding.
        let ctx = card(b"a\nb\n", b"a\nc\n", UNIFIED_80);
        let equal = &ctx.lines[1];
        assert!(equal.spans.iter().all(|s| s.style.bg.is_none()));
        assert_eq!(equal.to_string(), "  1   1  a");
    }

    #[test]
    fn syntax_colors_rust_keyword() {
        let rs = EditCard::new(
            input_named("m.rs", &Before::Absent, Some(b"fn main() {}\n")),
            &UNIFIED_80,
        );
        let fg_of = |card: &EditCard, text: &str| {
            card.lines[1]
                .spans
                .iter()
                .find(|s| s.content.trim() == text)
                .map(|s| s.style.fg)
        };
        let keyword = fg_of(&rs, "fn").expect("fn is its own span");
        assert!(matches!(keyword, Some(Color::Rgb(..))), "{keyword:?}");
        let ident = fg_of(&rs, "main").expect("main is its own span");
        assert_ne!(keyword, ident);
        assert!(rs.body_plain()[1].contains("+fn main() {}"));

        let unknown = EditCard::new(
            input_named("m.unknownext", &Before::Absent, Some(b"fn main() {}\n")),
            &UNIFIED_80,
        );
        assert!(
            unknown.lines[1]
                .spans
                .iter()
                .all(|s| s.style.fg.is_none() || s.style.fg == Some(GUTTER_FG))
        );
        let txt = EditCard::new(input(&Before::Absent, Some(b"fn main() {}\n")), &UNIFIED_80);
        assert!(
            txt.lines[1]
                .spans
                .iter()
                .all(|s| s.style.fg.is_none() || s.style.fg == Some(GUTTER_FG))
        );
    }

    #[test]
    fn first_line_detection_for_shebang() {
        let sh = EditCard::new(
            input_named("run", &Before::Absent, Some(b"#!/bin/sh\necho hi\n")),
            &UNIFIED_80,
        );
        assert!(
            sh.lines[2]
                .spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        );
        assert_eq!(sh.input.diff.first_line, "#!/bin/sh");
    }

    #[test]
    fn wrap_emits_continuation_rows() {
        let long = format!("{}\n", "x".repeat(200));
        let o = LayoutOpts {
            mode: ViewMode::SideBySide,
            width: 100,
            wrap: true,
        };
        let card = card(b"short\n", long.as_bytes(), o);
        let body = card.body_plain();
        assert!(body.len() > 3, "{body:?}");
        assert!(body.iter().all(|l| !l.contains('…')));
        let w = card.lines[1].width();
        for l in &card.lines[1..] {
            assert!(l.width() <= 100);
            assert_eq!(l.width(), w, "{l}");
        }
        // Continuations: blank left side, blank right gutter, tinted text.
        let cont = &card.lines[2];
        let text = cont.to_string();
        let (left, right) = text.split_once('│').unwrap();
        assert!(left.trim().is_empty(), "{text}");
        assert!(right.starts_with("      x"), "{text}");
        let bg = cont.spans.last().unwrap().style.bg;
        assert!(bg == Some(ADDED_BG) || bg == Some(ADDED_EMPH_BG), "{bg:?}");
        let joined: String = body[1..]
            .iter()
            .map(|l| {
                l.split('│')
                    .nth(1)
                    .unwrap()
                    .trim()
                    .trim_start_matches(|c: char| c != 'x')
            })
            .collect();
        assert_eq!(joined, "x".repeat(200));

        let uni = card_with(b"short\n", long.as_bytes(), ViewMode::Unified, 80, true);
        let body = uni.body_plain();
        assert!(body.len() > 3);
        assert!(body[3].starts_with("         x"), "{}", body[3]);
    }

    fn card_with(before: &[u8], after: &[u8], mode: ViewMode, width: u16, wrap: bool) -> EditCard {
        card(before, after, LayoutOpts { mode, width, wrap })
    }

    #[test]
    fn no_wrap_uses_ellipsis() {
        let long = format!("{}日本語\n", "x".repeat(200));
        let card = card(b"short\n", long.as_bytes(), opts(ViewMode::SideBySide, 100));
        let w = card.lines[1].width();
        for l in &card.lines[1..] {
            assert!(l.width() <= 100);
            assert_eq!(l.width(), w, "{l}");
        }
        assert!(
            card.body_plain()[1].contains('…'),
            "{:?}",
            card.body_plain()
        );
        let seg = |s: &str| {
            vec![Seg {
                text: s.into(),
                fg: None,
                emph: false,
            }]
        };
        let text = |lines: Vec<Vec<Seg>>| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.iter().map(|s| s.text.as_str()).collect())
                .collect()
        };
        assert_eq!(text(chunks(&seg("日本語"), 4, false)), ["日 …"]);
        assert_eq!(text(chunks(&seg("日本語"), 5, false)), ["日本…"]);
        assert_eq!(text(chunks(&seg("ab"), 4, false)), ["ab"]);
        assert_eq!(text(chunks(&seg("abcdef"), 4, true)), ["abcd", "ef"]);
        assert_eq!(text(chunks(&seg("a\tb"), 8, false)), ["a   b"]);
        assert_eq!(text(chunks(&seg("abcd\tb"), 12, false)), ["abcd    b"]);
        assert_eq!(text(chunks(&seg(""), 4, true)), [""]);
    }

    #[test]
    fn unified_layout_has_two_gutters() {
        let card = card(b"a\nb\nc\n", b"a\nB\nc\n", UNIFIED_80);
        let body = card.body_plain();
        assert_eq!(body[1], "  1   1  a");
        assert!(body[2].starts_with("  2     -b"), "{}", body[2]);
        assert!(body[3].starts_with("      2 +B"), "{}", body[3]);
        assert_eq!(body[4], "  3   3  c");
    }

    #[test]
    fn hunk_rule_carries_range() {
        let card = card(b"a\nb\nc\n", b"a\nB\nc\n", UNIFIED_80);
        let rule = &card.lines[0];
        assert_eq!(rule.width(), 80);
        assert!(rule.to_string().starts_with("── @@ -1,3 +1,3 @@ ───"));
        assert_eq!(rule.spans[0].style.fg, Some(GUTTER_FG));
    }
}
