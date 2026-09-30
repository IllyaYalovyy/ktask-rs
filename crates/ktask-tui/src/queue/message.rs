//! Showing the last run's or import's own report in place of the task list, or a run's
//! refusal to start beside it, and scrolling either one.

use ratatui::crossterm::event::KeyCode;

use super::{Queue, Request};

impl Queue {
    /// The screen once a run or an import this screen started has finished, showing `text` —
    /// its own report, the same words `ktask-rs run` or `ktask-rs import` itself would print —
    /// in place of the task list.
    pub(crate) fn shown_message(self, text: &str) -> Self {
        Self {
            message: Some(text.lines().map(str::to_owned).collect()),
            message_offset: 0,
            run_refusal: None,
            ..self
        }
    }

    /// The screen once a run this screen started has ended, or refused to start: `text`, the
    /// same words `ktask-rs run` itself printed, either way. A run that attempted nothing —
    /// recognised by [`is_start_refusal`] — shows `text` beside the task list; anything else
    /// shows `text` in place of the task list, the same as an import's own report.
    pub(crate) fn run_message(self, text: String) -> Self {
        if is_start_refusal(&text) {
            Self {
                run_refusal: Some(text),
                message: None,
                message_offset: 0,
                ..self
            }
        } else {
            self.shown_message(&text)
        }
    }

    /// A key while the last run's or import's results are shown in place of the task list:
    /// `j`/`Down` and `k`/`Up` scroll one line, `g`/`G` jump to the first or last line, and any
    /// other key dismisses the results, then is handled as it would be on the plain queue
    /// screen — so, for example, `r` both dismisses a shown message and starts a fresh run.
    pub(super) fn message_key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Char('j') | KeyCode::Down => (self.scroll_message(1), None),
            KeyCode::Char('k') | KeyCode::Up => (self.scroll_message(-1), None),
            KeyCode::Char('g') => (
                Self {
                    message_offset: 0,
                    ..self
                },
                None,
            ),
            KeyCode::Char('G') => {
                let offset = self.last_message_line();
                (
                    Self {
                        message_offset: offset,
                        ..self
                    },
                    None,
                )
            }
            _ => Self {
                message: None,
                message_offset: 0,
                ..self
            }
            .plain_key(key),
        }
    }

    /// A key while a run's refusal to start is shown beside the task list: `j`/`Down`,
    /// `k`/`Up`, `g` and `G` move the selection exactly as they would on the plain queue screen
    /// — the refusal stays shown — and any other key dismisses the refusal, then is handled as
    /// it would be on the plain queue screen.
    pub(super) fn run_refusal_key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Char('j' | 'k' | 'g' | 'G') | KeyCode::Down | KeyCode::Up => {
                self.plain_key(key)
            }
            _ => Self {
                run_refusal: None,
                ..self
            }
            .plain_key(key),
        }
    }

    /// The index of `message`'s last line, or `0` when there is none.
    fn last_message_line(&self) -> usize {
        self.message
            .as_ref()
            .map_or(0, |lines| lines.len().saturating_sub(1))
    }

    /// `message_offset` moved by `delta`, clamped to stay within the message's lines.
    fn scroll_message(self, delta: isize) -> Self {
        let offset = self
            .message_offset
            .saturating_add_signed(delta)
            .min(self.last_message_line());
        Self {
            message_offset: offset,
            ..self
        }
    }
}

/// Whether `text` is what a run prints when it refuses to start without attempting any task:
/// an earlier task left unfinished (ends with `"; run did not start"`), another run already in
/// progress (contains `"a run is already in progress"`), or nothing left pending (exactly
/// `"nothing is pending"`) — the same words `ktask-rs run` itself gives for each.
fn is_start_refusal(text: &str) -> bool {
    text == "nothing is pending"
        || text.contains("a run is already in progress")
        || text.ends_with("; run did not start")
}
