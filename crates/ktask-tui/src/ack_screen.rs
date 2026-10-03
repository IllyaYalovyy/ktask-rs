//! The acknowledgement form: records an optional message while completing a human task.

use ktask_core::TaskId;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::text::TextArea;

/// What the acknowledgement form's frame says at the bottom.
const ACK_KEYS: &str = " Ctrl-S acknowledge · Esc cancel ";

/// What a key on the acknowledgement form asks the application to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the form without acknowledging anything.
    Close,
    /// Acknowledge the task with this optional message.
    Submit(String),
}

/// The acknowledgement form's state: which task it acknowledges and the optional message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AckScreen {
    task: TaskId,
    text: TextArea,
}

impl AckScreen {
    /// An empty form for acknowledging `task`.
    pub(crate) fn new(task: TaskId) -> Self {
        Self {
            task,
            text: TextArea::new(false),
        }
    }

    /// The task this form acknowledges.
    pub(crate) fn task(&self) -> TaskId {
        self.task
    }

    /// A key on the form: Esc closes it; every other key edits the optional message.
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

    /// Ctrl-S submits the acknowledgement, with the message as typed.
    pub(crate) fn ctrl(self, letter: char) -> (Self, Option<Request>) {
        match letter {
            's' => {
                let text = self.text.text();
                (self, Some(Request::Submit(text)))
            }
            _ => (self, None),
        }
    }

    /// What the frame's bottom border says while the form is showing.
    pub(crate) fn footer_keys() -> &'static str {
        ACK_KEYS
    }

    /// Draws the form and returns the optional-message cursor position.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Position {
        let [info, prompt] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
        Paragraph::new(Line::styled(
            format!("Acknowledge human task #{}", self.task),
            Style::new().add_modifier(Modifier::BOLD),
        ))
        .render(info, buf);
        Paragraph::new(vec![
            Line::from("Message (optional):"),
            Line::from(format!("> {}", self.text.text())),
        ])
        .render(prompt, buf);
        Position::new(
            (prompt.x + 2 + u16::try_from(self.text.cursor().1).unwrap_or(u16::MAX))
                .min(prompt.x + prompt.width.saturating_sub(1)),
            prompt.y + 1,
        )
    }
}
