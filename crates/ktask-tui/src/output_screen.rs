//! The read-only, live provider-output screen.

use ktask_core::{StepTranscript, TaskId};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};

use crate::presentation::Transcript;
use crate::widgets::key_map;

const KEYS: [(&str, &str); 4] = [
    ("j, Down / k, Up", "move to the next / previous step"),
    ("l", "close the output"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map, or the output"),
];

/// A request made by the output screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Close,
}

/// Retained output for one selected task. The application refreshes the transcript while the
/// run is alive; this screen itself never touches the run or its files.
///
/// While no step is picked the screen follows the end of the output, so a running step is
/// always in view. Picking an earlier step puts that step's heading at the top of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputScreen {
    task: TaskId,
    transcript: Transcript,
    picked: Option<usize>,
    help: bool,
}

impl OutputScreen {
    pub(crate) fn new(task: TaskId) -> Self {
        Self {
            task,
            transcript: Transcript::default(),
            picked: None,
            help: false,
        }
    }
    pub(crate) fn task(&self) -> TaskId {
        self.task
    }
    pub(crate) fn refreshed(mut self, steps: &[StepTranscript]) -> Self {
        self.transcript = Transcript::new(steps);
        self.picked = self
            .picked
            .filter(|&index| index + 1 < self.transcript.steps());
        self
    }
    pub(crate) fn key(mut self, key: KeyCode) -> (Self, Option<Request>) {
        if self.help {
            self.help = !matches!(key, KeyCode::Esc | KeyCode::Char('?'));
            return (self, None);
        }
        match key {
            KeyCode::Char('?') => {
                self.help = true;
                (self, None)
            }
            KeyCode::Esc | KeyCode::Char('l') => (self, Some(Request::Close)),
            KeyCode::Char('j') | KeyCode::Down => (self.moved(1), None),
            KeyCode::Char('k') | KeyCode::Up => (self.moved(-1), None),
            _ => (self, None),
        }
    }

    /// Moves one step towards the end (`1`) or the start (`-1`). The last step is the one the
    /// screen follows, so reaching it picks nothing.
    fn moved(mut self, direction: isize) -> Self {
        let Some(last) = self.transcript.steps().checked_sub(1) else {
            return self;
        };
        let target = self
            .picked
            .unwrap_or(last)
            .saturating_add_signed(direction)
            .min(last);
        self.picked = (target < last).then_some(target);
        self
    }

    pub(crate) fn footer_keys() -> &'static str {
        " Esc, l close  j/k step  ? keys "
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if self.help {
            key_map(&KEYS, area, buf);
            return;
        }
        let text = self.transcript.text();
        let offset = self
            .picked
            .and_then(|index| self.transcript.heading_line(index))
            .unwrap_or_else(|| {
                text.lines()
                    .count()
                    .saturating_sub(usize::from(area.height))
            });
        Paragraph::new(if text.is_empty() {
            "Waiting for provider output…"
        } else {
            text
        })
        .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0))
        .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use ktask_core::{ProviderParser, StepTranscript};

    use super::*;

    fn steps(names: &[&str]) -> Vec<StepTranscript> {
        names
            .iter()
            .map(|name| {
                StepTranscript::new(name, "echo", None, ProviderParser::Plain, name.as_bytes())
            })
            .collect()
    }

    fn screen(names: &[&str]) -> OutputScreen {
        OutputScreen::new(TaskId(1)).refreshed(&steps(names))
    }

    fn press(mut screen: OutputScreen, keys: &[KeyCode]) -> OutputScreen {
        for key in keys {
            screen = screen.key(*key).0;
        }
        screen
    }

    #[test]
    fn the_keys_that_move_between_tasks_move_between_steps_and_the_last_step_is_followed() {
        let three = screen(&["a", "b", "c"]);
        assert_eq!(three.picked, None);
        assert_eq!(press(three.clone(), &[KeyCode::Char('k')]).picked, Some(1));
        assert_eq!(
            press(three.clone(), &[KeyCode::Up, KeyCode::Up]).picked,
            Some(0)
        );
        assert_eq!(
            press(three.clone(), &[KeyCode::Char('k'); 5]).picked,
            Some(0)
        );
        assert_eq!(
            press(three, &[KeyCode::Char('k'), KeyCode::Char('j')]).picked,
            None
        );
        assert_eq!(
            press(screen(&["a", "b", "c"]), &[KeyCode::Down]).picked,
            None
        );
    }

    #[test]
    fn a_screen_without_steps_ignores_the_movement_keys() {
        assert_eq!(
            press(screen(&[]), &[KeyCode::Char('k'), KeyCode::Char('j')]).picked,
            None
        );
    }

    #[test]
    fn a_refresh_that_leaves_the_picked_step_the_followed_one_follows_it() {
        let picked = press(screen(&["a", "b", "c"]), &[KeyCode::Char('k')]);
        assert_eq!(
            picked
                .clone()
                .refreshed(&steps(&["a", "b", "c", "d"]))
                .picked,
            Some(1)
        );
        assert_eq!(picked.refreshed(&steps(&["a", "b"])).picked, None);
    }

    #[test]
    fn question_mark_opens_the_key_map_which_swallows_other_keys_until_it_is_closed() {
        let open = press(screen(&["a", "b"]), &[KeyCode::Char('?')]);
        assert!(open.help);
        let still_open = press(open.clone(), &[KeyCode::Char('k'), KeyCode::Char('l')]);
        assert!(still_open.help);
        assert_eq!(still_open.picked, None);
        for close in [KeyCode::Char('?'), KeyCode::Esc] {
            let (closed, request) = open.clone().key(close);
            assert!(!closed.help);
            assert_eq!(request, None);
        }
    }
}
