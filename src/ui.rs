//! Drawing: a body of visible feed lines and a one-line status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    app.set_viewport(body.height);

    if app.cards.is_empty() {
        let mut lines = vec![
            Line::from(""),
            Line::from(format!(
                "  Waiting for Claude Code edits in {}",
                app.cwd().display()
            ))
            .dim(),
        ];
        if !app.spool_exists() {
            lines.push(
                Line::from("  No spool file yet. Run `diffier install` and restart Claude Code.")
                    .dim(),
            );
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
    } else {
        frame.render_widget(
            Paragraph::new(Text::from(app.visible_lines().to_vec())),
            body,
        );
    }

    frame.render_widget(
        Paragraph::new(app.status(status.width)).style(Style::new().reversed()),
        status,
    );
}
