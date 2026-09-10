//! TUI state: the card feed, scrolling, follow mode, and the spool tail loop.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::text::Line;

use crate::paths::Paths;
use crate::render::{EditCard, Renderer};
use crate::session::Pipeline;
use crate::spool::{self, SpoolTailer};

const WHEEL_STEP: usize = 3;

pub struct App {
    pub cards: Vec<EditCard>,
    lines: Vec<Line<'static>>,
    pub offset: usize,
    pub follow: bool,
    pub quit: bool,
    renderer: Renderer,
    cwd: PathBuf,
    width: u16,
    viewport: usize,
    pending_width: Option<u16>,
    pipeline: Pipeline,
    tailer: SpoolTailer,
    spool_path: PathBuf,
}

impl App {
    /// Replay the current session for `cwd` and prepare to tail.
    pub fn new(cwd: PathBuf, paths: &Paths, renderer: Renderer, width: u16) -> Self {
        let replay = spool::scan_replay(&paths.spool, &cwd);
        let mut pipeline = Pipeline::new(cwd.clone(), paths.snapshot_root.clone());
        let inputs = pipeline.replay(&replay.events);
        let tailer = SpoolTailer::new(paths.spool.clone(), replay.offset);
        let mut app = Self {
            cards: Vec::new(),
            lines: Vec::new(),
            offset: 0,
            follow: true,
            quit: false,
            renderer,
            cwd,
            width,
            viewport: 1,
            pending_width: None,
            pipeline,
            tailer,
            spool_path: paths.spool.clone(),
        };
        for input in inputs {
            let card = EditCard::new(input, &app.renderer, app.width, &app.cwd);
            app.cards.push(card);
        }
        app.rebuild_lines();
        app
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

    /// One iteration of background work: tail the spool, prune, apply resize.
    pub fn tick(&mut self) {
        if let Some(w) = self.pending_width.take()
            && w != self.width
        {
            self.width = w;
            // Only delta lays out to the width; the plain renderer does not, so
            // a resize there costs one pass over the headers instead of a delta
            // process per card.
            if self.renderer.is_delta() {
                for card in &mut self.cards {
                    card.rerender(&self.renderer, self.width, &self.cwd);
                }
            }
            self.rebuild_lines();
        }
        let events = self.tailer.poll();
        let mut added = false;
        for ev in &events {
            if let Some(input) = self.pipeline.handle(ev) {
                let card = EditCard::new(input, &self.renderer, self.width, &self.cwd);
                self.cards.push(card);
                added = true;
            }
        }
        if added {
            self.rebuild_lines();
        }
        self.pipeline.prune_pending(now_ms());
    }

    fn rebuild_lines(&mut self) {
        let mut lines = Vec::new();
        for card in &self.cards {
            lines.push(card.header_line(self.width));
            lines.extend(card.lines.iter().cloned());
            lines.push(Line::default());
        }
        self.lines = lines;
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

    /// Status bar text fitted to `width`: the cwd is elided from the left so
    /// the counters and key hints stay visible.
    pub fn status(&self, width: u16) -> String {
        let right = format!(
            "cards: {}  follow: {}  delta: {}  [j/k g/G p q]",
            self.cards.len(),
            if self.follow { "on" } else { "off" },
            if self.renderer.is_delta() {
                "yes"
            } else {
                "no"
            },
        );
        let mut cwd = self.cwd.to_string_lossy().into_owned();
        if let Some(home) = dirs::home_dir()
            && let Ok(rest) = self.cwd.strip_prefix(&home)
        {
            cwd = format!("~/{}", rest.display());
        }
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
