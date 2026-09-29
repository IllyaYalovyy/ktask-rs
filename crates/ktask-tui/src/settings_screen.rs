//! Draws the settings screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::settings_form::SettingsForm;

/// The field's marker, ahead of the typed value.
const PREFIX: &str = "> ";

/// Draws `form` over the whole of `area`, and returns where the cursor goes.
pub(crate) fn draw(form: &SettingsForm, area: Rect, buf: &mut Buffer) -> Position {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled("Settings", bold)];
    if let Some(problem) = &form.problem {
        lines.push(Line::styled(format!("! {problem}"), bold));
    }
    lines.push(Line::default());
    let kind = if form.is_default { "default" } else { "custom" };
    lines.push(Line::from(format!("Attempt timeout, in seconds ({kind}):")));
    lines.push(Line::from(format!(
        "{PREFIX}{}",
        form.attempt_timeout.text()
    )));

    let cursor_row = lines.len() - 1;
    let cursor_col = PREFIX.chars().count() + form.attempt_timeout.cursor().1;
    Paragraph::new(lines).render(area, buf);
    let x = u16::try_from(cursor_col)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(1));
    let y = u16::try_from(cursor_row)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(1));
    Position::new(area.x + x, area.y + y)
}
