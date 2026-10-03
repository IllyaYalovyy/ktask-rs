//! The read-only, live provider-output screen.

use ktask_core::TaskId;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

/// A request made by the output screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Close,
}

/// Retained output for one selected task. The application refreshes `text` while the run is
/// alive; this screen itself never touches the run or its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputScreen {
    task: TaskId,
    text: String,
}

impl OutputScreen {
    pub(crate) fn new(task: TaskId) -> Self {
        Self {
            task,
            text: String::new(),
        }
    }
    pub(crate) fn task(&self) -> TaskId {
        self.task
    }
    pub(crate) fn refreshed(mut self, text: String) -> Self {
        self.text = text;
        self
    }
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc | KeyCode::Char('l') => (self, Some(Request::Close)),
            _ => (self, None),
        }
    }
    pub(crate) fn footer_keys() -> &'static str {
        " Esc, l close "
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.text.lines().count();
        let offset =
            u16::try_from(lines.saturating_sub(usize::from(area.height))).unwrap_or(u16::MAX);
        Paragraph::new(if self.text.is_empty() {
            "Waiting for provider output…"
        } else {
            &self.text
        })
        .scroll((offset, 0))
        .render(area, buf);
    }
}
