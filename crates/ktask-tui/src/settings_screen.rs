//! Draws the settings screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::settings_form::SettingsForm;

/// The field's marker, ahead of the typed value, when it has the focus.
const FOCUSED_PREFIX: &str = "> ";
/// The field's marker, ahead of the typed value, when it does not have the focus.
const UNFOCUSED_PREFIX: &str = "  ";

/// The label a setting's own field is shown under, by name.
fn label(name: &str) -> &'static str {
    match name {
        "health-check" => "Health check command",
        "tracked-branch" => "Tracked branch (remote/branch)",
        "step-sync" => "Sync step (on/off)",
        "step-health-check" => "Health check step (on/off)",
        "step-review" => "Review step (on/off)",
        "step-testing" => "Testing step (on/off)",
        "step-commit" => "Commit step (on/off)",
        "step-push" => "Push step (on/off)",
        _ => "Attempt timeout, in seconds",
    }
}

/// Draws `form` over the whole of `area`, and returns where the cursor goes: in the field that
/// has the focus.
pub(crate) fn draw(form: &SettingsForm, area: Rect, buf: &mut Buffer) -> Position {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled("Settings", bold)];
    if let Some(problem) = &form.problem {
        lines.push(Line::styled(format!("! {problem}"), bold));
    }
    let mut cursor = Position::new(area.x, area.y);
    for (index, field) in form.fields.iter().enumerate() {
        lines.push(Line::default());
        let kind = if field.is_default {
            "default"
        } else {
            "custom"
        };
        lines.push(Line::from(format!("{} ({kind}):", label(field.name))));
        let focused = index == form.focus;
        let prefix = if focused {
            FOCUSED_PREFIX
        } else {
            UNFOCUSED_PREFIX
        };
        lines.push(Line::from(format!("{prefix}{}", field.text.text())));
        if focused {
            let row = lines.len() - 1;
            let col = prefix.chars().count() + field.text.cursor().1;
            cursor = Position::new(
                area.x
                    + u16::try_from(col)
                        .unwrap_or(u16::MAX)
                        .min(area.width.saturating_sub(1)),
                area.y
                    + u16::try_from(row)
                        .unwrap_or(u16::MAX)
                        .min(area.height.saturating_sub(1)),
            );
        }
    }
    Paragraph::new(lines).render(area, buf);
    cursor
}
