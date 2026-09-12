//! Turn a `CardInput` into styled terminal lines, via delta or a plain fallback.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

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

/// Run delta over a unified diff and return its ANSI output.
///
/// stdin is fed from a thread. Writing it inline would deadlock on any diff
/// whose delta output fills the stdout pipe before we start reading: delta
/// blocks writing, we block writing, and the whole TUI stops responding.
pub fn delta_ansi(bin: &Path, unified: &str, width: u16, cwd: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new(bin)
        .args([
            "--paging=never",
            "--max-line-length",
            "0",
            "--width",
            &width.max(20).to_string(),
        ])
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
    pub fn new(input: CardInput, renderer: &Renderer, width: u16) -> Self {
        let lines = body_lines(&input, renderer, width);
        Self { input, lines }
    }

    pub fn rerender(&mut self, renderer: &Renderer, width: u16) {
        self.lines = body_lines(&self.input, renderer, width);
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

fn body_lines(input: &CardInput, renderer: &Renderer, width: u16) -> Vec<Line<'static>> {
    let d = &input.diff;
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
            Renderer::Delta(bin) => delta_ansi(bin, &d.unified, width, &input.root)
                .and_then(|ansi| ansi.into_text().ok())
                .map(|t| t.lines)
                .unwrap_or_else(|| plain_lines(&d.unified)),
            Renderer::Plain => plain_lines(&d.unified),
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
            80,
        );
        assert_eq!(bin.body_plain(), ["(binary file)"]);
        let missing = EditCard::new(input(&Before::Absent, None), &Renderer::Plain, 80);
        assert_eq!(missing.body_plain(), ["(file missing after edit)"]);
        let big: String = (0..800).map(|i| format!("{i}\n")).collect();
        let t = EditCard::new(
            input(&Before::Absent, Some(big.as_bytes())),
            &Renderer::Plain,
            80,
        );
        assert_eq!(t.input.diff.kind, DiffKind::Created);
        assert_eq!(t.body_plain().last().unwrap(), "... 303 more lines");
    }

    #[test]
    fn empty_created_and_deleted_files_say_so() {
        let created = EditCard::new(input(&Before::Absent, Some(b"")), &Renderer::Plain, 80);
        assert_eq!(created.input.diff.kind, DiffKind::Created);
        assert_eq!(created.body_plain(), ["(empty file created)"]);
        let deleted = EditCard::new(
            input(&Before::Content(Vec::new()), None),
            &Renderer::Plain,
            80,
        );
        assert_eq!(deleted.input.diff.kind, DiffKind::Deleted);
        assert_eq!(deleted.body_plain(), ["(empty file deleted)"]);
        let same = EditCard::new(
            input(&Before::Content(b"x\n".to_vec()), Some(b"x\n")),
            &Renderer::Plain,
            80,
        );
        assert_eq!(same.body_plain(), ["(no changes)"]);
    }

    #[test]
    fn summary_is_header_without_the_time() {
        let card = EditCard::new(
            input(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            &Renderer::Plain,
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

    #[test]
    fn session_color_is_deterministic_and_in_palette() {
        let c = session_color("sess-ab12cd");
        assert_eq!(c, session_color("sess-ab12cd"));
        assert!(SESSION_COLORS.contains(&c));
        assert_eq!(session_suffix("ab"), "ab");
        assert_eq!(session_suffix("sess-ab12cd"), "ab12cd");
    }
}
