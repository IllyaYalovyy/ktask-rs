//! One task's row on the queue screen.

use ktask_core::{AttemptLine, QueueView, Task, TaskStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::presentation;
use crate::widgets::elide;

/// The width each of a task line's leading four columns needs to hold every task's own value
/// in the queue, so every row's title starts at the same offset as the one before it,
/// whichever task's position, ID, status or kind is widest.
pub(super) struct Columns {
    position: usize,
    id: usize,
    status: usize,
    kind: usize,
}

impl Columns {
    pub(super) fn of(view: &QueueView) -> Self {
        let mut columns = Self {
            position: 0,
            id: 0,
            status: 0,
            kind: 0,
        };
        for task in &view.tasks {
            let attempt = view.attempts.get(&task.id);
            let status =
                presentation::task_status(task.status, attempt.map(|attempt| attempt.outcome));
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

/// `task`'s row: its position, ID, status, kind and title, each of the first four padded to
/// `columns`' width so every row lines up under the one before it, and the title cut to fit
/// `width` with a trailing `…` when it does not. `attempt` — the same line `status` shows for
/// it, from the same use case — decides the status word when it says the task is shown
/// `interrupted` rather than `task.status`'s own `running`.
pub(super) fn task_line(
    task: &Task,
    attempt: Option<&AttemptLine>,
    selected: bool,
    columns: &Columns,
    width: usize,
) -> Line<'static> {
    let (marker, style) = task_style(task, selected);
    let position = task.position.to_string();
    let id = format!("#{}", task.id);
    let status = presentation::task_status(task.status, attempt.map(|attempt| attempt.outcome));
    let kind = task.kind.to_string();
    let prefix = format!(
        "{marker}{position:>pw$}  {id:<iw$}  {status:<sw$}  {kind:<kw$}  ",
        pw = columns.position,
        iw = columns.id,
        sw = columns.status,
        kw = columns.kind,
    );
    let selection = task_selection(task);
    let budget = width.saturating_sub(prefix.chars().count());
    Line::styled(
        format!(
            "{prefix}{}{}",
            elide(
                &task.title,
                budget.saturating_sub(selection.chars().count())
            ),
            selection
        ),
        style,
    )
}

/// The row marker and emphasis for a task's status and selection.
fn task_style(task: &Task, selected: bool) -> (char, Style) {
    let mut style = Style::new();
    if matches!(
        task.status,
        TaskStatus::Cancelled | TaskStatus::Skipped | TaskStatus::Superseded
    ) {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    (if selected { '>' } else { ' ' }, style)
}

/// The task-level provider and model selection printed after the title.
fn task_selection(task: &Task) -> String {
    match (task.provider.as_deref(), task.model.as_deref()) {
        (Some(provider), Some(model)) => format!(" · {provider} ({model})"),
        (Some(provider), None) => format!(" · {provider}"),
        (None, Some(model)) => format!(" · model: {model}"),
        (None, None) => String::new(),
    }
}
