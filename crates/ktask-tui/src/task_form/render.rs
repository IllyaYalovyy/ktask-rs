//! Drawing the task form: its heading, the discard question or the problems from a refused
//! submission, and every field — the one the focus is on marked, its text scrolled sideways
//! to keep the cursor in view.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::text::TextArea;
use crate::widgets::key_map;

use super::{Focus, Form, Placement, TaskFormScreen};

/// The keys shown by `?` while the discard question is asking.
const DISCARD_QUESTION_KEYS: [(&str, &str); 2] = [("y", "discard it"), ("n, Esc", "keep writing")];

impl TaskFormScreen {
    /// Draws the task form over the whole of `area`, and returns where the cursor goes.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if self.discarding && self.help {
            key_map(&DISCARD_QUESTION_KEYS, area, buf);
            return None;
        }
        draw_form(&self.form, self.discarding, area, buf)
    }
}

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

/// `text` broken into rows of at most `width` characters, at spaces where it can be.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let row = rows.last_mut().map_or(0, |row| row.chars().count());
            let separator = usize::from(row > 0);
            if row + separator + word.len() <= width {
                break;
            }
            if row > 0 {
                rows.push(String::new());
            } else {
                // A word longer than a row is cut.
                let rest = word.split_off(width.min(word.len()));
                if let Some(last) = rows.last_mut() {
                    last.extend(word);
                }
                rows.push(String::new());
                word = rest;
            }
        }
        if let Some(row) = rows.last_mut() {
            if !row.is_empty() {
                row.push(' ');
            }
            row.extend(word);
        }
    }
    rows
}

/// The marker in front of the field the focus is on.
fn marker(form: &Form, field: Focus) -> char {
    if form.focus == field { '>' } else { ' ' }
}

/// What the form is for, and where the task goes when it is not at the end.
fn heading(placement: Placement) -> String {
    match placement {
        Placement::End => "New task".to_owned(),
        Placement::Before(id) => format!("New task above #{id}"),
        Placement::After(id) => format!("New task below #{id}"),
    }
}

/// Pushes the discard notice, when `discard` asks for it, then every problem the form's last
/// submission found, each wrapped to `sheet`'s width.
fn push_problems(sheet: &mut Sheet, form: &Form, discard: bool, bold: Style) {
    if discard {
        sheet.rows.push(Line::styled(
            "Discard this task? y to discard · n or Esc to keep writing",
            bold,
        ));
    }
    for problem in &form.problems {
        for (index, row) in wrap(problem, sheet.width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if index == 0 { "! " } else { "  " };
            sheet.rows.push(Line::styled(format!("{lead}{row}"), bold));
        }
    }
}

/// Pushes the kind field: its value between angle brackets, with a hint on how to change it
/// while the focus is on it.
fn push_kind(sheet: &mut Sheet, form: &Form) {
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
        marker(form, Focus::Kind),
        form.kind
    ));
}

/// Pushes the criteria field: one row per criterion, or a hint when there are none yet.
fn push_criteria(sheet: &mut Sheet, form: &Form) {
    sheet.push("  Criteria:".to_owned());
    if form.criteria.is_empty() {
        sheet.push("    none: Ctrl-N adds one".to_owned());
    }
    for (index, criterion) in form.criteria.iter().enumerate() {
        let focus = Focus::Criterion(index);
        sheet.text(
            &format!("{}{:>3}. ", marker(form, focus), index + 1),
            "      ",
            criterion,
            form.focus == focus,
        );
    }
}

/// Pushes `form`'s own fields — title, kind, links, body and criteria — each marked with the
/// focus when it is on that field.
fn push_fields(sheet: &mut Sheet, form: &Form) {
    let mark = |field| marker(form, field);
    sheet.text(
        &format!("{} Title:     ", mark(Focus::Title)),
        "",
        &form.title,
        form.focus == Focus::Title,
    );
    push_kind(sheet, form);
    sheet.text(
        &format!("{} Links:     ", mark(Focus::Links)),
        "",
        &form.links,
        form.focus == Focus::Links,
    );
    sheet.push(format!("{} Body:", mark(Focus::Body)));
    sheet.text("    ", "    ", &form.body, form.focus == Focus::Body);
    push_criteria(sheet, form);
    if !form.provider.text().is_empty()
        || !form.model.text().is_empty()
        || matches!(form.focus, Focus::Provider | Focus::Model)
    {
        sheet.text(
            &format!("{} Provider:  ", mark(Focus::Provider)),
            "",
            &form.provider,
            form.focus == Focus::Provider,
        );
        sheet.text(
            &format!("{} Model:     ", mark(Focus::Model)),
            "",
            &form.model,
            form.focus == Focus::Model,
        );
    }
}

/// Draws `form` over the whole of `area`, asking to discard it when `discard` says so, and
/// returns where the cursor goes.
fn draw_form(form: &Form, discard: bool, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut sheet = Sheet {
        width: usize::from(area.width),
        rows: vec![Line::styled(heading(form.placement), bold)],
        focus_row: 0,
        cursor_x: None,
    };
    push_problems(&mut sheet, form, discard, bold);
    sheet.push(String::new());
    push_fields(&mut sheet, form);

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

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyCode;

    use super::*;

    fn typed(screen: TaskFormScreen, text: &str) -> TaskFormScreen {
        text.chars().fold(screen, |screen, c| {
            screen
                .key(if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                })
                .0
        })
    }

    fn draw_screen(
        screen: &TaskFormScreen,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<Position>) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let cursor = screen.draw(area, &mut buf);
        let rows = (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        (rows, cursor)
    }

    fn row(rows: &[String], y: usize) -> &str {
        rows[y].trim_end()
    }

    #[test]
    fn wrap_breaks_text_at_spaces_and_no_row_is_longer_than_the_width() {
        assert_eq!(wrap("a short one", 11), ["a short one"]);
        assert_eq!(wrap("aaa bbb ccc dd", 7), ["aaa bbb", "ccc dd"]);
        assert_eq!(wrap("abcdefgh x", 3), ["abc", "def", "gh", "x"]);
        assert_eq!(wrap("", 5), [""]);
    }

    #[test]
    fn the_form_shows_its_fields_and_puts_the_cursor_in_the_title() {
        let screen = TaskFormScreen::new(Placement::End);
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        assert_eq!(row(&rows, 0), "New task");
        assert_eq!(row(&rows, 2), "> Title:");
        assert_eq!(row(&rows, 3), "  Kind:      < agent >");
        assert_eq!(row(&rows, 4), "  Links:");
        assert_eq!(row(&rows, 5), "  Body:");
        assert_eq!(row(&rows, 7), "  Criteria:");
        assert_eq!(cursor, Some(Position::new(13, 2)));
    }

    #[test]
    fn a_long_title_scrolls_sideways_so_that_the_cursor_stays_on_the_screen() {
        let screen = typed(TaskFormScreen::new(Placement::End), &"x".repeat(70));
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        let cursor = cursor.expect("the cursor is shown");
        assert!(cursor.x < 60, "{cursor:?}");
        assert!(row(&rows, 2).ends_with('x'), "{rows:?}");
    }

    #[test]
    fn discarding_shows_the_question_and_the_typed_title_stays_visible() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title").start_discard();
        let (rows, _) = draw_screen(&screen, 60, 12);
        assert_eq!(
            row(&rows, 1),
            "Discard this task? y to discard · n or Esc to keep writing"
        );
        assert!(row(&rows, 3).ends_with("Title"), "{rows:?}");
    }

    #[test]
    fn discarding_with_help_open_shows_the_discard_questions_own_key_map() {
        let screen = typed(TaskFormScreen::new(Placement::End), "Title")
            .start_discard()
            .key(KeyCode::Char('?'))
            .0;
        let (rows, cursor) = draw_screen(&screen, 60, 12);
        let screen_text = rows.join("\n");
        assert!(screen_text.contains("discard it"), "{screen_text}");
        assert!(!screen_text.contains("Title"), "{screen_text}");
        assert_eq!(cursor, None);
    }
}
