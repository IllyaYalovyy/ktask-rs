//! The queue screen: the task list itself, a removal question, a refusal shown at once
//! without asking, the results of the last run or import this screen started, and a run's
//! refusal to start. This is what shows once nothing else — the task form, the import form,
//! settings, the project picker or the registration screen — covers it.

use ktask_core::{Placement, QueueView, TaskId};
use ratatui::crossterm::event::KeyCode;

mod actions;
mod message;
mod refusal;
mod render;
mod view_queries;

pub(crate) use refusal::Refusal;
use view_queries::{cancelled, is_running, removable, reselect};

/// What a key on the queue screen asks the rest of the application to do — open another
/// screen, or leave something for the loop to carry out — when it is not something the queue
/// answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Open the task form, for a task that goes at this placement.
    OpenForm(Placement),
    /// Open the import form.
    OpenImport,
    /// Start executing the pending tasks.
    StartRun,
    /// Open the project's settings.
    OpenSettings,
    /// Open the project's provider catalogue.
    OpenProviders,
    /// Open the registered-projects picker.
    OpenProjects,
    /// Remove this task, confirmed already.
    Remove(TaskId),
    /// Retry this task: it is `failed`, `failed-unknown` or `blocked` already, so this needs
    /// no confirmation.
    Retry(TaskId),
    /// Open the answer form for this task, with the question its attempt asked.
    OpenAnswer(TaskId, String),
    /// Open the done form for this task, to mark it done by hand.
    OpenDone(TaskId),
    /// Open the acknowledgement form for this pending human task.
    OpenAcknowledge(TaskId),
    /// Open the selected task's retained provider output.
    OpenOutput(TaskId),
    /// Leave every screen.
    Quit,
}

/// The queue screen's own state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Queue {
    view: Option<QueueView>,
    selected: Option<TaskId>,
    show_cancelled: bool,
    help: bool,
    /// The task a removal is being confirmed for.
    confirming: Option<TaskId>,
    refused: Option<Refusal>,
    /// The last run's or import's own report of what it did, one per line, shown above the
    /// task list — which stays on show under it, with the selection kept — until a key that
    /// is not one of the ones that scroll it dismisses it.
    message: Option<Vec<String>>,
    /// The first of `message`'s lines shown, when there are more than fit.
    message_offset: usize,
    /// A run this screen started refused to start at all, without attempting any task,
    /// printing this. Shown beside the task list, which stays on show under it.
    run_refusal: Option<String>,
}

impl Queue {
    /// The queue on show, when it has been loaded.
    pub(crate) fn view(&self) -> Option<&QueueView> {
        self.view.as_ref()
    }

    /// Whether cancelled and skipped tasks are asked for: the queue is loaded with them, in
    /// their places.
    pub(crate) fn show_cancelled(&self) -> bool {
        self.show_cancelled
    }

    /// Whether the queue's key map is covering the task list.
    pub(crate) fn help_open(&self) -> bool {
        self.help
    }

    /// The name of the project whose queue is on show, when one is.
    pub(crate) fn project_name(&self) -> Option<&str> {
        self.view.as_ref().map(|view| view.project.name.as_str())
    }

    /// The task the selection is on, when there is one.
    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<TaskId> {
        self.selected
    }

    /// The last run's or import's own report, shown in place of the task list, when there is
    /// one.
    #[cfg(test)]
    pub(crate) fn message(&self) -> Option<&[String]> {
        self.message.as_deref()
    }

    /// The screen once the task the form held is added as `id`: a task placed next to the
    /// selected one is selected, so that it is already there when the queue is loaded again;
    /// one added at the end leaves the selection where it was.
    pub(crate) fn added(self, id: TaskId, placed_next_to_one: bool) -> Self {
        Self {
            selected: if placed_next_to_one {
                Some(id)
            } else {
                self.selected
            },
            ..self
        }
    }

    /// The screen after `queue` is (re)loaded: keeps the selection, a pending removal
    /// confirmation and a refusal only as long as they still make sense against the fresh
    /// queue.
    pub(crate) fn loaded(self, queue: QueueView) -> Self {
        let selected = reselect(self.view.as_ref(), self.selected, &queue);
        let confirming = self.confirming.filter(|id| removable(&queue, *id));
        let refused = self.refused.filter(|refusal| match refusal {
            Refusal::Running(id) => is_running(&queue, *id),
            Refusal::AlreadyCancelled(id) | Refusal::NextToCancelled(id) => cancelled(&queue, *id),
            Refusal::NotRetryable(id, status) | Refusal::NotBlocked(id, status) => queue
                .tasks
                .iter()
                .any(|task| task.id == *id && task.status == *status),
            Refusal::NotAcknowledgeable(id, kind, status) => queue
                .tasks
                .iter()
                .any(|task| task.id == *id && task.kind == *kind && task.status == *status),
        });
        Self {
            view: Some(queue),
            selected,
            confirming,
            refused,
            ..self
        }
    }

    /// The screen once switching to another project replaces the queue on show: fresh,
    /// since nothing here means anything in the other project's own queue.
    pub(crate) fn replaced(queue: QueueView) -> Self {
        Self::default().loaded(queue)
    }

    /// A key on the queue screen: quits at once, whatever else is showing over the task list —
    /// a message, a run's refusal to start, a removal question or the key map — since none of
    /// those uses `q` for anything of their own; otherwise handled by whichever of those is
    /// showing, or the plain queue itself when none is.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        if key == KeyCode::Char('q') {
            return (self, Some(Request::Quit));
        }
        if self.message.is_some() {
            return self.message_key(key);
        }
        if self.run_refusal.is_some() {
            return self.run_refusal_key(key);
        }
        if self.confirming.is_some() {
            return self.removal_confirm_key(key);
        }
        if self.help {
            return (self.help_key(key), None);
        }
        self.plain_key(key)
    }

    /// A key while removing the selected task is being confirmed: while its own key map is
    /// open, only `?` or Esc, to close it back onto the question, answer.
    fn removal_confirm_key(self, key: KeyCode) -> (Self, Option<Request>) {
        if self.help {
            return (self.help_key(key), None);
        }
        match key {
            KeyCode::Char('y') => self.confirm_removal(),
            KeyCode::Char('n') | KeyCode::Esc => (
                Self {
                    confirming: None,
                    ..self
                },
                None,
            ),
            KeyCode::Char('?') => (Self { help: true, ..self }, None),
            _ => (self, None),
        }
    }

    /// A key while the key map is open: while it is open, only `?` or Esc, to close it, answer.
    fn help_key(self, key: KeyCode) -> Self {
        match key {
            KeyCode::Esc | KeyCode::Char('?') => Self {
                help: false,
                ..self
            },
            _ => self,
        }
    }

    /// A key on the plain queue screen: no message, refusal, removal question or key map in
    /// the way.
    fn plain_key(self, key: KeyCode) -> (Self, Option<Request>) {
        // `refused` is a one-shot notice: any key past the one that raised it dismisses it,
        // whether or not that key is `d` again.
        let this = Self {
            refused: None,
            ..self
        };
        match key {
            KeyCode::Char('?') => (Self { help: true, ..this }, None),
            KeyCode::Char('a') => (
                Self {
                    show_cancelled: !this.show_cancelled,
                    ..this
                },
                None,
            ),
            KeyCode::Char('j') | KeyCode::Down => (this.select_next(), None),
            KeyCode::Char('k') | KeyCode::Up => (this.select_previous(), None),
            KeyCode::Char('n') => (this, Some(Request::OpenForm(Placement::End))),
            KeyCode::Char('o') => this.open_form_next_to(Placement::After),
            KeyCode::Char('O') => this.open_form_next_to(Placement::Before),
            KeyCode::Char('d') => (this.press_d(), None),
            KeyCode::Char('t') => this.press_t(),
            KeyCode::Char('A') => this.press_answer(),
            KeyCode::Char('H') => this.press_acknowledge(),
            KeyCode::Char('l') => this.open_output(),
            KeyCode::Char('D') => this.press_done(),
            KeyCode::Char('r') => (this, Some(Request::StartRun)),
            KeyCode::Char('i') => (this, Some(Request::OpenImport)),
            KeyCode::Char('s') => (this, Some(Request::OpenSettings)),
            KeyCode::Char('v') => (this, Some(Request::OpenProviders)),
            KeyCode::Char('p') => (this, Some(Request::OpenProjects)),
            KeyCode::Char('g') => (this.select(|_, _| 0), None),
            KeyCode::Char('G') => (this.select(|_, len| len.saturating_sub(1)), None),
            _ => (this, None),
        }
    }

    /// Selects the following visible task, stopping at the last one.
    fn select_next(self) -> Self {
        self.select(|index, _| index.saturating_add(1))
    }

    /// Selects the preceding visible task, stopping at the first one.
    fn select_previous(self) -> Self {
        self.select(|index, _| index.saturating_sub(1))
    }

    /// Requests the selected task's output when there is a selection.
    fn open_output(self) -> (Self, Option<Request>) {
        match self.selected {
            Some(id) => (self, Some(Request::OpenOutput(id))),
            None => (self, None),
        }
    }

    /// What the frame's bottom border says while the queue screen is showing, whichever of its
    /// own sub-states is active: the same regardless, since every one of them is answered by a
    /// key already named in its own question line or key map.
    pub(crate) fn footer_keys() -> &'static str {
        " q quit · ? keys "
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{
        AnswerError, AttemptLine, AttemptOutcome, Outcome, Project, RetryError, StatusSummary,
        Task, TaskKind, TaskStatus,
    };

    use super::*;

    fn task(id: u64) -> Task {
        Task {
            id: TaskId(id),
            position: 0,
            title: format!("task {id}"),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind: TaskKind::Agent,
            links: vec![],
            status: TaskStatus::Pending,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn queue_of(ids: &[u64]) -> QueueView {
        QueueView {
            project: Project {
                name: "app".to_owned(),
                path: PathBuf::from("/work/app"),
                registered_at: SystemTime::UNIX_EPOCH,
            },
            summary: StatusSummary::default(),
            tasks: ids.iter().map(|id| task(*id)).collect(),
            attempts: HashMap::new(),
            history: HashMap::new(),
            done_by_user: HashMap::new(),
        }
    }

    fn loaded(ids: &[u64]) -> Queue {
        Queue::default().loaded(queue_of(ids))
    }

    fn press(queue: Queue, keys: &[KeyCode]) -> Queue {
        keys.iter().fold(queue, |queue, key| queue.key(*key).0)
    }

    fn on(queue: &Queue) -> Option<u64> {
        queue.selected.map(|id| id.0)
    }

    #[test]
    fn a_loaded_queue_is_shown_with_its_first_task_selected() {
        let queue = loaded(&[3, 5, 8]);
        assert_eq!(queue.view, Some(queue_of(&[3, 5, 8])));
        assert_eq!(on(&queue), Some(3));
        assert!(!queue.help && !queue.show_cancelled);
    }

    #[test]
    fn an_empty_queue_selects_nothing_and_keys_move_nothing() {
        let queue = loaded(&[]);
        assert_eq!(on(&queue), None);
        let queue = press(queue, &[KeyCode::Char('j'), KeyCode::Char('G')]);
        assert_eq!(on(&queue), None);
    }

    #[test]
    fn q_asks_to_quit() {
        let (queue, request) = loaded(&[1]).key(KeyCode::Char('q'));
        assert_eq!(request, Some(Request::Quit));
        assert_eq!(queue.view, Some(queue_of(&[1])));
    }

    #[test]
    fn j_and_down_select_the_next_task_and_stop_at_the_last() {
        for key in [KeyCode::Char('j'), KeyCode::Down] {
            let queue = loaded(&[1, 2, 3]);
            let queue = press(queue, &[key]);
            assert_eq!(on(&queue), Some(2));
            let queue = press(queue, &[key, key, key]);
            assert_eq!(on(&queue), Some(3));
        }
    }

    #[test]
    fn k_and_up_select_the_previous_task_and_stop_at_the_first() {
        for key in [KeyCode::Char('k'), KeyCode::Up] {
            let queue = press(loaded(&[1, 2, 3]), &[KeyCode::Char('G'), key]);
            assert_eq!(on(&queue), Some(2));
            let queue = press(queue, &[key, key, key]);
            assert_eq!(on(&queue), Some(1));
        }
    }

    #[test]
    fn g_selects_the_first_task_and_capital_g_the_last() {
        let queue = press(loaded(&[1, 2, 3]), &[KeyCode::Char('G')]);
        assert_eq!(on(&queue), Some(3));
        let queue = press(queue, &[KeyCode::Char('g')]);
        assert_eq!(on(&queue), Some(1));
    }

    #[test]
    fn a_reload_keeps_the_selection_on_the_same_task_when_others_come_before_it() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('j')]);
        let queue = queue.loaded(queue_of(&[7, 1, 2, 9]));
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn a_reload_without_the_selected_task_selects_the_one_in_its_place_or_the_last() {
        let queue = press(loaded(&[1, 2, 3]), &[KeyCode::Char('j')]);
        let moved = queue.clone().loaded(queue_of(&[1, 3]));
        assert_eq!(on(&moved), Some(3));
        let queue = press(queue, &[KeyCode::Char('G')]);
        let shorter = queue.loaded(queue_of(&[1, 2]));
        assert_eq!(on(&shorter), Some(2));
        let emptied = shorter.loaded(queue_of(&[]));
        assert_eq!(on(&emptied), None);
        let refilled = emptied.loaded(queue_of(&[4, 5]));
        assert_eq!(on(&refilled), Some(4));
    }

    #[test]
    fn a_toggles_asking_for_cancelled_tasks() {
        let queue = press(loaded(&[1]), &[KeyCode::Char('a')]);
        assert!(queue.show_cancelled);
        let queue = press(queue, &[KeyCode::Char('a')]);
        assert!(!queue.show_cancelled);
    }

    #[test]
    fn question_mark_opens_the_key_map_and_esc_or_question_mark_closes_it() {
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let queue = press(loaded(&[1]), &[KeyCode::Char('?')]);
            assert!(queue.help);
            assert!(!press(queue, &[close]).help);
        }
    }

    #[test]
    fn while_the_key_map_is_open_only_it_and_quit_answer_keys() {
        let open = press(loaded(&[1, 2]), &[KeyCode::Char('?')]);
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('G'),
            KeyCode::Down,
            KeyCode::Char('a'),
            KeyCode::Char('x'),
        ] {
            assert_eq!(press(open.clone(), &[key]), open);
        }
        assert_eq!(open.clone().key(KeyCode::Char('q')).1, Some(Request::Quit));
    }

    #[test]
    fn d_asks_about_removing_the_selected_task_and_changes_nothing_else() {
        let queue = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('j'), KeyCode::Char('d')],
        );
        assert_eq!(queue.confirming, Some(TaskId(2)));
        assert_eq!(on(&queue), Some(2));
        assert_eq!(queue.view, Some(queue_of(&[1, 2, 3])));
    }

    #[test]
    fn d_with_nothing_selected_asks_nothing() {
        assert_eq!(press(loaded(&[]), &[KeyCode::Char('d')]).confirming, None);
    }

    #[test]
    fn d_on_a_running_task_refuses_without_asking_and_changes_nothing_else() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Running;
        let queue = Queue::default().loaded(view.clone());

        let queue = press(queue, &[KeyCode::Char('d')]);

        assert_eq!(queue.confirming, None);
        assert_eq!(queue.refused, Some(Refusal::Running(TaskId(1))));
        assert_eq!(queue.view, Some(view));
    }

    #[test]
    fn d_on_a_cancelled_task_refuses_without_asking_and_changes_nothing_else() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Cancelled;
        let queue = Queue::default().loaded(view.clone());

        let queue = press(queue, &[KeyCode::Char('d')]);

        assert_eq!(queue.confirming, None);
        assert_eq!(queue.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));
        assert_eq!(
            Refusal::AlreadyCancelled(TaskId(1)).message(),
            "task 1 is already cancelled"
        );
        assert_eq!(queue.view, Some(view));
    }

    #[test]
    fn t_on_a_failed_a_failed_unknown_or_a_blocked_task_requests_a_retry_at_once() {
        for status in [
            TaskStatus::Failed,
            TaskStatus::FailedUnknown,
            TaskStatus::Blocked,
        ] {
            let mut view = queue_of(&[1, 2]);
            view.tasks[0].status = status;
            let queue = Queue::default().loaded(view.clone());

            let (queue, request) = queue.key(KeyCode::Char('t'));

            assert_eq!(request, Some(Request::Retry(TaskId(1))), "{status}");
            assert_eq!(queue.refused, None, "{status}");
            assert_eq!(queue.view, Some(view), "{status}");
        }
    }

    #[test]
    fn t_on_a_pending_a_running_or_a_done_task_refuses_without_asking_naming_its_status() {
        for status in [TaskStatus::Pending, TaskStatus::Running, TaskStatus::Done] {
            let mut view = queue_of(&[1, 2]);
            view.tasks[0].status = status;
            let queue = Queue::default().loaded(view.clone());

            let (queue, request) = queue.key(KeyCode::Char('t'));

            assert_eq!(request, None, "{status}");
            assert_eq!(
                queue.refused,
                Some(Refusal::NotRetryable(TaskId(1), status)),
                "{status}"
            );
            assert_eq!(queue.view, Some(view), "{status}");
        }
    }

    #[test]
    fn t_on_a_cancelled_task_refuses_too_in_the_same_words_as_ktask_rs_retry() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Cancelled;
        let queue = Queue::default().loaded(view);

        let (queue, request) = queue.key(KeyCode::Char('t'));

        assert_eq!(request, None);
        assert_eq!(
            queue.refused,
            Some(Refusal::NotRetryable(TaskId(1), TaskStatus::Cancelled))
        );
        assert_eq!(
            Refusal::NotRetryable(TaskId(1), TaskStatus::Cancelled).message(),
            RetryError::NotRetryable {
                id: TaskId(1),
                status: TaskStatus::Cancelled
            }
            .to_string()
        );
    }

    #[test]
    fn t_with_nothing_selected_requests_and_refuses_nothing() {
        let (queue, request) = loaded(&[]).key(KeyCode::Char('t'));
        assert_eq!(request, None);
        assert_eq!(queue.refused, None);
    }

    /// An [`AttemptLine`] blocked with `reason` as the question its attempt asked.
    fn blocked_attempt(reason: &str) -> AttemptLine {
        AttemptLine {
            number: 1,
            step: "implementation".to_owned(),
            provider: Some("echo".to_owned()),
            model: None,
            session: None,
            time_spent: std::time::Duration::from_secs(1),
            outcome: AttemptOutcome::Reported(Outcome::NeedsInput),
            reason: Some(reason.to_owned()),
            waiting_for: None,
            limit_wait: None,
            output_activity: None,
            usage: ktask_core::Usage::default(),
            steps: vec![],
        }
    }

    #[test]
    fn capital_a_on_a_blocked_task_opens_the_answer_form_with_its_question() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Blocked;
        view.attempts
            .insert(TaskId(1), blocked_attempt("which path?"));
        let queue = Queue::default().loaded(view);

        let (queue, request) = queue.key(KeyCode::Char('A'));

        assert_eq!(
            request,
            Some(Request::OpenAnswer(TaskId(1), "which path?".to_owned()))
        );
        assert_eq!(queue.refused, None);
    }

    #[test]
    fn capital_a_on_a_task_that_is_not_blocked_refuses_naming_its_status() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::FailedUnknown,
        ] {
            let mut view = queue_of(&[1, 2]);
            view.tasks[0].status = status;
            let queue = Queue::default().loaded(view.clone());

            let (queue, request) = queue.key(KeyCode::Char('A'));

            assert_eq!(request, None, "{status}");
            assert_eq!(
                queue.refused,
                Some(Refusal::NotBlocked(TaskId(1), status)),
                "{status}"
            );
            assert_eq!(queue.view, Some(view), "{status}");
        }
    }

    #[test]
    fn capital_a_with_nothing_selected_requests_and_refuses_nothing() {
        let (queue, request) = loaded(&[]).key(KeyCode::Char('A'));
        assert_eq!(request, None);
        assert_eq!(queue.refused, None);
    }

    #[test]
    fn the_not_blocked_refusal_is_dismissed_by_the_next_key_that_is_not_capital_a_again() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('A')]);
        assert_eq!(
            queue.refused,
            Some(Refusal::NotBlocked(TaskId(1), TaskStatus::Pending))
        );
        assert_eq!(press(queue, &[KeyCode::Char('j')]).refused, None);
    }

    #[test]
    fn the_not_blocked_refusal_is_worded_as_ktask_rs_answer_would() {
        assert_eq!(
            Refusal::NotBlocked(TaskId(1), TaskStatus::Pending).message(),
            AnswerError::NotBlocked {
                id: TaskId(1),
                status: TaskStatus::Pending
            }
            .to_string()
        );
    }

    #[test]
    fn capital_d_on_any_selected_task_opens_the_done_form() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::FailedUnknown,
            TaskStatus::Blocked,
            TaskStatus::Cancelled,
            TaskStatus::Skipped,
            TaskStatus::Superseded,
        ] {
            let mut view = queue_of(&[1, 2]);
            view.tasks[0].status = status;
            let queue = Queue::default().loaded(view.clone());

            let (queue, request) = queue.key(KeyCode::Char('D'));

            assert_eq!(request, Some(Request::OpenDone(TaskId(1))), "{status}");
            assert_eq!(queue.refused, None, "{status}");
            assert_eq!(queue.view, Some(view), "{status}");
        }
    }

    #[test]
    fn capital_d_with_nothing_selected_requests_and_refuses_nothing() {
        let (queue, request) = loaded(&[]).key(KeyCode::Char('D'));
        assert_eq!(request, None);
        assert_eq!(queue.refused, None);
    }

    #[test]
    fn the_not_retryable_refusal_is_dismissed_by_the_next_key_that_is_not_t_again() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('t')]);
        assert_eq!(
            queue.refused,
            Some(Refusal::NotRetryable(TaskId(1), TaskStatus::Pending))
        );
        assert_eq!(press(queue, &[KeyCode::Char('j')]).refused, None);
    }

    #[test]
    fn the_refusal_is_dismissed_by_the_next_key_that_is_not_d_again() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Running;
        let queue = Queue::default().loaded(view);
        let refused = press(queue, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::Running(TaskId(1))));

        for key in [KeyCode::Char('j'), KeyCode::Char('x')] {
            assert_eq!(press(refused.clone(), &[key]).refused, None);
        }
        // The task is still running, so d again just shows the same refusal afresh.
        assert_eq!(
            press(refused, &[KeyCode::Char('d')]).refused,
            Some(Refusal::Running(TaskId(1)))
        );
    }

    #[test]
    fn the_running_refusal_goes_when_its_task_stops_running_from_elsewhere() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Running;
        let queue = Queue::default().loaded(view);
        let refused = press(queue, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::Running(TaskId(1))));

        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Done;
        let reloaded = refused.loaded(view);
        assert_eq!(reloaded.refused, None);
    }

    #[test]
    fn the_cancelled_refusal_survives_a_reload_and_goes_when_the_task_is_gone() {
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Cancelled;
        let queue = Queue::default().loaded(view.clone());
        let refused = press(queue, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));

        let reloaded = refused.loaded(view);
        assert_eq!(reloaded.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));

        let gone = reloaded.loaded(queue_of(&[2]));
        assert_eq!(gone.refused, None);
    }

    #[test]
    fn r_asks_the_loop_to_start_a_run_and_changes_nothing_else() {
        let (queue, request) =
            press(loaded(&[1, 2]), &[KeyCode::Char('j')]).key(KeyCode::Char('r'));
        assert_eq!(request, Some(Request::StartRun));
        assert_eq!(on(&queue), Some(2));
        assert_eq!(queue.view, Some(queue_of(&[1, 2])));
    }

    #[test]
    fn n_asks_to_open_a_form_at_the_end() {
        let (_, request) = loaded(&[1, 2]).key(KeyCode::Char('n'));
        assert_eq!(request, Some(Request::OpenForm(Placement::End)));
    }

    #[test]
    fn i_asks_to_open_the_import_form() {
        let (_, request) = loaded(&[1, 2]).key(KeyCode::Char('i'));
        assert_eq!(request, Some(Request::OpenImport));
    }

    #[test]
    fn s_asks_the_loop_to_open_settings_and_changes_nothing_else() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('j')]);
        let (queue, request) = queue.key(KeyCode::Char('s'));
        assert_eq!(request, Some(Request::OpenSettings));
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn p_asks_the_loop_to_open_the_project_picker_and_changes_nothing_else() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('j')]);
        let (queue, request) = queue.key(KeyCode::Char('p'));
        assert_eq!(request, Some(Request::OpenProjects));
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn an_import_message_is_shown_until_a_key_that_does_not_scroll_it_dismisses_it() {
        let queue = loaded(&[1]).shown_message("1\n2\n");
        assert_eq!(
            queue.message.as_deref(),
            Some(["1".to_owned(), "2".to_owned()].as_slice())
        );
        // The message shows above the task list, not instead of it: the list and the
        // selection on it are still there underneath.
        assert_eq!(queue.view, Some(queue_of(&[1])));
        assert_eq!(on(&queue), Some(1));
        assert_eq!(press(queue.clone(), &[KeyCode::Char('x')]).message, None);
    }

    #[test]
    fn an_import_that_added_tasks_selects_the_first_one_and_shows_its_own_report() {
        let queue = loaded(&[1, 2, 3]).imported("1 task added: 3", Some(TaskId(3)));
        assert_eq!(
            queue.message.as_deref(),
            Some(["1 task added: 3".to_owned()].as_slice())
        );
        assert_eq!(on(&queue), Some(3));
    }

    #[test]
    fn an_import_that_added_nothing_leaves_the_selection_where_it_was() {
        let queue = press(loaded(&[1, 2]), &[KeyCode::Char('j')]);
        let queue = queue.imported("1 cancelled task was skipped", None);
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn a_run_message_is_shown_until_a_key_that_does_not_scroll_it_dismisses_it() {
        let queue = loaded(&[1]).run_message("task 1: done".to_owned());
        assert_eq!(
            queue.message.as_deref(),
            Some(["task 1: done".to_owned()].as_slice())
        );
        assert_eq!(queue.view, Some(queue_of(&[1])));
        assert_eq!(on(&queue), Some(1));

        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            assert_eq!(
                press(queue.clone(), &[key]).message,
                queue.message,
                "{key:?} should scroll, not dismiss"
            );
        }

        assert_eq!(press(queue.clone(), &[KeyCode::Char('x')]).message, None);
    }

    #[test]
    fn j_and_k_scroll_a_run_message_that_does_not_fit_and_g_and_shift_g_jump_to_its_ends() {
        let text = (1..=5)
            .map(|n| format!("task {n}: done"))
            .collect::<Vec<_>>()
            .join("\n");
        let queue = loaded(&[1]).run_message(text);
        assert_eq!(queue.message_offset, 0);

        let scrolled = press(queue.clone(), &[KeyCode::Char('j'), KeyCode::Char('j')]);
        assert_eq!(scrolled.message_offset, 2);
        assert_eq!(scrolled.message, queue.message);

        let back = press(scrolled.clone(), &[KeyCode::Char('k')]);
        assert_eq!(back.message_offset, 1);

        let bottom = press(queue.clone(), &[KeyCode::Char('G')]);
        assert_eq!(bottom.message_offset, 4);

        let held = press(queue.clone(), &[KeyCode::Char('k')]);
        assert_eq!(held.message_offset, 0);
        let past_bottom = press(bottom, &[KeyCode::Char('j')]);
        assert_eq!(past_bottom.message_offset, 4);

        let top = press(scrolled, &[KeyCode::Char('g')]);
        assert_eq!(top.message_offset, 0);
    }

    #[test]
    fn r_again_both_dismisses_a_shown_message_and_requests_a_fresh_run() {
        let queue = loaded(&[1]).run_message("task 1: done".to_owned());
        let (queue, request) = queue.key(KeyCode::Char('r'));
        assert_eq!(queue.message, None);
        assert_eq!(request, Some(Request::StartRun));
    }

    #[test]
    fn a_run_that_refuses_to_start_shows_beside_the_task_list_with_the_selection_kept() {
        let queue = press(loaded(&[1, 2, 3]), &[KeyCode::Char('j')]);
        assert_eq!(on(&queue), Some(2));

        let queue = queue.run_message("nothing is pending".to_owned());

        assert_eq!(queue.run_refusal.as_deref(), Some("nothing is pending"));
        assert_eq!(queue.message, None);
        assert_eq!(queue.view, Some(queue_of(&[1, 2, 3])));
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn every_run_refusal_to_start_is_recognised_the_same_way() {
        for text in [
            "nothing is pending",
            "task 3: failed: it broke; run did not start",
            "task 3: blocked; run did not start",
            "ktask-rs: a run is already in progress: process 4321",
            "ktask-rs: a run is already in progress",
        ] {
            let queue = loaded(&[1]).run_message(text.to_owned());
            assert_eq!(queue.run_refusal.as_deref(), Some(text), "{text}");
            assert_eq!(queue.message, None, "{text}");
        }
    }

    #[test]
    fn j_k_g_and_shift_g_move_the_selection_while_a_run_refusal_is_shown() {
        let queue = loaded(&[1, 2, 3]).run_message("nothing is pending".to_owned());
        assert_eq!(on(&queue), Some(1));

        let queue = press(queue, &[KeyCode::Char('j')]);
        assert_eq!(queue.run_refusal.as_deref(), Some("nothing is pending"));
        assert_eq!(on(&queue), Some(2));

        let queue = press(queue, &[KeyCode::Char('G')]);
        assert_eq!(queue.run_refusal.as_deref(), Some("nothing is pending"));
        assert_eq!(on(&queue), Some(3));

        let queue = press(queue, &[KeyCode::Char('k')]);
        assert_eq!(queue.run_refusal.as_deref(), Some("nothing is pending"));
        assert_eq!(on(&queue), Some(2));

        let queue = press(queue, &[KeyCode::Char('g')]);
        assert_eq!(queue.run_refusal.as_deref(), Some("nothing is pending"));
        assert_eq!(on(&queue), Some(1));
    }

    #[test]
    fn a_key_that_is_not_jkgg_dismisses_a_run_refusal_then_acts_as_it_would_otherwise() {
        let queue = loaded(&[1, 2]).run_message("nothing is pending".to_owned());

        let queue = press(queue, &[KeyCode::Char('a')]);
        assert_eq!(queue.run_refusal, None);
        assert!(queue.show_cancelled);
    }

    #[test]
    fn r_again_both_dismisses_a_shown_run_refusal_and_requests_a_fresh_run() {
        let queue = loaded(&[1]).run_message("nothing is pending".to_owned());
        let (queue, request) = queue.key(KeyCode::Char('r'));
        assert_eq!(queue.run_refusal, None);
        assert_eq!(request, Some(Request::StartRun));
    }

    #[test]
    fn y_confirms_the_removal_and_moves_the_selection_to_the_next_task() {
        let queue = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('j'), KeyCode::Char('d')],
        );
        let (queue, request) = queue.key(KeyCode::Char('y'));
        assert_eq!(queue.confirming, None);
        assert_eq!(request, Some(Request::Remove(TaskId(2))));
        assert_eq!(on(&queue), Some(3));
    }

    #[test]
    fn confirming_the_removal_of_the_last_task_moves_the_selection_to_the_one_before() {
        let queue = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('G'), KeyCode::Char('d')],
        );
        let (queue, request) = queue.key(KeyCode::Char('y'));
        assert_eq!(request, Some(Request::Remove(TaskId(3))));
        assert_eq!(on(&queue), Some(2));
    }

    #[test]
    fn n_and_esc_drop_the_question_and_change_nothing_else() {
        let before = loaded(&[1, 2]);
        for answer in [KeyCode::Char('n'), KeyCode::Esc] {
            let queue = press(before.clone(), &[KeyCode::Char('d'), answer]);
            assert_eq!(queue, before);
        }
    }

    #[test]
    fn while_a_removal_is_asked_about_only_its_answers_quit_and_the_key_map_are_heard() {
        let asked = press(loaded(&[1, 2]), &[KeyCode::Char('d')]);
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('G'),
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('x'),
        ] {
            assert_eq!(press(asked.clone(), &[key]), asked);
        }
        assert_eq!(asked.clone().key(KeyCode::Char('q')).1, Some(Request::Quit));

        let mapped = press(asked.clone(), &[KeyCode::Char('?')]);
        assert!(mapped.help);
        assert_eq!(
            mapped.confirming, asked.confirming,
            "the key map does not answer the question itself"
        );
        for key in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('j'),
            KeyCode::Char('x'),
        ] {
            assert_eq!(press(mapped.clone(), &[key]), mapped);
        }
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let closed = press(mapped.clone(), &[close]);
            assert!(!closed.help);
            assert_eq!(closed.confirming, asked.confirming);
        }
    }

    #[test]
    fn the_question_goes_when_its_task_is_gone_or_cancelled_from_elsewhere() {
        let asked = press(loaded(&[1, 2]), &[KeyCode::Char('d')]);
        let gone = asked.clone().loaded(queue_of(&[2]));
        assert_eq!(gone.confirming, None);
        let mut view = queue_of(&[1, 2]);
        view.tasks[0].status = TaskStatus::Cancelled;
        let cancelled = asked.clone().loaded(view);
        assert_eq!(cancelled.confirming, None);
        let same = asked.loaded(queue_of(&[1, 2, 3]));
        assert_eq!(same.confirming, Some(TaskId(1)));
    }

    #[test]
    fn o_and_capital_o_open_a_form_for_a_task_below_or_above_the_selected_one() {
        let queue = press(loaded(&[1, 2, 3]), &[KeyCode::Char('j')]);
        let (_, below) = queue.clone().key(KeyCode::Char('o'));
        assert_eq!(below, Some(Request::OpenForm(Placement::After(TaskId(2)))));
        let (above_queue, above) = queue.key(KeyCode::Char('O'));
        assert_eq!(above, Some(Request::OpenForm(Placement::Before(TaskId(2)))));
        assert_eq!(on(&above_queue), Some(2));
    }

    #[test]
    fn o_and_capital_o_on_an_empty_queue_open_a_form_for_a_task_at_the_end() {
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let (_, request) = loaded(&[]).key(key);
            assert_eq!(request, Some(Request::OpenForm(Placement::End)));
        }
    }

    #[test]
    fn o_and_capital_o_next_to_a_cancelled_task_refuse_at_once_without_opening_the_form() {
        let mut view = queue_of(&[1, 2, 3]);
        view.tasks[1].status = TaskStatus::Cancelled;
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let queue = Queue::default().loaded(view.clone());
            let queue = press(queue, &[KeyCode::Char('j')]);
            let (queue, request) = queue.key(key);
            assert_eq!(request, None);
            assert_eq!(queue.refused, Some(Refusal::NextToCancelled(TaskId(2))));
            assert_eq!(
                Refusal::NextToCancelled(TaskId(2)).message(),
                "task 2 is cancelled"
            );
        }
    }

    #[test]
    fn a_task_added_next_to_the_selected_one_is_selected_and_one_added_at_the_end_is_not() {
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let queue = press(loaded(&[1, 2]), &[key]);
            let queue = queue.added(TaskId(3), true);
            assert_eq!(on(&queue), Some(3));
            let reloaded = queue.loaded(queue_of(&[1, 3, 2]));
            assert_eq!(on(&reloaded), Some(3));
        }
        let queue = loaded(&[1, 2]).added(TaskId(3), false);
        assert_eq!(on(&queue), Some(1));
    }

    #[test]
    fn ctrl_keys_do_nothing_on_the_queue() {
        // The queue screen answers no key event but `Key`; a Ctrl-letter reaches it only
        // through the router, which does not forward one to the queue at all.
        let queue = loaded(&[1, 2]);
        assert_eq!(queue.clone(), queue);
    }
}
