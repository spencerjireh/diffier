//! Drawing: a pinned card header, the visible feed lines, a one-line status
//! bar, and the help overlay.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, help_lines};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    if app.has_visible_cards() {
        // Row 0 always shows the current card's header so a long diff never
        // loses its file name; the feed scrolls in the rows below.
        let [pin, feed] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
        app.set_viewport(feed.height);
        if let Some(header) = app.sticky_header() {
            frame.render_widget(Paragraph::new(header), pin);
        }
        // When the header itself is the top line it is already pinned, so
        // start one line lower rather than drawing it twice.
        let lines = app.visible_lines();
        let lines = if app.at_card_start() {
            let end = (app.offset + 1 + feed.height as usize).min(app.total_lines());
            &app.all_lines()[app.offset + 1..end]
        } else {
            lines
        };
        frame.render_widget(Paragraph::new(Text::from(lines.to_vec())), feed);
    } else {
        app.set_viewport(body.height);
        let mut lines = vec![Line::from("")];
        if app.cards.is_empty() {
            lines.push(
                Line::from(format!(
                    "  Waiting for Claude Code edits in {}",
                    app.cwd().display()
                ))
                .dim(),
            );
            lines.push(Line::from(format!("  Spool: {}", app.spool_path().display())).dim());
            if !app.spool_exists() {
                lines.push(
                    Line::from(
                        "  No spool file yet. Run `diffier install` and restart Claude Code.",
                    )
                    .dim(),
                );
            }
        } else {
            lines.push(Line::from(format!("  No cards for session {}", app.session_label())).dim());
        }
        lines.push(Line::from(""));
        lines.push(Line::from("  Press ? for keys").dim());
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
    }

    frame.render_widget(
        Paragraph::new(app.status(status.width)).style(Style::new().reversed()),
        status,
    );

    if app.help {
        let lines = help_lines();
        let height = lines.len() as u16 + 2;
        let width = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 2;
        let area = centered(frame.area(), width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().title(" keys ")),
            area,
        );
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(h);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{app_at, input_named};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn row(term: &Terminal<TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn feed(dir: &std::path::Path) -> (Terminal<TestBackend>, App) {
        let mut app = app_at(dir, 80);
        for p in ["a.txt", "b.txt", "c.txt"] {
            app.push_card(input_named("s1", p));
        }
        app.rebuild_lines();
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        let term = Terminal::new(TestBackend::new(80, 6)).unwrap();
        (term, app)
    }

    #[test]
    fn header_not_duplicated_at_card_start() {
        let dir = tempfile::tempdir().unwrap();
        let (mut term, mut app) = feed(dir.path());
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(row(&term, 0).starts_with("── a.txt"), "{}", row(&term, 0));
        assert!(row(&term, 1).starts_with("── @@"), "{}", row(&term, 1));
        assert!(row(&term, 5).contains("card: 1/3"), "{}", row(&term, 5));
    }

    #[test]
    fn sticky_header_pinned_when_scrolled_into_card() {
        let dir = tempfile::tempdir().unwrap();
        let (mut term, mut app) = feed(dir.path());
        term.draw(|f| draw(f, &mut app)).unwrap();
        // Two lines into card b: its header is off screen but pinned.
        app.offset = 7;
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(row(&term, 0).starts_with("── b.txt"), "{}", row(&term, 0));
        assert!(!row(&term, 1).starts_with("── b.txt"), "{}", row(&term, 1));
        assert!(row(&term, 5).contains("card: 2/3"), "{}", row(&term, 5));
    }

    #[test]
    fn help_overlay_draws_key_table() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut app) = feed(dir.path());
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen: Vec<String> = (0..24).map(|y| row(&term, y)).collect();
        assert!(screen.iter().any(|l| l.contains(" keys ")), "{screen:#?}");
        assert!(
            screen.iter().any(|l| l.contains("collapse / expand card")),
            "{screen:#?}"
        );
    }

    #[test]
    fn empty_state_mentions_spool_and_help() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path(), 80);
        let mut term = Terminal::new(TestBackend::new(80, 8)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen: Vec<String> = (0..8).map(|y| row(&term, y)).collect();
        assert!(screen.iter().any(|l| l.contains("Spool: ")), "{screen:#?}");
        assert!(
            screen.iter().any(|l| l.contains("Press ? for keys")),
            "{screen:#?}"
        );
        assert!(screen[7].contains("card: 0/0"), "{}", screen[7]);
    }
}
