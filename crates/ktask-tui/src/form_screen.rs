//! Draws the form a new task is written in.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::form::{Focus, Form};
use crate::text::TextArea;

/// The rows of the form so far, and where the focus is in them.
struct Sheet {
    width: usize,
    rows: Vec<Line<'static>>,
    /// The row the focus is on, which the screen scrolls to keep in view.
    focus_row: usize,
    /// The column of the cursor in that row, when the focus is on text.
    cursor_x: Option<usize>,
}

impl Sheet {
    fn push(&mut self, text: String) {
        self.rows.push(Line::from(text));
    }

    /// Adds the rows of `area`, the first behind `first` and the others behind `rest`; when
    /// `focused` the cursor is on one of them, and a long line scrolls sideways to keep it
    /// in view.
    fn text(&mut self, first: &str, rest: &str, area: &TextArea, focused: bool) {
        let (cursor_row, cursor_col) = area.cursor();
        for (index, line) in area.lines().iter().enumerate() {
            let prefix = if index == 0 { first } else { rest };
            let mut shown: Vec<char> = line.chars().collect();
            if focused && index == cursor_row {
                let room = self.width.saturating_sub(prefix.chars().count()).max(1);
                let skip = (cursor_col + 1).saturating_sub(room);
                shown.drain(..skip.min(shown.len()));
                let before: String = line.chars().skip(skip).take(cursor_col - skip).collect();
                self.focus_row = self.rows.len();
                self.cursor_x = Some(Line::from(format!("{prefix}{before}")).width());
            }
            self.push(format!("{prefix}{}", shown.into_iter().collect::<String>()));
        }
    }
}

/// The marker in front of the field the focus is on.
fn marker(form: &Form, field: Focus) -> char {
    if form.focus == field { '>' } else { ' ' }
}

/// Draws `form` over the whole of `area`, and returns where the cursor goes.
pub(crate) fn draw(form: &Form, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut sheet = Sheet {
        width: usize::from(area.width),
        rows: vec![Line::styled("New task", bold)],
        focus_row: 0,
        cursor_x: None,
    };
    sheet.rows.extend(
        form.problems
            .iter()
            .map(|problem| Line::styled(format!("! {problem}"), bold)),
    );
    sheet.push(String::new());

    let mark = |field| marker(form, field);
    sheet.text(
        &format!("{} Title:     ", mark(Focus::Title)),
        "",
        &form.title,
        form.focus == Focus::Title,
    );
    if form.focus == Focus::Kind {
        sheet.focus_row = sheet.rows.len();
    }
    let hint = if form.focus == Focus::Kind {
        "   Left, Right or Space to change"
    } else {
        ""
    };
    sheet.push(format!(
        "{} Kind:      < {} >{hint}",
        mark(Focus::Kind),
        form.kind
    ));
    sheet.text(
        &format!("{} Links:     ", mark(Focus::Links)),
        "",
        &form.links,
        form.focus == Focus::Links,
    );
    sheet.push(format!("{} Body:", mark(Focus::Body)));
    sheet.text("    ", "    ", &form.body, form.focus == Focus::Body);
    sheet.push("  Criteria:".to_owned());
    if form.criteria.is_empty() {
        sheet.push("    none: Ctrl-N adds one".to_owned());
    }
    for (index, criterion) in form.criteria.iter().enumerate() {
        let focus = Focus::Criterion(index);
        sheet.text(
            &format!("{}{:>3}. ", mark(focus), index + 1),
            "      ",
            criterion,
            form.focus == focus,
        );
    }

    let height = usize::from(area.height);
    let first = (sheet.focus_row + 1).saturating_sub(height);
    let visible: Vec<Line<'static>> = sheet.rows.into_iter().skip(first).take(height).collect();
    Paragraph::new(visible).render(area, buf);
    let x = u16::try_from(sheet.cursor_x?)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(1));
    let y = u16::try_from(sheet.focus_row - first).unwrap_or(u16::MAX);
    Some(Position::new(area.x + x, area.y + y))
}
