//! Draws the settings screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::scroll::first_shown;
use crate::settings_form::{SettingField, SettingsForm};

/// The field's marker, ahead of the typed value, when it has the focus.
const FOCUSED_PREFIX: &str = "> ";
/// The field's marker, ahead of the typed value, when it does not have the focus.
const UNFOCUSED_PREFIX: &str = "  ";

/// How many lines one field's own block takes: a blank line, its label, and its value.
const FIELD_HEIGHT: usize = 3;

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
/// has the focus, scrolled into view when the terminal is too short to show every field at
/// once.
pub(crate) fn draw(form: &SettingsForm, area: Rect, buf: &mut Buffer) -> Position {
    let header = header_lines(form);
    let header_height = u16::try_from(header.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let [header_area, fields_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
    Paragraph::new(header).render(header_area, buf);

    let (lines, cursor) = field_lines_and_cursor(form, fields_area);
    Paragraph::new(lines).render(fields_area, buf);
    cursor
}

/// "Settings", plus the last submission's refusal above the fields, when there is one.
fn header_lines(form: &SettingsForm) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut header = vec![Line::styled("Settings", bold)];
    if let Some(problem) = &form.problem {
        header.push(Line::styled(format!("! {problem}"), bold));
    }
    header
}

/// Every field's own lines that fit `fields_area`, scrolled so the focused one is always
/// among them, and where its cursor goes.
fn field_lines_and_cursor(
    form: &SettingsForm,
    fields_area: Rect,
) -> (Vec<Line<'static>>, Position) {
    let mut cursor = Position::new(fields_area.x, fields_area.y);
    if form.fields.is_empty() {
        return (Vec::new(), cursor);
    }
    let heights = vec![FIELD_HEIGHT; form.fields.len()];
    let first = first_shown(&heights, Some(form.focus), usize::from(fields_area.height));

    let mut lines = Vec::new();
    for (index, field) in form.fields.iter().enumerate().skip(first) {
        if usize::from(fields_area.height).saturating_sub(lines.len()) < FIELD_HEIGHT {
            break;
        }
        let focused = index == form.focus;
        push_field(&mut lines, field, focused);
        if focused {
            cursor = field_cursor(fields_area, lines.len() - 1, field);
        }
    }
    (lines, cursor)
}

/// Pushes one field's own three lines onto `lines`: a blank line, its label with whether it is
/// still the default, and its value marked with the focus or not.
fn push_field(lines: &mut Vec<Line<'static>>, field: &SettingField, focused: bool) {
    let kind = if field.is_default {
        "default"
    } else {
        "custom"
    };
    lines.push(Line::default());
    lines.push(Line::from(format!("{} ({kind}):", label(field.name))));
    let prefix = if focused {
        FOCUSED_PREFIX
    } else {
        UNFOCUSED_PREFIX
    };
    lines.push(Line::from(format!("{prefix}{}", field.text.text())));
}

/// Where the cursor goes for the focused `field`, whose value sits at `row` inside
/// `fields_area`.
fn field_cursor(fields_area: Rect, row: usize, field: &SettingField) -> Position {
    let col = FOCUSED_PREFIX.chars().count() + field.text.cursor().1;
    Position::new(
        fields_area.x
            + u16::try_from(col)
                .unwrap_or(u16::MAX)
                .min(fields_area.width.saturating_sub(1)),
        fields_area.y
            + u16::try_from(row)
                .unwrap_or(u16::MAX)
                .min(fields_area.height.saturating_sub(1)),
    )
}
