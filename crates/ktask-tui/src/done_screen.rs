//! The done form: asks for the reason a task is marked done by hand, covering the queue while
//! it is open.

use ktask_core::TaskId;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::text::TextArea;

/// What the done form's frame says at the bottom.
const DONE_KEYS: &str = " Ctrl-S mark done · Esc cancel ";

/// What a key on the done form asks the rest of the application to do, when it is not
/// something the form answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the form without marking anything done.
    Close,
    /// Mark the task done with this reason.
    Submit(String),
}

/// The done form's own state: which task it marks done, and the reason being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoneScreen {
    task: TaskId,
    text: TextArea,
}

impl DoneScreen {
    /// An empty form for marking `task` done.
    pub(crate) fn new(task: TaskId) -> Self {
        Self {
            task,
            text: TextArea::new(false),
        }
    }

    /// The task this form marks done.
    pub(crate) fn task(&self) -> TaskId {
        self.task
    }

    /// The reason typed so far.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.text.text()
    }

    /// A key on the done form: Esc closes it at once — there is no confirmation, unlike the
    /// removal question; anything else is typed into the reason.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc => (self, Some(Request::Close)),
            _ => (
                Self {
                    text: self.text.press(key),
                    ..self
                },
                None,
            ),
        }
    }

    /// A Ctrl-letter on the done form: `s` submits the typed reason, for the loop to record.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let text = self.text.text();
                (self, Some(Request::Submit(text)))
            }
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the done form is showing.
    pub(crate) fn footer_keys() -> &'static str {
        DONE_KEYS
    }

    /// Draws the done form over the whole of `area`: the task it marks done over the top of
    /// it, and the reason field pinned to its last two rows. Returns where the cursor goes, in
    /// the reason.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let [info, prompt] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        let info_lines = vec![Line::styled(
            format!("Mark task #{} done", self.task),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        Paragraph::new(info_lines).render(info, buf);
        let prompt_lines = vec![
            Line::from("Reason:"),
            Line::from(format!("> {}", self.text.text())),
        ];
        Paragraph::new(prompt_lines).render(prompt, buf);
        Position::new(
            (prompt.x + 2 + u16::try_from(self.text.cursor().1).unwrap_or(u16::MAX))
                .min(prompt.x + prompt.width.saturating_sub(1)),
            prompt.y + 1,
        )
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn a_new_form_holds_the_task_it_opened_on_with_an_empty_reason() {
        let screen = DoneScreen::new(TaskId(1));
        assert_eq!(screen.task(), TaskId(1));
        assert_eq!(screen.text(), "");
    }

    #[test]
    fn typing_edits_the_reason() {
        let screen = "fixed by hand"
            .chars()
            .fold(DoneScreen::new(TaskId(1)), |screen, c| {
                screen.key(KeyCode::Char(c)).0
            });
        assert_eq!(screen.text(), "fixed by hand");
    }

    #[test]
    fn esc_asks_to_close_the_form() {
        let (_, request) = DoneScreen::new(TaskId(1)).key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn ctrl_s_submits_the_typed_reason() {
        let screen = "fixed by hand"
            .chars()
            .fold(DoneScreen::new(TaskId(1)), |screen, c| {
                screen.key(KeyCode::Char(c)).0
            });
        let (_, request) = screen.ctrl('s');
        assert_eq!(request, Some(Request::Submit("fixed by hand".to_owned())));
    }

    #[test]
    fn other_ctrl_letters_do_nothing() {
        let screen = DoneScreen::new(TaskId(1));
        let (after, request) = screen.clone().ctrl('x');
        assert_eq!(after, screen);
        assert_eq!(request, None);
    }

    fn drawn(screen: &DoneScreen, width: u16, height: u16) -> (Vec<String>, Position) {
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
    fn the_form_shows_the_task_and_the_reason_field_with_the_cursor_in_it() {
        let screen = DoneScreen::new(TaskId(3));
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 0), "Mark task #3 done");
        assert_eq!(row(&rows, 4), "Reason:");
        assert_eq!(row(&rows, 5), ">");
        assert_eq!(cursor, Position::new(2, 5));
    }

    #[test]
    fn typing_a_reason_shows_it_with_the_cursor_after_it() {
        let screen = "fixed"
            .chars()
            .fold(DoneScreen::new(TaskId(3)), |screen, c| {
                screen.key(KeyCode::Char(c)).0
            });
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 5), "> fixed");
        assert_eq!(cursor, Position::new(7, 5));
    }
}
