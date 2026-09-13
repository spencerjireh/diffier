//! TUI state: the card feed, scrolling, follow mode, and the spool tail loop.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::text::Line;

use crate::paths::Paths;
use crate::render::{EditCard, Renderer, ViewMode};
use crate::session::{CardInput, Pipeline};
use crate::spool::{self, MatchMode, SpoolTailer};

const WHEEL_STEP: usize = 3;

pub struct App {
    pub cards: Vec<EditCard>,
    lines: Vec<Line<'static>>,
    pub offset: usize,
    pub follow: bool,
    pub quit: bool,
    renderer: Renderer,
    mode: ViewMode,
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
    /// Cards in `lines` after the filter.
    visible: usize,
}

impl App {
    /// Replay the current sessions for `cwd` and prepare to tail.
    pub fn new(
        cwd: PathBuf,
        paths: &Paths,
        renderer: Renderer,
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
            offset: 0,
            follow: true,
            quit: false,
            renderer,
            mode,
            cwd,
            width,
            viewport: 1,
            pending_width: None,
            pipeline,
            tailer,
            spool_path: paths.spool.clone(),
            sessions: Vec::new(),
            selected: None,
            visible: 0,
        };
        for input in inputs {
            app.push_card(input);
        }
        app.rebuild_lines();
        app
    }

    /// Render a card and record its session. Does not rebuild `lines`.
    pub fn push_card(&mut self, input: CardInput) {
        if let Some(id) = &input.session_id
            && !self.sessions.iter().any(|s| s == id)
        {
            self.sessions.push(id.clone());
        }
        self.cards
            .push(EditCard::new(input, &self.renderer, self.mode, self.width));
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
        self.visible
    }

    pub fn has_visible_cards(&self) -> bool {
        self.visible > 0
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

    pub fn spool_exists(&self) -> bool {
        self.spool_path.exists()
    }

    pub fn renderer_is_delta(&self) -> bool {
        self.renderer.is_delta()
    }

    pub fn view_mode(&self) -> ViewMode {
        self.mode
    }

    /// `v`: side by side <-> unified. Every card is laid out per mode, so all
    /// of them re-render; follow is left alone like `cycle_session`.
    pub fn toggle_view(&mut self) {
        self.mode = self.mode.toggle();
        for card in &mut self.cards {
            card.rerender(&self.renderer, self.mode, self.width);
        }
        self.rebuild_lines();
    }

    /// One iteration of background work: tail the spool, prune, apply resize.
    pub fn tick(&mut self) {
        if let Some(w) = self.pending_width.take()
            && w != self.width
        {
            self.width = w;
            // Delta and the split layout are laid out to the width; plain
            // unified is not, so a resize there costs one pass over the
            // headers instead of a render per card.
            if self.renderer.is_delta() || self.mode == ViewMode::SideBySide {
                for card in &mut self.cards {
                    card.rerender(&self.renderer, self.mode, self.width);
                }
            }
            self.rebuild_lines();
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

    fn rebuild_lines(&mut self) {
        let tags = self.show_tags();
        let mut lines = Vec::new();
        let mut visible = 0;
        for card in self.cards.iter().filter(|c| self.is_visible(c)) {
            lines.push(card.header_line(self.width, tags));
            lines.extend(card.lines.iter().cloned());
            lines.push(Line::default());
            visible += 1;
        }
        self.lines = lines;
        self.visible = visible;
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

    pub fn visible_lines(&self) -> &[Line<'static>] {
        let end = (self.offset + self.viewport).min(self.lines.len());
        &self.lines[self.offset.min(end)..end]
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
            (KeyCode::Char('v'), _) => self.toggle_view(),
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
    /// left so the counters and key hints stay visible.
    pub fn status(&self, width: u16) -> String {
        let cards = match self.selected {
            Some(_) => format!("{}/{}", self.visible, self.cards.len()),
            None => self.cards.len().to_string(),
        };
        let right = format!(
            "session: {}  cards: {cards}  follow: {}  view: {}  delta: {}  [j/k g/G p v Tab q]",
            self.session_label(),
            if self.follow { "on" } else { "off" },
            self.mode.label(),
            if self.renderer.is_delta() {
                "yes"
            } else {
                "no"
            },
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
mod tests {
    use super::*;
    use crate::diff;
    use crate::snapshot::Before;

    fn app(dir: &Path) -> App {
        app_at(dir, 80)
    }

    fn app_at(dir: &Path, width: u16) -> App {
        let paths = Paths {
            spool: dir.join("events.jsonl"),
            snapshot_root: dir.join("snaps"),
            hook_script: dir.join("hook.sh"),
            settings: dir.join("settings.json"),
        };
        App::new(
            dir.to_path_buf(),
            &paths,
            Renderer::Plain,
            ViewMode::SideBySide,
            width,
            MatchMode::CwdOnly,
        )
    }

    fn has_split_rows(app: &App) -> bool {
        app.lines.iter().any(|l| l.to_string().contains('│'))
    }

    fn input(session: &str) -> CardInput {
        CardInput {
            path: "f.txt".into(),
            file: PathBuf::from("/p/f.txt"),
            tool: "Edit".into(),
            ts: 0,
            agent: None,
            diff: diff::compute(&Before::Content(b"a\n".to_vec()), Some(b"b\n"), "f.txt"),
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
            .filter(|l| l.starts_with("── "))
            .collect()
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
        assert!(app.status(200).contains("session: all  cards: 4"));

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
        assert!(app.status(200).contains("session: s1  cards: 2/4"));
    }

    #[test]
    fn tab_with_no_cards_stays_on_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.cycle_session();
        assert_eq!(app.selected_session(), None);
        assert!(!app.has_visible_cards());
    }

    #[test]
    fn v_toggles_view_and_rerenders_cards() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path(), 120);
        app.push_card(input("s1"));
        app.rebuild_lines();
        assert!(app.status(200).contains("view: split"));
        assert!(has_split_rows(&app));
        app.on_key(KeyEvent::from(KeyCode::Char('v')));
        assert_eq!(app.view_mode(), ViewMode::Unified);
        assert!(app.status(200).contains("view: unified"));
        assert!(!has_split_rows(&app));
        app.on_key(KeyEvent::from(KeyCode::Char('v')));
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
}
