//! The answer form: asks for the answer to the question a blocked task's attempt asked,
//! covering the queue while it is open.

use ktask_core::TaskId;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::text::TextArea;

/// What the answer form's frame says at the bottom.
const ANSWER_KEYS: &str = " Ctrl-S answer · Esc cancel ";

/// What a key on the answer form asks the rest of the application to do, when it is not
/// something the form answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the form without answering anything.
    Close,
    /// Answer the task with this text.
    Submit(String),
}

/// The answer form's own state: which task is answered, the question its attempt asked, and
/// the answer being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnswerScreen {
    task: TaskId,
    question: String,
    text: TextArea,
}

impl AnswerScreen {
    /// An empty form for the question task `task`'s attempt asked.
    pub(crate) fn new(task: TaskId, question: String) -> Self {
        Self {
            task,
            question,
            text: TextArea::new(false),
        }
    }

    /// The task this form answers.
    pub(crate) fn task(&self) -> TaskId {
        self.task
    }

    /// The answer typed so far.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.text.text()
    }

    /// A key on the answer form: Esc closes it at once — there is no confirmation, unlike the
    /// removal question; anything else is typed into the answer.
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

    /// A Ctrl-letter on the answer form: `s` submits the typed answer, for the loop to record.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let text = self.text.text();
                (self, Some(Request::Submit(text)))
            }
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the answer form is showing.
    pub(crate) fn footer_keys() -> &'static str {
        ANSWER_KEYS
    }

    /// Draws the answer form over the whole of `area`: the question this task's attempt
    /// asked, wrapped to fit, over the top of it, and the answer field pinned to its last two
    /// rows. Returns where the cursor goes, in the answer.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let [info, prompt] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        let info_lines = vec![
            Line::styled(
                format!("Answer task #{}", self.task),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::default(),
            Line::from(self.question.clone()),
        ];
        Paragraph::new(info_lines)
            .wrap(Wrap { trim: false })
            .render(info, buf);
        let prompt_lines = vec![
            Line::from("Answer:"),
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
    fn a_new_form_holds_the_task_and_question_it_opened_on_with_an_empty_answer() {
        let screen = AnswerScreen::new(TaskId(1), "which path?".to_owned());
        assert_eq!(screen.task(), TaskId(1));
        assert_eq!(screen.text(), "");
    }

    #[test]
    fn typing_edits_the_answer() {
        let screen = "the left one".chars().fold(
            AnswerScreen::new(TaskId(1), "which path?".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        assert_eq!(screen.text(), "the left one");
    }

    #[test]
    fn esc_asks_to_close_the_form() {
        let (_, request) = AnswerScreen::new(TaskId(1), "q?".to_owned()).key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn ctrl_s_submits_the_typed_answer() {
        let screen = "the left one".chars().fold(
            AnswerScreen::new(TaskId(1), "q?".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        let (_, request) = screen.ctrl('s');
        assert_eq!(request, Some(Request::Submit("the left one".to_owned())));
    }

    #[test]
    fn other_ctrl_letters_do_nothing() {
        let screen = AnswerScreen::new(TaskId(1), "q?".to_owned());
        let (after, request) = screen.clone().ctrl('x');
        assert_eq!(after, screen);
        assert_eq!(request, None);
    }

    fn drawn(screen: &AnswerScreen, width: u16, height: u16) -> (Vec<String>, Position) {
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
    fn the_form_shows_the_task_the_question_and_the_answer_field_with_the_cursor_in_it() {
        let screen = AnswerScreen::new(TaskId(3), "which path?".to_owned());
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 0), "Answer task #3");
        assert_eq!(row(&rows, 2), "which path?");
        assert_eq!(row(&rows, 4), "Answer:");
        assert_eq!(row(&rows, 5), ">");
        assert_eq!(cursor, Position::new(2, 5));
    }

    #[test]
    fn typing_an_answer_shows_it_with_the_cursor_after_it() {
        let screen = "left".chars().fold(
            AnswerScreen::new(TaskId(3), "which path?".to_owned()),
            |screen, c| screen.key(KeyCode::Char(c)).0,
        );
        let (rows, cursor) = drawn(&screen, 60, 6);
        assert_eq!(row(&rows, 5), "> left");
        assert_eq!(cursor, Position::new(6, 5));
    }
}
