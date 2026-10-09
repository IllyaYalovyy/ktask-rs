//! The read-only task-detail screen: every fact [`ktask_core::TaskDetail`] carries, wrapped to
//! the screen's width and scrollable with j/k — never elided, the way the queue screen's own
//! attempt line is.

use ktask_core::{TaskDetail, TaskId, TaskKind, TaskStatus};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::presentation::{DetailLine, detail_lines};
use crate::scroll::first_shown;
use crate::widgets::key_map;
use crate::wrap::wrap;

const KEYS: [(&str, &str); 9] = [
    ("j, Down / k, Up", "scroll"),
    ("g / G", "top / bottom"),
    ("l", "open the selected attempt's output"),
    (
        "t",
        "retry the task, once it is failed, failed-unknown or blocked",
    ),
    ("A", "answer the task's question, once it is blocked"),
    ("D", "mark the task done by hand"),
    ("H", "acknowledge the task, once it is a pending human task"),
    ("?", "show or hide this key map"),
    ("Esc", "back to the queue"),
];

/// A request made by the detail screen — every one of them, but [`Request::Close`] and
/// [`Request::OpenOutput`], the very ones the queue screen answers for the same key, pressed
/// on the task this screen opened on, so the loop carries them out exactly as it would from
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Back to the queue, the same task selected.
    Close,
    /// Open the output of this attempt, `None` naming the latest, followed live.
    OpenOutput(Option<u32>),
    /// Retry the task: it is `failed`, `failed-unknown` or `blocked` already.
    Retry,
    /// Open the answer form, with the question the task's attempt asked.
    OpenAnswer(String),
    /// Open the done form, to mark the task done by hand.
    OpenDone,
    /// Open the acknowledgement form: the task is a pending human one.
    OpenAcknowledge,
}

/// The detail screen's own state: every line [`crate::presentation::detail_lines`] built for
/// the task it opened on, which one the selection — and so scrolling — is on, and the task's
/// own status, kind and blocked question, so `t`, `A`, `D` and `H` act on it exactly as they
/// would on the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailScreen {
    task: TaskId,
    lines: Vec<DetailLine>,
    selected: usize,
    help: bool,
    status: TaskStatus,
    kind: TaskKind,
    /// The question the task's attempt asked, when it is `blocked`.
    question: Option<String>,
}

impl DetailScreen {
    pub(crate) fn new(task: TaskId) -> Self {
        Self {
            task,
            lines: Vec::new(),
            selected: 0,
            help: false,
            status: TaskStatus::Pending,
            kind: TaskKind::Agent,
            question: None,
        }
    }

    pub(crate) fn task(&self) -> TaskId {
        self.task
    }

    /// The screen once its task's detail was (re)loaded: the selection stays where it was,
    /// clamped to the fresh lines, and the task's own status, kind and blocked question — read
    /// fresh too — decide what `t`, `A`, `D` and `H` do next.
    pub(crate) fn refreshed(mut self, detail: &TaskDetail) -> Self {
        self.lines = detail_lines(detail);
        self.status = detail.task.status;
        self.kind = detail.task.kind;
        self.question = (detail.task.status == TaskStatus::Blocked).then(|| {
            detail
                .status
                .as_ref()
                .and_then(|entry| entry.attempt.reason.clone())
                .unwrap_or_default()
        });
        self.selected = self.selected.min(self.last());
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
            KeyCode::Esc => (self, Some(Request::Close)),
            KeyCode::Char('l') => {
                let attempt = self.selected_attempt();
                (self, Some(Request::OpenOutput(attempt)))
            }
            KeyCode::Char('t') => {
                let request = self.retryable().then_some(Request::Retry);
                (self, request)
            }
            KeyCode::Char('A') => {
                let request = self.question.clone().map(Request::OpenAnswer);
                (self, request)
            }
            KeyCode::Char('D') => (self, Some(Request::OpenDone)),
            KeyCode::Char('H') => {
                let request = (self.kind == TaskKind::Human && self.status == TaskStatus::Pending)
                    .then_some(Request::OpenAcknowledge);
                (self, request)
            }
            KeyCode::Char('j') | KeyCode::Down => (self.moved(1), None),
            KeyCode::Char('k') | KeyCode::Up => (self.moved(-1), None),
            KeyCode::Char('g') => (self.moved_to(0), None),
            KeyCode::Char('G') => {
                let last = self.last();
                (self.moved_to(last), None)
            }
            _ => (self, None),
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Failed | TaskStatus::FailedUnknown | TaskStatus::Blocked
        )
    }

    fn last(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn moved(mut self, direction: isize) -> Self {
        self.selected = self
            .selected
            .saturating_add_signed(direction)
            .min(self.last());
        self
    }

    fn moved_to(mut self, index: usize) -> Self {
        self.selected = index.min(self.last());
        self
    }

    /// The attempt whose output `l` should open: the one the selected line belongs to, or the
    /// latest attempt shown when the selection is on the task's own fields, which belong to no
    /// attempt. `None` asks the output screen for the latest attempt, followed live, exactly as
    /// it would if opened fresh; `Some` pins an earlier one.
    fn selected_attempt(&self) -> Option<u32> {
        let latest = self.lines.iter().filter_map(|line| line.attempt).max()?;
        match self.lines.get(self.selected).and_then(|line| line.attempt) {
            Some(number) if number != latest => Some(number),
            _ => None,
        }
    }

    pub(crate) fn footer_keys() -> &'static str {
        " Esc back · j/k scroll · l output · t/A/D/H act · ? keys "
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if self.help {
            key_map(&KEYS, area, buf);
            return;
        }
        if self.lines.is_empty() {
            return;
        }
        let width = usize::from(area.width);
        let wrapped: Vec<Vec<String>> = self
            .lines
            .iter()
            .map(|line| wrap(&line.text, width))
            .collect();
        let heights: Vec<usize> = wrapped.iter().map(Vec::len).collect();
        let first = first_shown(&heights, Some(self.selected), usize::from(area.height));
        let text: Vec<Line<'_>> = wrapped
            .get(first..)
            .unwrap_or_default()
            .iter()
            .flatten()
            .map(|row| Line::from(row.clone()))
            .collect();
        Paragraph::new(text).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> DetailLine {
        DetailLine {
            text: text.to_owned(),
            attempt: None,
        }
    }

    fn of_attempt(text: &str, attempt: u32) -> DetailLine {
        DetailLine {
            text: text.to_owned(),
            attempt: Some(attempt),
        }
    }

    fn screen() -> DetailScreen {
        DetailScreen {
            task: TaskId(1),
            lines: vec![
                plain("#1 pending"),
                plain("Title: a"),
                of_attempt("Attempt 2 (latest)", 2),
                of_attempt("  implementation ...", 2),
                of_attempt("Attempt 1", 1),
                of_attempt("  implementation ...", 1),
            ],
            selected: 0,
            help: false,
            status: TaskStatus::Pending,
            kind: TaskKind::Agent,
            question: None,
        }
    }

    fn press(mut screen: DetailScreen, keys: &[KeyCode]) -> DetailScreen {
        for key in keys {
            screen = screen.key(*key).0;
        }
        screen
    }

    #[test]
    fn j_and_k_move_the_selection_and_stop_at_the_ends() {
        let moved = press(screen(), &[KeyCode::Char('j'); 10]);
        assert_eq!(moved.selected, moved.last());
        let back = press(moved, &[KeyCode::Char('k'); 10]);
        assert_eq!(back.selected, 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let bottom = press(screen(), &[KeyCode::Char('G')]);
        assert_eq!(bottom.selected, bottom.last());
        let top = press(bottom, &[KeyCode::Char('g')]);
        assert_eq!(top.selected, 0);
    }

    #[test]
    fn esc_asks_to_close() {
        let (_, request) = screen().key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn l_on_a_task_field_opens_the_latest_attempts_output() {
        let (_, request) = screen().key(KeyCode::Char('l'));
        assert_eq!(request, Some(Request::OpenOutput(None)));
    }

    #[test]
    fn l_on_the_latest_attempts_own_line_still_asks_for_the_latest() {
        let on_latest = press(screen(), &[KeyCode::Char('j'); 2]);
        let (_, request) = on_latest.key(KeyCode::Char('l'));
        assert_eq!(request, Some(Request::OpenOutput(None)));
    }

    #[test]
    fn l_on_an_earlier_attempts_line_pins_that_attempt() {
        let on_earlier = press(screen(), &[KeyCode::Char('j'); 4]);
        let (_, request) = on_earlier.key(KeyCode::Char('l'));
        assert_eq!(request, Some(Request::OpenOutput(Some(1))));
    }

    #[test]
    fn question_mark_opens_the_key_map_which_swallows_other_keys_until_it_is_closed() {
        let open = press(screen(), &[KeyCode::Char('?')]);
        assert!(open.help);
        let still_open = press(open.clone(), &[KeyCode::Char('j'), KeyCode::Char('l')]);
        assert!(still_open.help);
        assert_eq!(still_open.selected, 0);
        for close in [KeyCode::Char('?'), KeyCode::Esc] {
            let (closed, request) = open.clone().key(close);
            assert!(!closed.help);
            assert_eq!(request, None);
        }
    }

    #[test]
    fn a_screen_with_no_lines_ignores_movement() {
        let empty = DetailScreen::new(TaskId(1));
        assert_eq!(
            press(empty.clone(), &[KeyCode::Char('j'), KeyCode::Char('G')]),
            empty
        );
    }

    #[test]
    fn t_on_a_failed_a_failed_unknown_or_a_blocked_task_requests_a_retry() {
        for status in [
            TaskStatus::Failed,
            TaskStatus::FailedUnknown,
            TaskStatus::Blocked,
        ] {
            let screen = DetailScreen { status, ..screen() };
            let (_, request) = screen.key(KeyCode::Char('t'));
            assert_eq!(request, Some(Request::Retry), "{status}");
        }
    }

    #[test]
    fn t_on_a_task_that_is_not_retryable_requests_nothing() {
        let screen = DetailScreen {
            status: TaskStatus::Done,
            ..screen()
        };
        assert_eq!(screen.key(KeyCode::Char('t')).1, None);
    }

    #[test]
    fn capital_a_on_a_blocked_task_opens_the_answer_form_with_its_question() {
        let screen = DetailScreen {
            status: TaskStatus::Blocked,
            question: Some("which path?".to_owned()),
            ..screen()
        };
        let (_, request) = screen.key(KeyCode::Char('A'));
        assert_eq!(request, Some(Request::OpenAnswer("which path?".to_owned())));
    }

    #[test]
    fn capital_a_on_a_task_that_is_not_blocked_requests_nothing() {
        assert_eq!(screen().key(KeyCode::Char('A')).1, None);
    }

    #[test]
    fn capital_d_on_any_task_opens_the_done_form() {
        assert_eq!(screen().key(KeyCode::Char('D')).1, Some(Request::OpenDone));
    }

    #[test]
    fn capital_h_on_a_pending_human_task_opens_the_acknowledgement_form() {
        let screen = DetailScreen {
            status: TaskStatus::Pending,
            kind: TaskKind::Human,
            ..screen()
        };
        assert_eq!(
            screen.key(KeyCode::Char('H')).1,
            Some(Request::OpenAcknowledge)
        );
    }

    #[test]
    fn capital_h_on_an_agent_task_or_one_not_pending_requests_nothing() {
        assert_eq!(screen().key(KeyCode::Char('H')).1, None);
        let running_human = DetailScreen {
            status: TaskStatus::Running,
            kind: TaskKind::Human,
            ..screen()
        };
        assert_eq!(running_human.key(KeyCode::Char('H')).1, None);
    }

    #[test]
    fn refreshed_reads_the_tasks_status_kind_and_blocked_question_from_the_detail() {
        use ktask_core::{AttemptLine, AttemptOutcome, StatusEntry, Task, Usage};
        use std::time::{Duration, SystemTime};

        let task = Task {
            id: TaskId(1),
            position: 1,
            title: "a".to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            provider: None,
            model: None,
            status: TaskStatus::Blocked,
            created_at: SystemTime::UNIX_EPOCH,
        };
        let attempt = AttemptLine {
            number: 1,
            step: "implementation".to_owned(),
            provider: Some("echo".to_owned()),
            model: None,
            session: None,
            time_spent: Duration::from_secs(1),
            outcome: AttemptOutcome::Reported(ktask_core::Outcome::NeedsInput),
            reason: Some("which path?".to_owned()),
            findings: Vec::new(),
            waiting: None,
            limit_wait: None,
            limit_warning: None,
            output_activity: None,
            steps: vec![],
            usage: Usage::default(),
            routed: None,
            more_time: None,
        };
        let detail = TaskDetail {
            task,
            provider: "echo".to_owned(),
            provider_is_own: false,
            model: String::new(),
            model_is_own: false,
            status: Some(StatusEntry {
                task: TaskId(1),
                title: "a".to_owned(),
                status: TaskStatus::Blocked,
                attempt,
                history: vec![],
                done_by_user: None,
            }),
            done_by_user: None,
        };

        let screen = DetailScreen::new(TaskId(1)).refreshed(&detail);

        assert_eq!(
            screen.clone().key(KeyCode::Char('A')).1,
            Some(Request::OpenAnswer("which path?".to_owned()))
        );
        assert_eq!(screen.key(KeyCode::Char('t')).1, Some(Request::Retry));
    }
}
