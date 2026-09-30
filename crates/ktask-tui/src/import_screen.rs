//! The import form: asks for the path of a file of tasks to import, covering the queue while
//! it is open.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::text::TextArea;

/// What the import form's frame says at the bottom.
const IMPORT_KEYS: &str = " Ctrl-S import · Esc cancel ";

/// What a key on the import form asks the rest of the application to do, when it is not
/// something the form answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the form without importing anything.
    Close,
    /// Import the file at this path.
    Submit(String),
}

/// The import form's own state: the file path being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportScreen {
    path: TextArea,
}

impl ImportScreen {
    /// An empty form.
    pub(crate) fn new() -> Self {
        Self {
            path: TextArea::new(false),
        }
    }

    /// The path typed so far.
    #[cfg(test)]
    pub(crate) fn path_text(&self) -> String {
        self.path.text()
    }

    /// A key on the import form: Esc closes it at once — there is no confirmation, unlike the
    /// task form — anything else is typed into the path.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc => (self, Some(Request::Close)),
            _ => (
                Self {
                    path: self.path.press(key),
                },
                None,
            ),
        }
    }

    /// A Ctrl-letter on the import form: `s` submits the typed path, for the loop to import.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let path = self.path.text();
                (self, Some(Request::Submit(path)))
            }
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the import form is showing.
    pub(crate) fn footer_keys() -> &'static str {
        IMPORT_KEYS
    }

    /// Draws the import form over the whole of `area`: a prompt and the path field, and
    /// returns where the cursor goes, in the path.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let lines = vec![
            Line::styled("Import tasks", Style::new().add_modifier(Modifier::BOLD)),
            Line::default(),
            Line::from("File path:"),
            Line::from(format!("> {}", self.path.text())),
        ];
        let cursor = Position::new(
            (area.x + 2 + u16::try_from(self.path.cursor().1).unwrap_or(u16::MAX))
                .min(area.x + area.width.saturating_sub(1)),
            area.y + 3,
        );
        Paragraph::new(lines).render(area, buf);
        cursor
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn a_new_form_has_an_empty_path() {
        assert_eq!(ImportScreen::new().path_text(), "");
    }

    #[test]
    fn typing_edits_the_path() {
        let screen = "/tmp/tasks.json"
            .chars()
            .fold(ImportScreen::new(), |screen, c| {
                screen.key(KeyCode::Char(c)).0
            });
        assert_eq!(screen.path_text(), "/tmp/tasks.json");
    }

    #[test]
    fn esc_asks_to_close_the_form() {
        let (_, request) = ImportScreen::new().key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn ctrl_s_submits_the_typed_path() {
        let screen = "/tmp/x".chars().fold(ImportScreen::new(), |screen, c| {
            screen.key(KeyCode::Char(c)).0
        });
        let (_, request) = screen.ctrl('s');
        assert_eq!(request, Some(Request::Submit("/tmp/x".to_owned())));
    }

    #[test]
    fn other_ctrl_letters_do_nothing() {
        let (screen, request) = ImportScreen::new().ctrl('x');
        assert_eq!(screen, ImportScreen::new());
        assert_eq!(request, None);
    }

    fn drawn(screen: &ImportScreen, width: u16, height: u16) -> (Vec<String>, Position) {
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
    fn the_form_shows_a_prompt_and_the_path_field_with_the_cursor_in_it() {
        let (rows, cursor) = drawn(&ImportScreen::new(), 60, 4);
        assert_eq!(row(&rows, 0), "Import tasks");
        assert_eq!(row(&rows, 2), "File path:");
        assert_eq!(row(&rows, 3), ">");
        assert_eq!(cursor, Position::new(2, 3));
    }

    #[test]
    fn typing_a_path_shows_it_with_the_cursor_after_it() {
        let screen = "/tmp/x".chars().fold(ImportScreen::new(), |screen, c| {
            screen.key(KeyCode::Char(c)).0
        });
        let (rows, cursor) = drawn(&screen, 60, 4);
        assert_eq!(row(&rows, 3), "> /tmp/x");
        assert_eq!(cursor, Position::new(8, 3));
    }
}
