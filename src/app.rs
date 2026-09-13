//! TUI state: the card feed, scrolling, follow mode, and the spool tail loop.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use crate::paths::Paths;
use crate::render::{EditCard, LayoutOpts, ViewMode};
use crate::session::{CardInput, Pipeline};
use crate::spool::{self, MatchMode, SpoolTailer};

const WHEEL_STEP: usize = 3;

/// Key table shown by `?`.
pub const KEYS: &[(&str, &str)] = &[
    ("j / k, arrows, wheel", "scroll"),
    ("Ctrl-d / Ctrl-u", "half page"),
    ("PgDn / PgUp", "page"),
    ("g / G", "top / bottom, resume follow"),
    ("p", "toggle follow"),
    ("n / N", "next / previous card"),
    ("Enter", "collapse / expand card"),
    ("z", "collapse / expand all"),
    ("v", "side by side / unified"),
    ("w", "toggle wrap"),
    ("Tab", "cycle session filter"),
    ("?", "this help"),
    ("q, Esc, Ctrl-c", "quit"),
];

/// Where a visible card starts in `lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CardSpan {
    start: usize,
    card: usize,
}

pub struct App {
    pub cards: Vec<EditCard>,
    lines: Vec<Line<'static>>,
    /// One entry per visible card, in feed order.
    spans: Vec<CardSpan>,
    pub offset: usize,
    pub follow: bool,
    pub quit: bool,
    pub help: bool,
    mode: ViewMode,
    wrap: bool,
    cwd: PathBuf,
    width: u16,
    viewport: usize,
    pending_width: Option<u16>,
    pipeline: Pipeline,
    tailer: SpoolTailer,
    spool_path: PathBuf,
    /// Session ids in order of first card.
    sessions: Vec<String>,
    /// Index into `sessions` when the feed is filtered to one session.
    selected: Option<usize>,
}

impl App {
    /// Replay the current sessions for `cwd` and prepare to tail.
    pub fn new(
        cwd: PathBuf,
        paths: &Paths,
        mode: ViewMode,
        width: u16,
        matching: MatchMode,
    ) -> Self {
        let mut pipeline = Pipeline::new(cwd.clone(), paths.snapshot_root.clone(), matching);
        let replay = spool::scan_replay(&paths.spool, pipeline.matcher_mut());
        let inputs = pipeline.replay(&replay.events);
        let tailer = SpoolTailer::new(paths.spool.clone(), replay.offset);
        let mut app = Self {
            cards: Vec::new(),
            lines: Vec::new(),
            spans: Vec::new(),
            offset: 0,
            follow: true,
            quit: false,
            help: false,
            mode,
            wrap: true,
            cwd,
            width,
            viewport: 1,
            pending_width: None,
            pipeline,
            tailer,
            spool_path: paths.spool.clone(),
            sessions: Vec::new(),
            selected: None,
        };
        for input in inputs {
            app.push_card(input);
        }
        app.rebuild_lines();
        app
    }

    fn opts(&self) -> LayoutOpts {
        LayoutOpts {
            mode: self.mode,
            width: self.width,
            wrap: self.wrap,
        }
    }

    /// Render a card and record its session. Does not rebuild `lines`.
    pub fn push_card(&mut self, input: CardInput) {
        if let Some(id) = &input.session_id
            && !self.sessions.iter().any(|s| s == id)
        {
            self.sessions.push(id.clone());
        }
        self.cards.push(EditCard::new(input, &self.opts()));
    }

    fn show_tags(&self) -> bool {
        self.sessions.len() > 1
    }

    pub fn selected_session(&self) -> Option<&str> {
        self.selected.map(|i| self.sessions[i].as_str())
    }

    fn is_visible(&self, card: &EditCard) -> bool {
        match self.selected_session() {
            Some(id) => card.input.session_id.as_deref() == Some(id),
            None => true,
        }
    }

    pub fn visible_cards(&self) -> usize {
        self.spans.len()
    }

    pub fn has_visible_cards(&self) -> bool {
        !self.spans.is_empty()
    }

    /// Tab: all -> each session in first-seen order -> all. Follow is left
    /// alone, so the rebuild lands at the bottom when it is on.
    pub fn cycle_session(&mut self) {
        self.selected = match self.selected {
            None if self.sessions.is_empty() => None,
            None => Some(0),
            Some(i) if i + 1 < self.sessions.len() => Some(i + 1),
            Some(_) => None,
        };
        self.rebuild_lines();
    }

    /// `all`, or the tag of the selected session's first card.
    pub fn session_label(&self) -> String {
        let Some(id) = self.selected_session() else {
            return "all".into();
        };
        self.cards
            .iter()
            .find(|c| c.input.session_id.as_deref() == Some(id))
            .and_then(|c| c.tag())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn spool_path(&self) -> &Path {
        &self.spool_path
    }

    pub fn spool_exists(&self) -> bool {
        self.spool_path.exists()
    }

    pub fn view_mode(&self) -> ViewMode {
        self.mode
    }

    pub fn wrap(&self) -> bool {
        self.wrap
    }

    fn rerender_all(&mut self) {
        let opts = self.opts();
        for card in &mut self.cards {
            card.rerender(&opts);
        }
        self.rebuild_lines();
    }

    /// `v`: side by side <-> unified. Follow is left alone like
    /// `cycle_session`.
    pub fn toggle_view(&mut self) {
        self.mode = self.mode.toggle();
        self.rerender_all();
    }

    /// `w`: wrap <-> clip long lines.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.rerender_all();
    }

    /// One iteration of background work: tail the spool, prune, apply resize.
    pub fn tick(&mut self) {
        if let Some(w) = self.pending_width.take()
            && w != self.width
        {
            self.width = w;
            self.rerender_all();
        }
        let events = self.tailer.poll();
        let mut added = false;
        for ev in &events {
            if let Some(input) = self.pipeline.handle(ev) {
                self.push_card(input);
                added = true;
            }
        }
        if added {
            self.rebuild_lines();
        }
        self.pipeline.prune_pending(now_ms());
    }

    pub(crate) fn rebuild_lines(&mut self) {
        let tags = self.show_tags();
        let mut lines = Vec::new();
        let mut spans = Vec::new();
        for (i, card) in self.cards.iter().enumerate() {
            if !self.is_visible(card) {
                continue;
            }
            spans.push(CardSpan {
                start: lines.len(),
                card: i,
            });
            lines.push(card.header_line(self.width, tags));
            if !card.collapsed {
                lines.extend(card.lines.iter().cloned());
            }
            lines.push(Line::default());
        }
        self.lines = lines;
        self.spans = spans;
        self.clamp();
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    pub fn set_viewport(&mut self, height: u16) {
        self.viewport = height.max(1) as usize;
        self.clamp();
    }

    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport)
    }

    fn clamp(&mut self) {
        if self.follow {
            self.offset = self.max_offset();
        } else {
            self.offset = self.offset.min(self.max_offset());
        }
    }

    pub fn all_lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn visible_lines(&self) -> &[Line<'static>] {
        let end = (self.offset + self.viewport).min(self.lines.len());
        &self.lines[self.offset.min(end)..end]
    }

    /// Index into `spans` of the card owning the line at `offset`.
    fn current(&self) -> Option<usize> {
        self.spans.iter().rposition(|s| s.start <= self.offset)
    }

    /// Index into `cards` of the current card.
    pub fn current_card(&self) -> Option<usize> {
        self.current().map(|i| self.spans[i].card)
    }

    /// True when the line at `offset` is the current card's own header.
    pub fn at_card_start(&self) -> bool {
        self.current()
            .is_some_and(|i| self.spans[i].start == self.offset)
    }

    /// The current card's header, for pinning above the body.
    pub fn sticky_header(&self) -> Option<Line<'static>> {
        let i = self.current()?;
        Some(self.lines[self.spans[i].start].clone())
    }

    /// Put `line` at the top of the viewport. Follow resumes only when that
    /// is also the bottom of the feed.
    fn jump_to(&mut self, line: usize) {
        self.offset = line.min(self.max_offset());
        self.follow = self.offset == self.max_offset();
    }

    /// `n`: the next card's header to the top.
    pub fn next_card(&mut self) {
        if let Some(i) = self.current()
            && let Some(next) = self.spans.get(i + 1)
        {
            self.jump_to(next.start);
        }
    }

    /// `N`: the current card's header to the top, or the previous card's
    /// when the header is already there.
    pub fn prev_card(&mut self) {
        let Some(i) = self.current() else { return };
        let start = self.spans[i].start;
        if self.offset > start {
            self.jump_to(start);
        } else if i > 0 {
            self.jump_to(self.spans[i - 1].start);
        }
    }

    /// Rebuild after a collapse change and keep `card` at the top.
    fn rebuild_keeping(&mut self, card: usize) {
        self.follow = false;
        self.rebuild_lines();
        if let Some(span) = self.spans.iter().find(|s| s.card == card) {
            self.jump_to(span.start);
        }
    }

    /// Enter: collapse or expand the current card.
    pub fn toggle_collapse(&mut self) {
        let Some(i) = self.current_card() else { return };
        self.cards[i].collapsed = !self.cards[i].collapsed;
        self.rebuild_keeping(i);
    }

    /// z: collapse every visible card, or expand all when none is expanded.
    pub fn toggle_collapse_all(&mut self) {
        let Some(cur) = self.current_card() else {
            return;
        };
        let any_expanded = self.spans.iter().any(|s| !self.cards[s.card].collapsed);
        for s in &self.spans {
            self.cards[s.card].collapsed = any_expanded;
        }
        self.rebuild_keeping(cur);
    }

    fn scroll_by(&mut self, delta: i64) {
        if delta < 0 {
            self.follow = false;
            self.offset = self.offset.saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.offset = (self.offset + delta as usize).min(self.max_offset());
            if self.offset == self.max_offset() {
                self.follow = true;
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.help {
            if matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('?'), _) | (KeyCode::Esc, _) | (KeyCode::Char('q'), _)
            ) {
                self.help = false;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.quit = true;
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => self.quit = true,
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => self.quit = true,
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => self.scroll_by(1),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => self.scroll_by(-1),
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.scroll_by(self.viewport as i64 / 2)
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.scroll_by(-(self.viewport as i64 / 2))
            }
            (KeyCode::PageDown, _) => self.scroll_by(self.viewport as i64),
            (KeyCode::PageUp, _) => self.scroll_by(-(self.viewport as i64)),
            (KeyCode::Char('g'), _) => {
                self.follow = false;
                self.offset = 0;
            }
            (KeyCode::Char('G'), _) => {
                self.follow = true;
                self.clamp();
            }
            (KeyCode::Char('p'), _) => {
                self.follow = !self.follow;
                self.clamp();
            }
            (KeyCode::Char('n'), _) => self.next_card(),
            (KeyCode::Char('N'), _) => self.prev_card(),
            (KeyCode::Enter, _) => self.toggle_collapse(),
            (KeyCode::Char('z'), _) => self.toggle_collapse_all(),
            (KeyCode::Char('v'), _) => self.toggle_view(),
            (KeyCode::Char('w'), _) => self.toggle_wrap(),
            (KeyCode::Char('?'), _) => self.help = true,
            (KeyCode::Tab, _) => self.cycle_session(),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_STEP as i64)),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_STEP as i64),
            _ => {}
        }
    }

    pub fn on_resize(&mut self, width: u16, _height: u16) {
        self.pending_width = Some(width);
    }

    /// Status bar text fitted to `width`: the directory is elided from the
    /// left so the state fields stay visible.
    pub fn status(&self, width: u16) -> String {
        let card = self.current().map(|i| i + 1).unwrap_or(0);
        let right = format!(
            "session: {}  card: {card}/{}  follow: {}  view: {}  wrap: {}  ? help",
            self.session_label(),
            self.spans.len(),
            if self.follow { "on" } else { "off" },
            self.mode.label(),
            if self.wrap { "on" } else { "off" },
        );
        let mut cwd = tilde(self.pipeline.toplevel().unwrap_or(&self.cwd));
        let avail = (width as usize).saturating_sub(right.chars().count() + 3);
        let len = cwd.chars().count();
        if len > avail {
            let keep = avail.saturating_sub(1);
            cwd = format!("…{}", cwd.chars().skip(len - keep).collect::<String>());
        }
        format!(" {cwd}  {right}")
    }
}

/// The key table as lines for the help overlay.
pub fn help_lines() -> Vec<Line<'static>> {
    let key_w = KEYS.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    KEYS.iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!(" {k:<key_w$}  "), Style::new().bold()),
                Span::raw(format!("{what} ")).dim(),
            ])
        })
        .collect()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn tilde(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::diff;
    use crate::snapshot::Before;

    pub(crate) fn app(dir: &Path) -> App {
        app_at(dir, 80)
    }

    pub(crate) fn app_at(dir: &Path, width: u16) -> App {
        let paths = Paths {
            spool: dir.join("events.jsonl"),
            snapshot_root: dir.join("snaps"),
            hook_script: dir.join("hook.sh"),
            settings: dir.join("settings.json"),
        };
        App::new(
            dir.to_path_buf(),
            &paths,
            ViewMode::SideBySide,
            width,
            MatchMode::CwdOnly,
        )
    }

    pub(crate) fn input(session: &str) -> CardInput {
        input_named(session, "f.txt")
    }

    pub(crate) fn input_named(session: &str, path: &str) -> CardInput {
        CardInput {
            path: path.into(),
            file: PathBuf::from(format!("/p/{path}")),
            tool: "Edit".into(),
            ts: 0,
            agent: None,
            diff: diff::compute(&Before::Content(b"a\n".to_vec()), Some(b"b\n")),
            user_modified: false,
            session_id: Some(session.to_string()),
            worktree: None,
            root: PathBuf::from("/p"),
        }
    }

    fn headers(app: &App) -> Vec<String> {
        app.lines
            .iter()
            .map(|l| l.to_string())
            .filter(|l| l.starts_with("── ") && !l.starts_with("── @@"))
            .collect()
    }

    fn has_split_rows(app: &App) -> bool {
        app.lines.iter().any(|l| l.to_string().contains('│'))
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    /// Three one-line cards: header + hunk rule + 2 rows + blank = 5 lines each.
    fn three_cards(dir: &Path) -> App {
        let mut app = app(dir);
        for p in ["a.txt", "b.txt", "c.txt"] {
            app.push_card(input_named("s1", p));
        }
        app.rebuild_lines();
        app.set_viewport(4);
        app
    }

    #[test]
    fn tags_hidden_until_a_second_session_has_a_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.push_card(input("sess-aaaaaa"));
        app.rebuild_lines();
        assert!(!headers(&app)[0].contains("aaaaaa"));
        app.push_card(input("sess-bbbbbb"));
        app.rebuild_lines();
        let h = headers(&app);
        assert!(h[0].contains("aaaaaa"), "{}", h[0]);
        assert!(h[1].contains("bbbbbb"), "{}", h[1]);
    }

    #[test]
    fn tab_cycles_in_first_seen_order_and_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        for s in ["s1", "s2", "s1", "s3"] {
            app.push_card(input(s));
        }
        app.rebuild_lines();
        app.set_viewport(3);
        assert!(app.status(200).contains("session: all  card: 4/4"));

        let expect = [(Some("s1"), 2), (Some("s2"), 1), (Some("s3"), 1), (None, 4)];
        for (id, visible) in expect {
            app.on_key(KeyEvent::from(KeyCode::Tab));
            assert_eq!(app.selected_session(), id);
            assert_eq!(app.visible_cards(), visible);
            assert_eq!(headers(&app).len(), visible);
            assert!(app.follow);
            assert_eq!(app.offset, app.max_offset());
        }
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert!(app.status(200).contains("session: s1  card: 2/2"));
    }

    #[test]
    fn tab_with_no_cards_stays_on_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.cycle_session();
        assert_eq!(app.selected_session(), None);
        assert!(!app.has_visible_cards());
        assert_eq!(app.sticky_header(), None);
        assert!(app.status(200).contains("card: 0/0"));
    }

    #[test]
    fn v_toggles_view_and_rerenders_cards() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path(), 120);
        app.push_card(input("s1"));
        app.rebuild_lines();
        assert!(app.status(200).contains("view: split"));
        assert!(has_split_rows(&app));
        app.on_key(key('v'));
        assert_eq!(app.view_mode(), ViewMode::Unified);
        assert!(app.status(200).contains("view: unified"));
        assert!(!has_split_rows(&app));
        app.on_key(key('v'));
        assert_eq!(app.view_mode(), ViewMode::SideBySide);
        assert!(has_split_rows(&app));
    }

    #[test]
    fn resize_rerenders_split_layout() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path(), 120);
        app.push_card(input("s1"));
        app.rebuild_lines();
        assert!(has_split_rows(&app));
        app.on_resize(80, 24);
        app.tick();
        assert!(!has_split_rows(&app));
        assert!(app.status(200).contains("view: split"));
        app.on_resize(120, 24);
        app.tick();
        assert!(has_split_rows(&app));
    }

    #[test]
    fn w_toggles_wrap_and_rerenders() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let mut long = input("s1");
        long.diff = diff::compute(
            &Before::Content(b"a\n".to_vec()),
            Some(format!("{}\n", "x".repeat(200)).as_bytes()),
        );
        app.push_card(long);
        app.rebuild_lines();
        assert!(app.wrap());
        let wrapped = app.total_lines();
        assert!(!app.lines.iter().any(|l| l.to_string().contains('…')));
        app.on_key(key('w'));
        assert!(!app.wrap());
        assert!(app.status(200).contains("wrap: off"));
        assert!(app.total_lines() < wrapped);
        assert!(app.lines.iter().any(|l| l.to_string().contains('…')));
    }

    #[test]
    fn current_card_follows_offset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = three_cards(dir.path());
        assert_eq!(app.total_lines(), 15);
        app.on_key(key('g'));
        assert_eq!(app.current_card(), Some(0));
        assert!(app.at_card_start());
        app.offset = 4; // the blank after card 0
        assert_eq!(app.current_card(), Some(0));
        assert!(!app.at_card_start());
        app.offset = 5;
        assert_eq!(app.current_card(), Some(1));
        assert!(app.at_card_start());
        assert!(app.sticky_header().unwrap().to_string().contains("b.txt"));
        assert!(app.status(200).contains("card: 2/3"));
    }

    #[test]
    fn n_and_prev_move_between_card_headers() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = three_cards(dir.path());
        app.on_key(key('g'));
        app.on_key(key('n'));
        assert_eq!((app.offset, app.follow), (5, false));
        app.on_key(key('n'));
        // Card 2 starts at 10, but max_offset is 11 with a 4-line viewport.
        assert_eq!(app.offset, 10);
        app.on_key(key('n'));
        assert_eq!(app.offset, 10);
        app.on_key(key('j'));
        assert_eq!(app.offset, 11);
        assert!(app.follow);
        app.on_key(key('N'));
        assert_eq!((app.offset, app.follow), (10, false));
        app.on_key(key('N'));
        assert_eq!(app.offset, 5);
        app.on_key(key('N'));
        assert_eq!(app.offset, 0);
        app.on_key(key('N'));
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn enter_collapses_current_card_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = three_cards(dir.path());
        app.on_key(key('g'));
        app.on_key(key('n'));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.cards[1].collapsed);
        assert!(!app.cards[0].collapsed && !app.cards[2].collapsed);
        // header + blank for card 1
        assert_eq!(app.total_lines(), 12);
        assert_eq!(app.offset, 5);
        assert_eq!(app.current_card(), Some(1));
        assert_eq!(headers(&app).len(), 3);
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(!app.cards[1].collapsed);
        assert_eq!(app.total_lines(), 15);
    }

    #[test]
    fn z_toggles_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = three_cards(dir.path());
        app.on_key(key('z'));
        assert!(app.cards.iter().all(|c| c.collapsed));
        assert_eq!(app.total_lines(), 6);
        app.cards[0].collapsed = false;
        app.rebuild_lines();
        // One expanded card means z collapses again.
        app.on_key(key('z'));
        assert!(app.cards.iter().all(|c| c.collapsed));
        app.on_key(key('z'));
        assert!(app.cards.iter().all(|c| !c.collapsed));
        assert_eq!(app.total_lines(), 15);
    }

    #[test]
    fn help_swallows_keys_until_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = three_cards(dir.path());
        app.on_key(key('?'));
        assert!(app.help);
        app.on_key(key('q'));
        assert!(!app.help && !app.quit);
        app.on_key(key('?'));
        app.on_key(key('v'));
        assert_eq!(app.view_mode(), ViewMode::SideBySide);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.help);
        app.on_key(key('q'));
        assert!(app.quit);
        assert!(help_lines().len() == KEYS.len());
    }
}
