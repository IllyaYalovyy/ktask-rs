//! The queue screen: the task list itself, a removal question, a refusal shown at once
//! without asking, the results of the last run or import this screen started, and a run's
//! refusal to start. This is what shows once nothing else — the task form, the import form,
//! settings, the project picker or the registration screen — covers it.

use ktask_core::{
    AppendError, AttemptLine, CancelError, Placement, QueueView, StepLine, Task, TaskId,
    TaskStatus, displayed_status,
};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::scroll::first_shown;
use crate::widgets::{elide, key_map};

/// Every key the queue screen itself answers, and what it does. Keys that only work while a
/// question, a form or another screen is up are that context's own — shown there, in its own
/// question line or footer — and left out of this key map, so no key map here shows a key
/// that does not work in the context it is shown in, and no key is listed twice.
const KEYS: [(&str, &str); 16] = [
    ("j, Down", "select the next task"),
    ("k, Up", "select the previous task"),
    ("g", "select the first task"),
    ("G", "select the last task"),
    ("a", "show or hide cancelled tasks"),
    ("n", "add a task at the end, written in a form"),
    ("o", "add a task below the selected one, written in a form"),
    ("O", "add a task above the selected one, written in a form"),
    ("d", "remove the selected task, after asking"),
    (
        "r",
        "start executing the queue, exactly as `ktask-rs run` does",
    ),
    (
        "i",
        "import the tasks of a JSON file, asked for by its path",
    ),
    ("s", "open the project's settings"),
    ("p", "work on another registered project's queue"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map"),
    ("q", "quit"),
];

/// The keys shown by `?` while the removal question is asking — the question itself already
/// shows them inline, but a long enough title still leaves `?` as the one way to see them
/// without cutting anything.
const REMOVAL_QUESTION_KEYS: [(&str, &str); 2] = [("y", "remove the task"), ("n, Esc", "keep it")];

/// Why a key was refused at once, without asking or opening anything: the same reason, in the
/// same words, that `ktask-rs remove` or `ktask-rs add` gives for the same situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `d` on the selected task, which is running: `ktask-rs remove` would refuse it too.
    Running(TaskId),
    /// `d` on the selected task, which is cancelled already: `ktask-rs remove` would refuse
    /// it too.
    AlreadyCancelled(TaskId),
    /// `o` or `O` next to the selected task, which is cancelled: `ktask-rs add` would refuse
    /// a task placed next to it too.
    NextToCancelled(TaskId),
}

impl Refusal {
    /// The message shown for this refusal, word for word what the CLI command it mirrors
    /// would print.
    pub(crate) fn message(self) -> String {
        match self {
            Self::Running(id) => CancelError::Running(id).to_string(),
            Self::AlreadyCancelled(id) => CancelError::AlreadyCancelled(id).to_string(),
            Self::NextToCancelled(id) => AppendError::CancelledTask(id).to_string(),
        }
    }
}

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
    /// Open the registered-projects picker.
    OpenProjects,
    /// Remove this task, confirmed already.
    Remove(TaskId),
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
    /// The last run's or import's own report of what it did, one per line, shown in place of
    /// the task list until a key that is not one of the ones that scroll it dismisses it.
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

    /// Whether cancelled tasks are asked for: the queue is loaded with them, in their places.
    pub(crate) fn show_cancelled(&self) -> bool {
        self.show_cancelled
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

    /// The screen after `queue` is (re)loaded: keeps the selection, a pending removal
    /// confirmation and a refusal only as long as they still make sense against the fresh
    /// queue.
    pub(crate) fn loaded(self, queue: QueueView) -> Self {
        let selected = reselect(self.view.as_ref(), self.selected, &queue);
        let confirming = self.confirming.filter(|id| removable(&queue, *id));
        let refused = self.refused.filter(|refusal| match refusal {
            Refusal::Running(id) => is_running(&queue, *id),
            Refusal::AlreadyCancelled(id) | Refusal::NextToCancelled(id) => cancelled(&queue, *id),
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

    /// A key while the last run's or import's results are shown in place of the task list:
    /// `j`/`Down` and `k`/`Up` scroll one line, `g`/`G` jump to the first or last line, and any
    /// other key dismisses the results, then is handled as it would be on the plain queue
    /// screen — so, for example, `r` both dismisses a shown message and starts a fresh run.
    fn message_key(self, key: KeyCode) -> (Self, Option<Request>) {
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
    fn run_refusal_key(self, key: KeyCode) -> (Self, Option<Request>) {
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

    /// The screen once the removal it asks about is confirmed, and the request for the loop to
    /// carry it out: the selection moves to the task after the one removed, or the one before
    /// it when it was the last, so that it is already there when the queue is loaded again.
    fn confirm_removal(self) -> (Self, Option<Request>) {
        let Some(id) = self.confirming else {
            return (self, None);
        };
        let neighbour = self.view.as_ref().and_then(|view| {
            let index = view.tasks.iter().position(|task| task.id == id)?;
            let next = view.tasks.get(index + 1);
            next.or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|before| view.tasks.get(before))
            })
            .map(|task| task.id)
        });
        (
            Self {
                confirming: None,
                selected: neighbour.or(self.selected),
                ..self
            },
            Some(Request::Remove(id)),
        )
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
            KeyCode::Char('j') | KeyCode::Down => {
                (this.select(|index, _| index.saturating_add(1)), None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                (this.select(|index, _| index.saturating_sub(1)), None)
            }
            KeyCode::Char('n') => (this, Some(Request::OpenForm(Placement::End))),
            KeyCode::Char('o') => this.open_form_next_to(Placement::After),
            KeyCode::Char('O') => this.open_form_next_to(Placement::Before),
            KeyCode::Char('d') => (this.press_d(), None),
            KeyCode::Char('r') => (this, Some(Request::StartRun)),
            KeyCode::Char('i') => (this, Some(Request::OpenImport)),
            KeyCode::Char('s') => (this, Some(Request::OpenSettings)),
            KeyCode::Char('p') => (this, Some(Request::OpenProjects)),
            KeyCode::Char('g') => (this.select(|_, _| 0), None),
            KeyCode::Char('G') => (this.select(|_, len| len.saturating_sub(1)), None),
            _ => (this, None),
        }
    }

    /// The screen with an empty form asked for, for a task that goes next to the selected one
    /// the way `beside` says; at the end when nothing is selected. Refuses at once, without
    /// asking for the form, when the selected task is cancelled.
    fn open_form_next_to(self, beside: fn(TaskId) -> Placement) -> (Self, Option<Request>) {
        let Some(id) = self.selected else {
            return (self, Some(Request::OpenForm(Placement::End)));
        };
        if self.view.as_ref().is_some_and(|view| cancelled(view, id)) {
            return (
                Self {
                    refused: Some(Refusal::NextToCancelled(id)),
                    ..self
                },
                None,
            );
        }
        (self, Some(Request::OpenForm(beside(id))))
    }

    /// The screen after `d` on the selected task: it asks to confirm removing it when it can
    /// be removed, and otherwise refuses at once, naming why — running, or cancelled already —
    /// without asking; with nothing selected, changes nothing.
    fn press_d(self) -> Self {
        let Some(id) = self.selected else {
            return self;
        };
        let Some(view) = &self.view else {
            return self;
        };
        if removable(view, id) {
            Self {
                confirming: Some(id),
                ..self
            }
        } else if is_running(view, id) {
            Self {
                refused: Some(Refusal::Running(id)),
                ..self
            }
        } else if cancelled(view, id) {
            Self {
                refused: Some(Refusal::AlreadyCancelled(id)),
                ..self
            }
        } else {
            self
        }
    }

    /// The screen with the selection moved to the index `target` picks, given the index it is
    /// at and how many tasks there are. It stays inside the list.
    fn select(self, target: impl FnOnce(usize, usize) -> usize) -> Self {
        let Some(view) = &self.view else {
            return self;
        };
        let Some(last) = view.tasks.len().checked_sub(1) else {
            return self;
        };
        let index = view
            .tasks
            .iter()
            .position(|task| Some(task.id) == self.selected)
            .unwrap_or(0);
        let selected = view
            .tasks
            .get(target(index, view.tasks.len()).min(last))
            .map(|task| task.id);
        Self { selected, ..self }
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

    /// What the frame's bottom border says while the queue screen is showing, whichever of its
    /// own sub-states is active: the same regardless, since every one of them is answered by a
    /// key already named in its own question line or key map.
    pub(crate) fn footer_keys() -> &'static str {
        " q quit · ? keys "
    }

    /// Draws the queue screen over the whole of `area`.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let Some(view) = &self.view else {
            Paragraph::new("Loading the queue…").render(area, buf);
            return;
        };
        if self.help {
            key_map(self.help_keys(), area, buf);
            return;
        }
        let [header, list] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        Paragraph::new(self.header_lines(view, usize::from(header.width))).render(header, buf);
        let height = usize::from(list.height);
        let width = usize::from(list.width);
        match &self.message {
            Some(message) => {
                Paragraph::new(message_lines(self.message_offset, message, height))
                    .render(list, buf);
            }
            None => {
                Paragraph::new(task_lines(self.selected, view, height, width)).render(list, buf);
            }
        }
    }

    /// The keys `?` shows right now: the removal question's own keys while it is asking, the
    /// queue's full key map otherwise.
    fn help_keys(&self) -> &'static [(&'static str, &'static str)] {
        if self.confirming.is_some() {
            &REMOVAL_QUESTION_KEYS
        } else {
            &KEYS
        }
    }

    /// The header: the project, the counts, and the question line.
    fn header_lines(&self, view: &QueueView, width: usize) -> Vec<Line<'static>> {
        let summary = view.summary;
        vec![
            Line::styled(
                view.project.name.clone(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::from(format!(
                "pending {}  running {}  done {}  failed {}  blocked {}  unknown {}  cancelled {}",
                summary.pending,
                summary.running,
                summary.done,
                summary.failed,
                summary.blocked,
                summary.failed_unknown,
                summary.cancelled
            )),
            self.question_line(view, width),
        ]
    }

    /// The one-line question or notice shown under the summary — a removal confirmation, a
    /// refusal, or a run's refusal to start, whichever applies — or an empty line when none
    /// does. The title is cut with `…` to fit `width` when it is long, so its keys are never
    /// pushed off screen.
    fn question_line(&self, view: &QueueView, width: usize) -> Line<'static> {
        if let Some(refusal) = &self.run_refusal {
            return Line::styled(refusal.clone(), Style::new().add_modifier(Modifier::BOLD));
        }
        let question = self
            .confirming
            .and_then(|id| view.tasks.iter().find(|task| task.id == id))
            .map_or_else(Line::default, |task| {
                let prefix = format!("Remove #{} ", task.id);
                let suffix = "? y to remove · n or Esc to keep";
                let budget = width.saturating_sub(prefix.chars().count() + suffix.chars().count());
                Line::styled(
                    format!("{prefix}{}{suffix}", elide(&task.title, budget)),
                    Style::new().add_modifier(Modifier::BOLD),
                )
            });
        self.refused.map_or(question, |refusal| {
            Line::styled(refusal.message(), Style::new().add_modifier(Modifier::BOLD))
        })
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

/// Whether `view` shows the task `id` and it can still be removed: a cancelled or a running
/// one cannot.
fn removable(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && !cancelled(view, id) && !is_running(view, id))
}

/// Whether `view` shows the task `id` as running.
fn is_running(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Running)
}

/// Whether `view` shows the task `id` as cancelled.
fn cancelled(view: &QueueView, id: TaskId) -> bool {
    view.tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Cancelled)
}

/// The task to keep selected once `queue` replaces `previous`, given the selection it had: the
/// same task if it is still there, otherwise the one that took its place in the list, or the
/// last.
fn reselect(
    previous: Option<&QueueView>,
    selected: Option<TaskId>,
    queue: &QueueView,
) -> Option<TaskId> {
    let index = match (previous, selected) {
        (Some(_), Some(id)) if queue.tasks.iter().any(|task| task.id == id) => {
            return Some(id);
        }
        (Some(old), Some(id)) => old.tasks.iter().position(|task| task.id == id),
        _ => None,
    };
    let last = queue.tasks.len().checked_sub(1)?;
    queue
        .tasks
        .get(index.unwrap_or(0).min(last))
        .map(|task| task.id)
}

/// `message` — the last run's or import's own results, one per line — windowed to the
/// `height` lines that fit, scrolled to `offset`, clamped so the window never runs past the
/// last line.
fn message_lines(offset: usize, message: &[String], height: usize) -> Vec<Line<'static>> {
    if message.is_empty() || height == 0 {
        return Vec::new();
    }
    let first = offset.min(message.len().saturating_sub(height));
    message
        .iter()
        .skip(first)
        .take(height)
        .cloned()
        .map(Line::from)
        .collect()
}

/// The rows of the task list that fit in `height` lines, scrolled so that the selected task's
/// block is the last one in view when it would not be otherwise. The selected task is marked
/// with `>` and shown reversed; a cancelled one is dimmed and says so in its status. A task
/// that has an attempt carries one dimmed line per step run so far, in the same order
/// `status` prints them, from the same use case. When a task's own steps do not all fit, the
/// earliest are replaced by a single `…` line so the block still fits, keeping the most
/// recently finished steps, and the one still running, visible — never a line cut with no
/// sign of it.
fn task_lines(
    selected: Option<TaskId>,
    view: &QueueView,
    height: usize,
    width: usize,
) -> Vec<Line<'static>> {
    if view.tasks.is_empty() {
        return vec![Line::from("The queue is empty.")];
    }
    let columns = Columns::of(view);
    let selected_index = view.tasks.iter().position(|task| Some(task.id) == selected);
    let first = first_shown(&block_heights(view), selected_index, height);
    let mut lines = Vec::new();
    for (index, task) in view.tasks.iter().enumerate().skip(first) {
        let remaining = height.saturating_sub(lines.len());
        if remaining == 0 {
            break;
        }
        let attempt = view.attempts.get(&task.id);
        let steps = attempt.map_or_else(Vec::new, |attempt| step_lines(&attempt.steps, width));
        if !steps.is_empty() && remaining < 2 {
            break;
        }
        lines.push(task_line(
            task,
            attempt,
            Some(index) == selected_index,
            &columns,
            width,
        ));
        lines.extend(windowed(steps, remaining - 1));
    }
    lines
}

/// How many lines each task in `view.tasks` takes: one for the task itself, plus one per step
/// its attempt, if any, has run so far.
fn block_heights(view: &QueueView) -> Vec<usize> {
    view.tasks
        .iter()
        .map(|task| {
            1 + view
                .attempts
                .get(&task.id)
                .map_or(0, |attempt| attempt.steps.len())
        })
        .collect()
}

/// The width each of a task line's leading four columns needs to hold every task's own value
/// in the queue, so every row's title starts at the same offset as the one before it,
/// whichever task's position, ID, status or kind is widest.
struct Columns {
    position: usize,
    id: usize,
    status: usize,
    kind: usize,
}

impl Columns {
    fn of(view: &QueueView) -> Self {
        let mut columns = Self {
            position: 0,
            id: 0,
            status: 0,
            kind: 0,
        };
        for task in &view.tasks {
            let attempt = view.attempts.get(&task.id);
            let status = displayed_status(task.status, attempt.map(|attempt| attempt.outcome));
            columns.position = columns
                .position
                .max(task.position.to_string().chars().count());
            columns.id = columns.id.max(format!("#{}", task.id).chars().count());
            columns.status = columns.status.max(status.chars().count());
            columns.kind = columns.kind.max(task.kind.to_string().chars().count());
        }
        columns
    }
}

/// One line per step of `steps`, in order — the same lines `status` prints for the same
/// attempt, from the same use case: step, provider (`-` for a step the tool ran itself, which
/// names none), time spent, outcome, and the reason when there is one, cut to fit `width`
/// with a trailing `…` when it does not.
fn step_lines(steps: &[StepLine], width: usize) -> Vec<Line<'static>> {
    steps
        .iter()
        .map(|step| {
            let provider = step.provider.as_deref().unwrap_or("-");
            let seconds = step.time_spent.as_secs();
            let outcome = step.outcome;
            let prefix = format!("      {} · {provider} · {seconds}s · {outcome}", step.step);
            let text = step.reason.as_deref().map_or_else(
                || prefix.clone(),
                |reason| {
                    let budget = width.saturating_sub(prefix.chars().count() + 2);
                    format!("{prefix}: {}", elide(reason, budget))
                },
            );
            Line::styled(text, Style::new().add_modifier(Modifier::DIM))
        })
        .collect()
}

/// `lines`, kept to at most `budget`: shown in full when they already fit; otherwise the
/// earliest are dropped in favour of one leading `…` line, so the tail — the most recently
/// finished steps, and the one still running — stays visible, and the cut is never silent.
fn windowed(lines: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    if lines.len() <= budget {
        return lines;
    }
    let skip = lines.len() + 1 - budget;
    let mut shown = vec![Line::styled(
        "      …",
        Style::new().add_modifier(Modifier::DIM),
    )];
    shown.extend(lines.into_iter().skip(skip));
    shown
}

/// `task`'s row: its position, ID, status, kind and title, each of the first four padded to
/// `columns`' width so every row lines up under the one before it, and the title cut to fit
/// `width` with a trailing `…` when it does not. `attempt` — the same line `status` shows for
/// it, from the same use case — decides the status word when it says the task is shown
/// `interrupted` rather than `task.status`'s own `running`.
fn task_line(
    task: &Task,
    attempt: Option<&AttemptLine>,
    selected: bool,
    columns: &Columns,
    width: usize,
) -> Line<'static> {
    let marker = if selected { '>' } else { ' ' };
    let mut style = Style::new();
    if task.status == TaskStatus::Cancelled {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let position = task.position.to_string();
    let id = format!("#{}", task.id);
    let status = displayed_status(task.status, attempt.map(|attempt| attempt.outcome));
    let kind = task.kind.to_string();
    let prefix = format!(
        "{marker}{position:>pw$}  {id:<iw$}  {status:<sw$}  {kind:<kw$}  ",
        pw = columns.position,
        iw = columns.id,
        sw = columns.status,
        kw = columns.kind,
    );
    let budget = width.saturating_sub(prefix.chars().count());
    Line::styled(format!("{prefix}{}", elide(&task.title, budget)), style)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use ktask_core::{
        AttemptOutcome, IMPLEMENTATION, Outcome, Project, StatusSummary, TaskId, TaskKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

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
        assert_eq!(press(queue.clone(), &[KeyCode::Char('x')]).message, None);
    }

    #[test]
    fn a_run_message_is_shown_until_a_key_that_does_not_scroll_it_dismisses_it() {
        let queue = loaded(&[1]).run_message("task 1: done".to_owned());
        assert_eq!(
            queue.message.as_deref(),
            Some(["task 1: done".to_owned()].as_slice())
        );

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

    /// The rows of `queue` drawn on a `width` × `height` screen.
    fn drawn(queue: &Queue, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        queue.draw(area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn row(rows: &[String], y: usize) -> &str {
        rows[y].trim_end()
    }

    fn loaded_with_attempts(tasks: Vec<Task>, attempts: HashMap<TaskId, AttemptLine>) -> Queue {
        let summary = StatusSummary {
            pending: tasks.len(),
            ..StatusSummary::default()
        };
        let project = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        };
        Queue::default().loaded(QueueView {
            project,
            summary,
            tasks,
            attempts,
        })
    }

    fn task_named(position: usize, title: &str, kind: TaskKind) -> Task {
        Task {
            id: TaskId(position as u64 * 10),
            position,
            title: title.to_owned(),
            body: String::new(),
            criteria: vec!["it works".to_owned()],
            kind,
            links: vec![],
            status: TaskStatus::Pending,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn before_the_queue_is_loaded_the_screen_says_so() {
        let rows = drawn(&Queue::default(), 40, 5);
        assert_eq!(row(&rows, 0), "Loading the queue…");
    }

    #[test]
    fn an_empty_queue_shows_the_project_the_counts_and_a_message() {
        let rows = drawn(&loaded_with_attempts(vec![], HashMap::new()), 90, 8);
        assert_eq!(row(&rows, 0), "app");
        assert_eq!(
            row(&rows, 1),
            "pending 0  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
        );
        assert_eq!(row(&rows, 3), "The queue is empty.");
    }

    #[test]
    fn tasks_are_listed_in_order_with_position_id_status_kind_and_title() {
        let queue = loaded_with_attempts(
            vec![
                task_named(1, "first", TaskKind::Agent),
                task_named(2, "second", TaskKind::Human),
            ],
            HashMap::new(),
        );
        let rows = drawn(&queue, 90, 8);
        assert_eq!(row(&rows, 3), ">1  #10  pending  agent  first");
        assert_eq!(row(&rows, 4), " 2  #20  pending  human  second");
    }

    #[test]
    fn a_title_too_long_for_the_screen_is_cut_with_a_trailing_ellipsis() {
        let long_title = "x".repeat(60);
        let queue = loaded_with_attempts(
            vec![task_named(1, &long_title, TaskKind::Agent)],
            HashMap::new(),
        );
        let rows = drawn(&queue, 40, 8);
        let row = row(&rows, 3);
        assert!(row.starts_with(">1  #10  pending  agent  x"), "{row:?}");
        assert!(row.ends_with('…'), "{row:?}");
    }

    #[test]
    fn the_key_map_lists_every_key_of_the_queue_screen_instead_of_the_queue() {
        use ratatui::crossterm::event::KeyCode::Char;
        let queue = press(loaded(&[1]), &[Char('?')]);
        let rows = drawn(&queue, 60, 20);
        let screen = rows.join("\n");
        for key in [
            "j, Down", "k, Up", "g ", "G ", "a ", "d ", "r ", "i ", "s ", "? ", "Esc", "q ",
        ] {
            assert!(screen.contains(key), "{key:?} in\n{screen}");
        }
    }

    #[test]
    fn the_queues_key_map_names_no_key_twice_and_none_that_only_works_while_a_question_is_open() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in KEYS {
            assert!(seen.insert(key), "{key:?} listed twice in {KEYS:?}");
        }
        assert!(
            KEYS.iter().all(|(key, _)| *key != "y"),
            "{KEYS:?} lists `y`, which only answers a question the key map is never open under"
        );
    }

    #[test]
    fn asking_to_remove_a_task_names_it_and_the_keys_that_answer() {
        use ratatui::crossterm::event::KeyCode::Char;
        let queue = press(loaded(&[1, 2]), &[Char('j'), Char('d')]);
        let rows = drawn(&queue, 60, 8);
        assert_eq!(
            row(&rows, 2),
            "Remove #2 task 2? y to remove · n or Esc to keep"
        );
    }

    #[test]
    fn a_run_message_shows_in_place_of_the_task_list_one_line_per_line_it_printed() {
        let text = "task 1: done\ntask 2: failed: it broke\nnothing else is pending";
        let queue = loaded(&[1]).run_message(text.to_owned());
        let rows = drawn(&queue, 60, 8);
        assert_eq!(row(&rows, 3), "task 1: done");
        assert_eq!(row(&rows, 4), "task 2: failed: it broke");
    }

    #[test]
    fn a_run_that_refuses_to_start_shows_beside_the_task_list_not_in_place_of_it() {
        let queue = loaded(&[1]).run_message("nothing is pending".to_owned());
        let rows = drawn(&queue, 60, 8);
        assert_eq!(row(&rows, 2), "nothing is pending");
        assert!(row(&rows, 3).contains("first") || rows[3].contains('1'));
    }

    fn attempt(provider: &str, seconds: u64, outcome: AttemptOutcome) -> AttemptLine {
        AttemptLine {
            number: 1,
            step: IMPLEMENTATION.to_owned(),
            provider: Some(provider.to_owned()),
            time_spent: Duration::from_secs(seconds),
            outcome,
            reason: None,
            steps: vec![StepLine {
                step: IMPLEMENTATION.to_owned(),
                provider: Some(provider.to_owned()),
                time_spent: Duration::from_secs(seconds),
                outcome,
                reason: None,
            }],
        }
    }

    #[test]
    fn a_task_with_an_attempt_shows_the_same_line_status_would_for_it() {
        let mut attempts = HashMap::new();
        attempts.insert(TaskId(10), attempt("echo", 12, AttemptOutcome::Running));
        let queue = loaded_with_attempts(vec![task_named(1, "first", TaskKind::Agent)], attempts);
        let rows = drawn(&queue, 60, 8);
        assert_eq!(row(&rows, 4), "      implementation · echo · 12s · running");
    }

    #[test]
    fn a_done_attempt_carries_no_reason() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt("echo", 3, AttemptOutcome::Reported(Outcome::Done)),
        );
        let mut task = task_named(1, "first", TaskKind::Agent);
        task.status = TaskStatus::Done;
        let queue = loaded_with_attempts(vec![task], attempts);
        let rows = drawn(&queue, 60, 8);
        assert_eq!(row(&rows, 4), "      implementation · echo · 3s · done");
    }
}
