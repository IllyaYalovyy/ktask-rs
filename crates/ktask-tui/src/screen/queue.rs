//! The queue screen: ordered tasks with state, protocol, phase and attempts.
//!
//! The table is drawn into the body of the [`LayoutPlan`] that
//! [`layout_for`](crate::layout::layout_for) made for the frame, and every
//! cell goes through [`truncate_to_width`], so a narrow terminal loses the
//! ends of cells and the last columns, never the layout. Each state has its
//! own style as well as its own name, so the queue can be read at a glance.
//!
//! The selection is a row index in [`App::selected`]. Updates only ever
//! append tasks, so it keeps pointing at the same task while the queue
//! grows; if the queue is ever shorter than the index, the last row is shown
//! as selected instead.
//!
//! Ten keys act on the queue: `p` pause, `i` interrupt, `R` resume the queue,
//! `r` retry the selected task, `c` cancel it, `A` acknowledge its human gate
//! and `x` re-run its gates are [`Action`]s, the operations that have a CLI
//! command of the same name; [`update`](crate::update) does no I/O, so a key
//! that asks for one appends it to [`App::outbox`] and the shell carries it
//! out (see [`actions`](crate::actions)). `Enter` opens the selected task in
//! the inspector, `a` attaches to the run in progress and `d` opens the
//! selected task's diff; those three are [`ViewOp`]s or plain navigation and
//! change only what is shown. Each is offered under the rule its CLI command
//! applies (`pause` and `interrupt` need a task in flight, `resume` a queue
//! with something left to do and nothing running, `retry` a failed task,
//! `cancel` any but a finished or publishing one, `A` a task at a human gate,
//! `x` any but a finished task nothing is working); an operation the selected
//! state does not allow is drawn dimmed in the action bar, does nothing when
//! pressed, and says why in [`App::notice`].

use crate::actions::{apply_view, check_view};
use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::text::{display_width, truncate_to_width};
use crate::types::{Action, Screen, TaskView, ViewOp};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{PauseReason, TaskState};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

/// The column headings.
const HEADINGS: [&str; 5] = ["ID", "STATE", "PROTOCOL", "PHASE", "ATTEMPTS"];

/// The width each column asks for, in columns: wide enough for its heading
/// and for the longest value it can hold.
const WANTED: [usize; 5] = [4, 17, 10, 15, 8];

/// The fewest body rows that leave room for the action bar: the heading, three
/// tasks and the bar's first row. A shorter body gives every row to the tasks;
/// a taller one lets the bar take a second row.
const BAR_MIN_HEIGHT: u16 = 5;

/// What separates two entries of the action bar.
const BAR_GAP: &str = "  ";

/// The marker column that points at the selected row.
const MARKER: &str = "> ";

/// What the phase column shows for a task that has not entered a phase.
const NO_PHASE: &str = "-";

/// What the body shows when there are no tasks.
const EMPTY: &str = "No tasks queued.";

/// The style of a state's name, distinct for each state of
/// [`TaskState`]. A state this screen does not know is
/// drawn plainly, so a state added to the core is still shown.
#[must_use]
pub fn state_style(state: &str) -> Style {
    let style = Style::new();
    match state {
        "Queued" => style.fg(Color::DarkGray),
        "Preflight" => style.fg(Color::Cyan),
        "Running" => style.fg(Color::Blue).add_modifier(Modifier::BOLD),
        "Remediating" => style.fg(Color::Magenta),
        "Verifying" => style.fg(Color::Yellow),
        "Publishing" => style.fg(Color::LightBlue),
        "PublishedVerified" => style.fg(Color::LightGreen),
        "Done" => style.fg(Color::Green),
        "Acknowledged" => style.fg(Color::Green).add_modifier(Modifier::ITALIC),
        "Paused" => style.fg(Color::Yellow).add_modifier(Modifier::DIM),
        "Failed" => style.fg(Color::Red).add_modifier(Modifier::BOLD),
        "Cancelled" => style
            .fg(Color::DarkGray)
            .add_modifier(Modifier::CROSSED_OUT),
        _ => style,
    }
}

/// The row of the queue that is selected: the stored index, or the last row
/// when the queue has become shorter than it. `None` for an empty queue.
#[must_use]
pub fn selected_row(app: &App) -> Option<usize> {
    let last = app.tasks.len().checked_sub(1)?;
    Some(
        app.selected
            .get(&Screen::Queue)
            .map_or(0, |at| (*at).min(last)),
    )
}

/// What one of the queue's action keys asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Pause,
    Interrupt,
    Retry,
    Cancel,
    Inspect,
    Resume,
    Acknowledge,
    RerunGate,
    Attach,
    OpenDiff,
}

impl Operation {
    /// Every operation, in the order the action bar lists them.
    const ALL: [Operation; 10] = [
        Operation::Pause,
        Operation::Interrupt,
        Operation::Retry,
        Operation::Cancel,
        Operation::Inspect,
        Operation::Resume,
        Operation::Acknowledge,
        Operation::RerunGate,
        Operation::Attach,
        Operation::OpenDiff,
    ];

    /// The operation `key` asks for. Modifiers unbind the key, so `Ctrl-C`
    /// stays quit and never cancels; Shift is the exception, since it is
    /// how a capital letter is typed.
    fn from_key(key: &KeyEvent) -> Option<Self> {
        let modifiers = match key.code {
            KeyCode::Char(_) => key.modifiers - KeyModifiers::SHIFT,
            _ => key.modifiers,
        };
        if modifiers != KeyModifiers::NONE {
            return None;
        }
        match key.code {
            KeyCode::Char('p') => Some(Operation::Pause),
            KeyCode::Char('i') => Some(Operation::Interrupt),
            KeyCode::Char('r') => Some(Operation::Retry),
            KeyCode::Char('c') => Some(Operation::Cancel),
            KeyCode::Enter => Some(Operation::Inspect),
            KeyCode::Char('R') => Some(Operation::Resume),
            KeyCode::Char('A') => Some(Operation::Acknowledge),
            KeyCode::Char('x') => Some(Operation::RerunGate),
            KeyCode::Char('a') => Some(Operation::Attach),
            KeyCode::Char('d') => Some(Operation::OpenDiff),
            _ => None,
        }
    }

    /// The key as the action bar names it.
    fn key(self) -> &'static str {
        match self {
            Operation::Pause => "p",
            Operation::Interrupt => "i",
            Operation::Retry => "r",
            Operation::Cancel => "c",
            Operation::Inspect => "Enter",
            Operation::Resume => "R",
            Operation::Acknowledge => "A",
            Operation::RerunGate => "x",
            Operation::Attach => "a",
            Operation::OpenDiff => "d",
        }
    }

    /// The word the action bar and the refusals use: the CLI command's name
    /// for an action.
    fn label(self) -> &'static str {
        match self {
            Operation::Pause => "pause",
            Operation::Interrupt => "interrupt",
            Operation::Retry => "retry",
            Operation::Cancel => "cancel",
            Operation::Inspect => "inspect",
            Operation::Resume => "resume",
            Operation::Acknowledge => "ack",
            Operation::RerunGate => "rerun-gate",
            Operation::Attach => "attach",
            Operation::OpenDiff => "open-diff",
        }
    }

    /// The word the action bar uses, where it is short of room for the
    /// label: the same as the label except for the two longest.
    fn bar_label(self) -> &'static str {
        match self {
            Operation::RerunGate => "rerun",
            Operation::OpenDiff => "diff",
            other => other.label(),
        }
    }
}

/// What an available operation does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Ask for this action to be carried out.
    Dispatch(Action),
    /// Show the task on this row in the inspector.
    Inspect(usize),
    /// Change what is shown.
    View(ViewOp),
}

/// Whether a supervisor is in the middle of a task in `state`: an attempt is
/// in flight, and `pause` and `interrupt` have something to act on. The
/// states are named as [`TaskView::state`] names them.
pub(crate) fn in_flight(state: &str) -> bool {
    matches!(
        state,
        "Preflight" | "Running" | "Remediating" | "Verifying" | "Publishing"
    )
}

/// Why `cancel` cannot help a task in `state`, completing "cancel: task N
/// ...", or `None` when it can: nothing is running a queued, paused or failed
/// task, and a supervisor stops a running one; but a half-published change
/// cannot be cancelled and a finished task has nothing left to cancel.
fn cancel_refusal(state: &str) -> Option<String> {
    match state {
        "Publishing" => Some(
            "is publishing; a half-published change cannot be cancelled, and once it is \
             published it is done"
                .to_owned(),
        ),
        "PublishedVerified" | "Done" | "Acknowledged" | "Cancelled" => {
            let name = state.to_lowercase();
            Some(if state == "Cancelled" {
                format!("is already {name}")
            } else {
                format!("is already {name}; there is nothing to cancel")
            })
        }
        _ => None,
    }
}

/// The name of the state `task` is in. The journal is asked first, through
/// the inbox, which follows every task's state by the core's transition
/// table; the row's own state is what is left for a task no event has named.
fn state_of(app: &App, task: &TaskView) -> String {
    app.inbox
        .state(task.id)
        .map_or_else(|| task.state.clone(), |state| state.name().to_owned())
}

/// The first task, in queue order, that a supervisor is in the middle of.
pub(crate) fn running(app: &App) -> Option<&TaskView> {
    app.tasks
        .iter()
        .find(|task| in_flight(&state_of(app, task)))
}

/// Whether the journal put `task` at a human gate: paused, waiting for
/// `ack`.
fn at_gate(app: &App, task: &TaskView) -> bool {
    matches!(
        app.inbox.state(task.id),
        Some(TaskState::Paused {
            reason: PauseReason::HumanGate,
            ..
        })
    )
}

/// Whether `state` is one `rerun-gate` finds nothing to verify in: done,
/// acknowledged or cancelled. A task published and verified may still have its
/// worktree, so it is not among them.
fn nothing_to_verify(state: &str) -> bool {
    matches!(state, "Done" | "Acknowledged" | "Cancelled")
}

/// Whether the queue has moved past a task in `state`: the states `resume`
/// does not stop at.
fn complete(state: &str) -> bool {
    matches!(
        state,
        "PublishedVerified" | "Done" | "Acknowledged" | "Cancelled"
    )
}

/// What `operation` does now, or the reason it is not available: what the
/// operator is told when they press its key anyway.
fn plan(app: &App, operation: Operation) -> Result<Outcome, String> {
    let label = operation.label();
    let selected = selected_row(app).and_then(|row| app.tasks.get(row).map(|task| (row, task)));
    match (operation, selected) {
        (Operation::Pause | Operation::Interrupt, _) => {
            if running(app).is_some() {
                Ok(Outcome::Dispatch(if operation == Operation::Pause {
                    Action::Pause
                } else {
                    Action::Interrupt
                }))
            } else {
                Err(format!(
                    "{label}: no task is running; there is nothing to {label}"
                ))
            }
        }
        (Operation::Resume, _) => {
            if let Some(task) = running(app) {
                Err(format!(
                    "resume: task {} is {}; a run is already in progress",
                    task.id,
                    state_of(app, task).to_lowercase()
                ))
            } else if app.tasks.iter().all(|task| complete(&state_of(app, task))) {
                Err("resume: the queue is drained; there is nothing to resume".to_owned())
            } else {
                Ok(Outcome::Dispatch(Action::Resume))
            }
        }
        (Operation::Attach, _) => {
            let op = ViewOp::Attach;
            check_view(app, &op).map(|()| Outcome::View(op))
        }
        (_, None) => Err(format!("{label}: no task is selected")),
        (Operation::Inspect, Some((row, _))) => Ok(Outcome::Inspect(row)),
        (Operation::OpenDiff, Some((_, task))) => {
            let op = ViewOp::OpenDiff { task: task.id };
            check_view(app, &op).map(|()| Outcome::View(op))
        }
        (Operation::Retry, Some((_, task))) => {
            let state = state_of(app, task);
            if state == "Failed" {
                Ok(Outcome::Dispatch(Action::Retry { task: task.id }))
            } else {
                Err(format!(
                    "retry: task {} is {}; only a failed task can be retried",
                    task.id,
                    state.to_lowercase()
                ))
            }
        }
        (Operation::Cancel, Some((_, task))) => match cancel_refusal(&state_of(app, task)) {
            None => Ok(Outcome::Dispatch(Action::Cancel { task: task.id })),
            Some(why) => Err(format!("cancel: task {} {why}", task.id)),
        },
        (Operation::Acknowledge, Some((_, task))) => {
            if at_gate(app, task) {
                Ok(Outcome::Dispatch(Action::Acknowledge {
                    task: Some(task.id),
                }))
            } else if app.tasks.iter().any(|other| at_gate(app, other)) {
                Err(format!(
                    "ack: task {} is {}, not at a human gate",
                    task.id,
                    state_of(app, task).to_lowercase()
                ))
            } else {
                Err("ack: no human gate is pending".to_owned())
            }
        }
        (Operation::RerunGate, Some((_, task))) => {
            let state = state_of(app, task);
            let (id, name) = (task.id, state.to_lowercase());
            if nothing_to_verify(&state) {
                Err(format!(
                    "rerun-gate: task {id} is {name}; a finished task has nothing left to verify"
                ))
            } else if in_flight(&state) {
                Err(format!(
                    "rerun-gate: task {id} is {name} and a supervisor is still working it; \
                     interrupt it first"
                ))
            } else {
                Ok(Outcome::Dispatch(Action::RerunGate {
                    task: id,
                    gate: None,
                }))
            }
        }
    }
}

/// Carries out `operation` if it is available, otherwise leaves only the
/// reason in [`App::notice`].
fn perform(app: &mut App, operation: Operation) {
    match plan(app, operation) {
        Ok(Outcome::Dispatch(action)) => app.outbox.push(action),
        Ok(Outcome::Inspect(row)) => {
            app.selected.insert(Screen::Inspector, row);
            app.screen = Screen::Inspector;
        }
        Ok(Outcome::View(op)) => {
            if let Err(why) = apply_view(app, &op) {
                app.notice = Some(why);
            }
        }
        Err(why) => app.notice = Some(why),
    }
}

/// Handles the queue's keys: `p`, `i`, `r`, `c` and `Enter` (see the module
/// documentation), and `j`, `k`, the arrows, `g` and `G`, which move the
/// selection. Does nothing on other screens, under an overlay, or for other
/// keys. Any key press here first clears the notice the last one left.
///
/// The selection stops at the first and last rows rather than wrapping.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    app.notice = None;
    if let Some(operation) = Operation::from_key(key) {
        perform(app, operation);
        return;
    }
    let (Some(current), Some(last)) = (selected_row(app), app.tasks.len().checked_sub(1)) else {
        return;
    };
    let next = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::MoveDown) => (current + 1).min(last),
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => last,
        _ => return,
    };
    app.selected.insert(Screen::Queue, next);
}

/// The width of each column when `total` columns are available: columns take
/// what they ask for in order, and those that no longer fit get less, down to
/// none.
fn column_widths(total: usize) -> [usize; 5] {
    let mut left = total.saturating_sub(MARKER.len());
    WANTED.map(|wanted| {
        let width = wanted.min(left);
        left = left.saturating_sub(width + 1);
        width
    })
}

/// `text` cut to `width` columns and padded with spaces to exactly `width`.
fn cell(text: &str, width: usize) -> String {
    let cut = truncate_to_width(text, width);
    let padding = width - display_width(&cut);
    format!("{cut}{}", " ".repeat(padding))
}

/// Joins the spans of one row, leaving out the columns that got no width.
fn row_line(marker: &str, cells: [(String, Style); 5], widths: [usize; 5]) -> Line<'static> {
    let mut spans = vec![Span::raw(truncate_to_width(marker, widths_total(widths)))];
    let mut first = true;
    for ((text, style), width) in cells.into_iter().zip(widths) {
        if width == 0 {
            continue;
        }
        if !first {
            spans.push(Span::raw(" "));
        }
        first = false;
        spans.push(Span::styled(cell(&text, width), style));
    }
    Line::from(spans)
}

fn widths_total(widths: [usize; 5]) -> usize {
    MARKER.len() + widths.iter().sum::<usize>() + widths.iter().filter(|w| **w > 0).count()
}

fn task_line(task: &TaskView, selected: bool, widths: [usize; 5]) -> Line<'static> {
    let phase = task
        .phase
        .map_or_else(|| NO_PHASE.to_owned(), |phase| format!("{phase:?}"));
    let plain = Style::new();
    let cells = [
        (task.id.get().to_string(), plain),
        (task.state.clone(), state_style(&task.state)),
        (task.protocol.clone(), plain),
        (phase, plain),
        (task.attempts.to_string(), plain),
    ];
    let line = row_line(if selected { MARKER } else { "  " }, cells, widths);
    if selected {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

/// The action bar: each operation's key and name, dimmed when it is not
/// available for the selected task, in at most `max_rows` rows of `width`
/// columns. An entry goes on the row it fits on, or else starts the next
/// row; when there is no next row, the entry is cut at the edge.
fn action_bar(app: &App, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut left = 0;
    for operation in Operation::ALL {
        let style = if plan(app, operation).is_ok() {
            Style::new()
        } else {
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
        };
        let entry = format!("{} {}", operation.key(), operation.bar_label());
        let fits = left >= BAR_GAP.len() + display_width(&entry);
        let full = rows.len() >= max_rows;
        match rows.last_mut() {
            Some(row) if fits => {
                row.push(Span::raw(BAR_GAP));
                row.push(Span::styled(entry.clone(), style));
                left -= BAR_GAP.len() + display_width(&entry);
            }
            Some(row) if full => {
                // The gap is not part of the entry, so it does not take its style.
                let text = truncate_to_width(&format!("{BAR_GAP}{entry}"), left);
                left -= display_width(&text);
                match text.strip_prefix(BAR_GAP) {
                    Some(cut) => {
                        row.push(Span::raw(BAR_GAP));
                        row.push(Span::styled(cut.to_owned(), style));
                    }
                    None => row.push(Span::styled(text, style)),
                }
            }
            _ if max_rows > 0 => {
                let text = truncate_to_width(&entry, width);
                left = width - display_width(&text);
                rows.push(vec![Span::styled(text, style)]);
            }
            _ => {}
        }
    }
    rows.into_iter().map(Line::from).collect()
}

/// How many rows the action bar may take in a body `height` rows tall: none
/// when there is no room for it and a few tasks, one when there is, two when
/// there is room for one more.
fn bar_rows(height: u16) -> usize {
    match height {
        0..BAR_MIN_HEIGHT => 0,
        BAR_MIN_HEIGHT => 1,
        _ => 2,
    }
}

/// The last `rows` rows of `body`, where the action bar goes, and where the
/// notice goes in its place.
fn last_rows(body: Rect, rows: usize) -> Rect {
    let rows = u16::try_from(rows).unwrap_or(u16::MAX).min(body.height);
    Rect {
        y: body.bottom() - rows,
        height: rows,
        ..body
    }
}

/// Draws the queue into the body of `plan`: a heading row, then the tasks,
/// scrolled so that the selected one is in view, then the action bar or, when
/// there is a notice, the notice in its place.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let mut actions = Vec::new();
    if let Some(selected) = selected_row(app) {
        let widths = column_widths(width);
        let headings = HEADINGS.map(|heading| (heading.to_owned(), Style::new()));
        let mut lines =
            vec![row_line("  ", headings, widths).style(Style::new().add_modifier(Modifier::BOLD))];
        actions = action_bar(app, width, bar_rows(body.height));
        let rows = usize::from(body.height) - 1 - actions.len();
        let offset = (selected + 1).saturating_sub(rows);
        lines.extend(
            app.tasks
                .iter()
                .enumerate()
                .skip(offset)
                .take(rows)
                .map(|(at, task)| task_line(task, at == selected, widths)),
        );
        frame.render_widget(Paragraph::new(lines), body);
        if !actions.is_empty() {
            let area = last_rows(body, actions.len());
            frame.render_widget(Paragraph::new(actions.clone()), area);
        }
    } else {
        frame.render_widget(Paragraph::new(truncate_to_width(EMPTY, width)), body);
    }
    if let Some(notice) = &app.notice {
        let line = Span::styled(
            truncate_to_width(notice, width),
            Style::new().fg(Color::Yellow),
        );
        // The notice takes the place of the whole bar, not of its last row.
        frame.render_widget(Clear, last_rows(body, actions.len().max(1)));
        frame.render_widget(Paragraph::new(line), last_rows(body, 1));
    }
}

/// Whether the queue screen has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Queue && app.overlay.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{render as render_app, update};
    use crate::event::AppEvent;
    use crate::types::Overlay;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ktask_core::{
        AttemptId, Event, EventKind, EventSeq, FailureClass, PauseReason, Phase, TaskId, TaskState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use time::OffsetDateTime;

    fn view(id: u32, state: &str, phase: Option<Phase>, attempts: u32) -> TaskView {
        TaskView {
            id: TaskId::new(id),
            title: format!("Task {id}"),
            state: state.to_owned(),
            protocol: "tdd".to_owned(),
            phase,
            attempts,
            elapsed: None,
        }
    }

    fn app_with(tasks: Vec<TaskView>, size: (u16, u16)) -> App {
        App {
            tasks,
            ..App::new(size)
        }
    }

    fn draw(app: &App) -> Buffer {
        let (w, h) = app.size;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| render_app(app, frame)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The screen, one line per row, with trailing blanks removed.
    fn snapshot(app: &App) -> String {
        let buffer = draw(app);
        (0..app.size.1)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < app.size.0 {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    // A wide symbol also covers the cell after it.
                    x += u16::try_from(display_width(symbol).max(1)).unwrap_or(1);
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn selected(app: &App) -> Option<usize> {
        selected_row(app)
    }

    /// One of every `TaskState`, the state each names, and the phase it shows.
    fn every_state() -> Vec<(TaskState, Option<Phase>)> {
        let attempt = AttemptId::new(1);
        let inner = Box::new(TaskState::Queued);
        vec![
            (TaskState::Queued, None),
            (TaskState::Preflight, None),
            (
                TaskState::Running {
                    attempt,
                    phase: Phase::Red,
                },
                Some(Phase::Red),
            ),
            (
                TaskState::Remediating {
                    attempt,
                    phase: Phase::Green,
                },
                Some(Phase::Green),
            ),
            (TaskState::Verifying { attempt }, Some(Phase::Verify)),
            (TaskState::Publishing { attempt }, Some(Phase::Publish)),
            (
                TaskState::PublishedVerified {
                    commit: "abc".to_owned(),
                },
                None,
            ),
            (TaskState::Done, None),
            (
                TaskState::Acknowledged {
                    by: "ops".to_owned(),
                    at: OffsetDateTime::UNIX_EPOCH,
                },
                None,
            ),
            (
                TaskState::Paused {
                    reason: PauseReason::HumanGate,
                    resume_to: inner,
                },
                None,
            ),
            (
                TaskState::Failed {
                    class: FailureClass::AgentFailure,
                    detail: "boom".to_owned(),
                },
                None,
            ),
            (TaskState::Cancelled, None),
        ]
    }

    fn queued_event(id: u32) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(u64::from(id)),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(id)),
            kind: EventKind::TaskQueued {
                title: format!("Task {id}"),
            },
        })
    }

    #[test]
    fn queue_empty_shows_the_headline_and_no_rows() {
        let app = App::new((40, 5));
        assert_eq!(snapshot(&app), "1 Queue\nNo tasks queued.\n\n\n");
    }

    #[test]
    fn queue_snapshot_for_each_state_shows_its_name_phase_and_attempts() {
        let expected = [
            "> 1    Queued            tdd        -               0",
            "> 1    Preflight         tdd        -               0",
            "> 1    Running           tdd        Red             2",
            "> 1    Remediating       tdd        Green           2",
            "> 1    Verifying         tdd        Verify          2",
            "> 1    Publishing        tdd        Publish         2",
            "> 1    PublishedVerified tdd        -               2",
            "> 1    Done              tdd        -               2",
            "> 1    Acknowledged      tdd        -               2",
            "> 1    Paused            tdd        -               2",
            "> 1    Failed            tdd        -               2",
            "> 1    Cancelled         tdd        -               2",
        ];
        let states = every_state();
        assert_eq!(states.len(), expected.len());
        for ((state, phase), row) in states.iter().zip(expected) {
            let attempts = if state == &TaskState::Queued || state == &TaskState::Preflight {
                0
            } else {
                2
            };
            let app = app_with(vec![view(1, state.name(), *phase, attempts)], (64, 4));
            let want = format!(
                "1 Queue\n  ID   STATE             PROTOCOL   PHASE           ATTEMPTS\n{row}\n"
            );
            assert_eq!(snapshot(&app), want, "{}", state.name());
        }
    }

    #[test]
    fn queue_every_state_has_its_own_style() {
        let names: Vec<&str> = every_state().iter().map(|(s, _)| s.name()).collect();
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(state_style(a), state_style(b), "{a} and {b} look alike");
            }
            assert_ne!(state_style(a), Style::new(), "{a} is drawn plainly");
        }
        assert_eq!(state_style("Unheard-of"), Style::new());
    }

    #[test]
    fn queue_state_cell_is_drawn_in_its_state_style() {
        let names: Vec<&str> = every_state().iter().map(|(s, _)| s.name()).collect();
        for name in names {
            // Row 2 is the task; the state column starts after the marker
            // and the id column.
            let buffer = draw(&app_with(vec![view(1, name, None, 0)], (64, 4)));
            let cell = &buffer[(7, 2)];
            let want = state_style(name);
            assert_eq!(cell.fg, want.fg.unwrap_or(Color::Reset), "{name}");
            assert!(cell.modifier.contains(want.add_modifier), "{name}");
        }
    }

    #[test]
    fn queue_selected_row_is_marked_and_highlighted() {
        let mut app = app_with(
            vec![view(1, "Done", None, 1), view(2, "Running", None, 1)],
            (64, 5),
        );
        app = press(app, KeyCode::Char('j'));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[2].starts_with("  1 "), "{rows:?}");
        assert!(rows[3].starts_with("> 2 "), "{rows:?}");
        let buffer = draw(&app);
        assert!(buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(0, 2)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn queue_j_k_and_arrows_move_the_selection_and_stop_at_the_ends() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 8));
        assert_eq!(selected(&app), Some(0));
        for (code, want) in [
            (KeyCode::Char('j'), 1),
            (KeyCode::Down, 2),
            (KeyCode::Char('j'), 2),
            (KeyCode::Char('k'), 1),
            (KeyCode::Up, 0),
            (KeyCode::Char('k'), 0),
        ] {
            app = press(app, code);
            assert_eq!(selected(&app), Some(want), "{code:?}");
        }
    }

    #[test]
    fn queue_g_and_capital_g_jump_to_the_first_and_last_rows() {
        let tasks = (1..=5).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 8));
        app = press(app, KeyCode::Char('G'));
        assert_eq!(selected(&app), Some(4));
        app = press(app, KeyCode::Char('g'));
        assert_eq!(selected(&app), Some(0));
    }

    #[test]
    fn queue_keys_on_an_empty_queue_change_nothing() {
        let before = App::new((64, 8));
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Down,
            KeyCode::Up,
        ] {
            assert_eq!(press(before.clone(), code), before, "{code:?}");
        }
    }

    #[test]
    fn queue_movement_keys_are_ignored_off_the_queue_screen() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let before = App {
            screen: Screen::Logs,
            ..app_with(tasks, (64, 8))
        };
        assert_eq!(press(before.clone(), KeyCode::Char('j')), before);
        assert_eq!(press(before.clone(), KeyCode::Char('G')), before);
    }

    #[test]
    fn queue_movement_keys_are_ignored_under_an_overlay() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let before = App {
            overlay: Some(Overlay::KeyMap),
            ..app_with(tasks, (64, 8))
        };
        assert_eq!(press(before.clone(), KeyCode::Char('j')), before);
        assert!(!has_focus(&before));
    }

    #[test]
    fn queue_selection_survives_an_incoming_update() {
        let mut app = app_with(
            vec![view(1, "Done", None, 1), view(2, "Done", None, 1)],
            (64, 8),
        );
        app = press(app, KeyCode::Char('j'));
        app = update(app, queued_event(3));
        app = update(app, AppEvent::Tick);
        assert_eq!(app.tasks.len(), 3);
        assert_eq!(selected(&app), Some(1));
        let text = snapshot(&app);
        assert!(text.lines().any(|row| row.starts_with("> 2 ")), "{text}");
    }

    #[test]
    fn queue_selection_is_clamped_to_the_last_row_when_the_queue_is_shorter() {
        let mut app = app_with(vec![view(1, "Done", None, 1)], (64, 6));
        app.selected.insert(Screen::Queue, 9);
        assert_eq!(selected(&app), Some(0));
        assert!(snapshot(&app).contains("> 1 "));
    }

    #[test]
    fn queue_scrolls_to_keep_the_selection_in_view() {
        let tasks = (1..=10).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 5));
        app = press(app, KeyCode::Char('G'));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[1].contains("ID"), "{rows:?}");
        assert!(rows[2].starts_with("  8 "), "{rows:?}");
        assert!(rows[4].starts_with("> 10 "), "{rows:?}");
    }

    #[test]
    fn queue_cells_are_truncated_to_their_columns() {
        let mut long = view(
            1,
            "PublishedVerified",
            Some(Phase::AcceptanceTests),
            123_456_789,
        );
        long.protocol = "spec-first-with-a-long-name".to_owned();
        let app = app_with(vec![long], (90, 4));
        let text = snapshot(&app);
        let row = text.lines().nth(2).expect("task row");
        assert_eq!(
            row,
            "> 1    PublishedVerified spec-first AcceptanceTests 12345678"
        );
    }

    #[test]
    fn queue_wide_characters_are_never_split_by_truncation() {
        let mut task = view(1, "Queued", None, 0);
        task.protocol = "日本語日本語日本語".to_owned();
        let app = app_with(vec![task], (64, 4));
        let text = snapshot(&app);
        let row = text.lines().nth(2).expect("task row");
        assert!(row.contains("日本語日本 "), "{row}");
        assert!(!row.contains("日本語日本語"), "{row}");
    }

    #[test]
    fn queue_narrow_terminals_drop_trailing_columns_and_never_overflow() {
        let app = app_with(vec![view(1, "Running", Some(Phase::Red), 2)], (12, 4));
        let text = snapshot(&app);
        assert_eq!(text, "1 Queue\n  ID   STATE\n> 1    Runni\n");
        for size in [(0, 0), (1, 1), (2, 3), (0, 24), (80, 0), (5, 2), (9, 6)] {
            let app = app_with(vec![view(1, "Running", Some(Phase::Red), 2)], size);
            let _ = snapshot(&app);
            let empty = App::new(size);
            let _ = snapshot(&empty);
        }
    }

    #[test]
    fn queue_is_placed_in_the_body_of_the_layout_plan() {
        // 80x24 is the full layout: the header row above, the footer row below.
        let tasks = (1..=30).map(|id| view(id, "Queued", None, 0)).collect();
        let app = app_with(tasks, (80, 24));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows[0], "1 Queue");
        assert!(rows[1].contains("STATE"));
        assert!(rows[2].starts_with("> 1 "));
        // The body's last two rows are the action bar, so two fewer tasks show.
        assert_eq!(
            rows[20],
            "  19   Queued            tdd        -               0"
        );
        assert_eq!(
            rows[21],
            "p pause  i interrupt  r retry  c cancel  Enter inspect  R resume  A ack  x rerun"
        );
        assert_eq!(rows[22], "a attach  d diff");
        assert_eq!(rows[23], "Press ? for the key map");
    }
    // --- actions -----------------------------------------------------------

    use crate::testing::Harness;
    use crate::types::Action;

    /// A harness over `tasks` on a terminal tall enough for the action bar.
    fn harness_with(tasks: Vec<TaskView>) -> Harness {
        Harness::from_app(app_with(tasks, (80, 24)))
    }

    fn press_code(harness: &mut Harness, code: KeyCode) {
        harness.send(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// One task in each of the states an action's availability depends on.
    fn mixed_queue() -> Vec<TaskView> {
        vec![
            view(1, "Running", Some(Phase::Red), 1),
            view(2, "Failed", None, 2),
            view(3, "Queued", None, 0),
            view(4, "Done", None, 1),
        ]
    }

    #[test]
    fn queue_actions_dispatch_core_operations() {
        let mut harness = harness_with(mixed_queue());

        // Task 1 is running: pause, interrupt and cancel apply to it.
        harness.key('p');
        harness.key('i');
        harness.key('c');
        assert_eq!(
            harness.app().outbox,
            [
                Action::Pause,
                Action::Interrupt,
                Action::Cancel {
                    task: TaskId::new(1)
                }
            ]
        );
        assert_eq!(harness.app().notice, None);

        // Task 2 is failed: it can be retried, and cancelled.
        harness.key('j');
        harness.key('r');
        harness.key('c');
        let outbox = harness.app().outbox.clone();
        assert_eq!(
            outbox[3..],
            [
                Action::Retry {
                    task: TaskId::new(2)
                },
                Action::Cancel {
                    task: TaskId::new(2)
                }
            ]
        );

        // Each is the action whose CLI command it names.
        let commands: Vec<&str> = outbox.iter().map(Action::command).collect();
        assert_eq!(
            commands,
            ["pause", "interrupt", "cancel", "retry", "cancel"]
        );

        // Jump to the inspector is a view change, not an action.
        press_code(&mut harness, KeyCode::Enter);
        assert_eq!(harness.app().screen, Screen::Inspector);
        assert_eq!(harness.app().selected.get(&Screen::Inspector), Some(&1));
        assert_eq!(harness.app().outbox, outbox);
        assert!(harness.text().starts_with("5 Task inspector"));
    }

    #[test]
    fn queue_action_keys_act_on_the_selected_task() {
        let mut harness = harness_with(mixed_queue());
        for (moves, want) in [(0, 1), (1, 2), (1, 3)] {
            for _ in 0..moves {
                harness.key('j');
            }
            harness.key('c');
            let last = harness.app().outbox.last().cloned();
            assert_eq!(
                last,
                Some(Action::Cancel {
                    task: TaskId::new(want)
                })
            );
        }
    }

    #[test]
    fn queue_enter_opens_the_inspector_on_the_selected_task() {
        let mut harness = harness_with(mixed_queue());
        harness.key('G');
        press_code(&mut harness, KeyCode::Enter);
        assert_eq!(harness.app().screen, Screen::Inspector);
        assert_eq!(harness.app().selected.get(&Screen::Inspector), Some(&3));
        // The queue keeps its own selection for when the operator returns.
        assert_eq!(harness.app().selected.get(&Screen::Queue), Some(&3));
    }

    /// The actions that are available for a task in each state, named as the
    /// CLI names them, from the rules the CLI applies (`pause` and
    /// `interrupt` need a task in flight; `retry` a failed one; `cancel` any
    /// task but a finished or publishing one).
    fn expected_available(state: &TaskState) -> Vec<&'static str> {
        let in_flight = matches!(
            state,
            TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
                | TaskState::Publishing { .. }
        );
        let cancellable = matches!(
            state,
            TaskState::Queued
                | TaskState::Paused { .. }
                | TaskState::Failed { .. }
                | TaskState::Preflight
                | TaskState::Running { .. }
                | TaskState::Remediating { .. }
                | TaskState::Verifying { .. }
        );
        let mut available = Vec::new();
        if in_flight {
            available.extend(["pause", "interrupt"]);
        }
        if matches!(state, TaskState::Failed { .. }) {
            available.push("retry");
        }
        if cancellable {
            available.push("cancel");
        }
        available
    }

    #[test]
    fn queue_actions_are_available_exactly_in_the_states_the_cli_accepts() {
        for (state, phase) in every_state() {
            let mut harness = harness_with(vec![view(1, state.name(), phase, 1)]);
            for key in ['p', 'i', 'r', 'c'] {
                harness.key(key);
            }
            let dispatched: Vec<&str> = harness.app().outbox.iter().map(Action::command).collect();
            let mut want = expected_available(&state);
            want.sort_by_key(|name| {
                ["pause", "interrupt", "retry", "cancel"]
                    .iter()
                    .position(|n| n == name)
            });
            assert_eq!(dispatched, want, "{}", state.name());
        }
    }

    #[test]
    fn queue_bar_shows_an_unavailable_action_dimmed_and_an_available_one_plain() {
        let bars = [
            "p pause  i interrupt  r retry  c cancel  Enter inspect  R resume  A ack  x rerun",
            "a attach  d diff",
        ];
        for (state, running) in [("Failed", false), ("Running", true)] {
            let harness = harness_with(vec![view(1, state, None, 1)]);
            let text = harness.text();
            for (row, bar) in [21, 22].into_iter().zip(bars) {
                let shown = text.lines().nth(row).expect("bar row").trim_end();
                assert_eq!(shown, bar, "{state}");
            }
            let buffer = harness.buffer();
            // Where each action's key is: its row, its column and whether the
            // selected task lets it be used.
            for (name, row, column, available) in [
                ("pause", 21, 0, running),
                ("interrupt", 21, 9, running),
                ("retry", 21, 22, !running),
                ("cancel", 21, 31, true),
                ("inspect", 21, 41, true),
                ("resume", 21, 56, !running),
                ("ack", 21, 66, false),
                ("rerun", 21, 73, !running),
                ("attach", 22, 0, running),
                ("diff", 22, 10, true),
            ] {
                let cell = &buffer[(column, row)];
                let dimmed = cell.modifier.contains(Modifier::DIM);
                assert_eq!(dimmed, !available, "{name} with the task {state}");
                assert_eq!(cell.fg == Color::DarkGray, !available, "{name} {state}");
            }
        }
    }

    #[test]
    fn queue_bar_is_left_out_when_the_body_is_too_short_for_it() {
        let app = app_with(vec![view(1, "Failed", None, 1)], (64, 5));
        assert!(!snapshot(&app).contains("pause"));
        let app = app_with(vec![view(1, "Failed", None, 1)], (64, 6));
        assert!(snapshot(&app).contains("pause"));
    }

    #[test]
    fn queue_bar_is_cut_to_the_width_and_never_overflows() {
        let app = app_with(vec![view(1, "Failed", None, 1)], (20, 8));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows[6], "p pause  i interrupt");
        assert_eq!(rows[7], "r retry  c cancel  E");
        // With room for one row only, the rest of the bar is cut, not moved.
        let app = app_with(vec![view(1, "Failed", None, 1)], (20, 6));
        let text = snapshot(&app);
        assert_eq!(text.lines().nth(5), Some("p pause  i interrupt"));
        for size in [(1, 8), (3, 8), (10, 8), (0, 8)] {
            let _ = snapshot(&app_with(vec![view(1, "Failed", None, 1)], size));
        }
    }

    #[test]
    fn queue_bar_keeps_the_last_task_out_of_its_row() {
        let tasks = (1..=10).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 6));
        app = press(app, KeyCode::Char('G'));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[1].contains("ID"), "{rows:?}");
        assert!(rows[4].starts_with("> 10 "), "{rows:?}");
        assert!(rows[5].starts_with("p pause"), "{rows:?}");
    }

    #[test]
    fn queue_disabled_action_does_nothing_and_says_why() {
        let cases = [
            (
                'p',
                vec![view(1, "Queued", None, 0)],
                "pause: no task is running",
            ),
            (
                'i',
                vec![view(1, "Done", None, 1)],
                "interrupt: no task is running",
            ),
            (
                'r',
                vec![view(1, "Running", None, 1)],
                "retry: task 1 is running; only a failed task can be retried",
            ),
            (
                'c',
                vec![view(1, "Publishing", None, 1)],
                "cancel: task 1 is publishing",
            ),
            (
                'c',
                vec![view(1, "Done", None, 1)],
                "cancel: task 1 is already done; there is nothing to cancel",
            ),
            (
                'c',
                vec![view(1, "Cancelled", None, 1)],
                "cancel: task 1 is already cancelled",
            ),
        ];
        for (key, tasks, why) in cases {
            let mut harness = harness_with(tasks.clone());
            let before = harness.app().clone();
            harness.key(key);
            let after = harness.app().clone();
            assert_eq!(
                after.notice.as_deref().map(|n| n.starts_with(why)),
                Some(true),
                "{key}: {:?}",
                after.notice
            );
            assert!(after.outbox.is_empty(), "{key}");
            assert_eq!(
                App {
                    notice: None,
                    ..after
                },
                before,
                "{key}"
            );
            assert!(harness.text().contains(why), "{key}: {}", harness.text());
        }
    }

    #[test]
    fn queue_notice_is_drawn_in_place_of_the_bar_and_the_next_key_clears_it() {
        let mut harness = harness_with(vec![view(1, "Done", None, 1)]);
        harness.key('r');
        let shown = harness.text();
        assert!(shown.contains("retry: task 1 is done"), "{shown}");
        assert!(!shown.contains("p pause"), "{shown}");
        harness.key('j');
        assert_eq!(harness.app().notice, None);
        assert!(harness.text().contains("p pause"));
        assert!(!harness.text().contains("retry: task 1"));
    }

    #[test]
    fn queue_notice_is_shown_even_when_the_body_has_no_room_for_the_bar() {
        let mut app = app_with(vec![view(1, "Done", None, 1)], (64, 4));
        app = press(app, KeyCode::Char('r'));
        assert!(snapshot(&app).contains("retry: task 1 is done"));
    }

    #[test]
    fn queue_actions_on_an_empty_queue_are_disabled_with_a_reason() {
        for (key, why) in [
            ('p', "pause: no task is running; there is nothing to pause"),
            (
                'i',
                "interrupt: no task is running; there is nothing to interrupt",
            ),
            ('r', "retry: no task is selected"),
            ('c', "cancel: no task is selected"),
        ] {
            let mut harness = Harness::new(80, 5);
            harness.key(key);
            assert!(harness.app().outbox.is_empty(), "{key}");
            assert_eq!(harness.app().notice.as_deref(), Some(why));
            assert!(harness.text().contains(why), "{}", harness.text());
        }
        let mut harness = Harness::new(80, 5);
        press_code(&mut harness, KeyCode::Enter);
        assert_eq!(harness.app().screen, Screen::Queue);
        assert_eq!(
            harness.app().notice.as_deref(),
            Some("inspect: no task is selected")
        );
    }

    #[test]
    fn queue_action_keys_are_ignored_off_the_queue_screen_and_under_an_overlay() {
        for key in ['p', 'i', 'r', 'c'] {
            let before = App {
                screen: Screen::Logs,
                ..app_with(mixed_queue(), (80, 24))
            };
            assert_eq!(press(before.clone(), KeyCode::Char(key)), before, "{key}");
            let before = App {
                overlay: Some(Overlay::KeyMap),
                ..app_with(mixed_queue(), (80, 24))
            };
            assert_eq!(press(before.clone(), KeyCode::Char(key)), before, "{key}");
        }
        let before = App {
            screen: Screen::Logs,
            ..app_with(mixed_queue(), (80, 24))
        };
        assert_eq!(press(before.clone(), KeyCode::Enter), before);
    }

    #[test]
    fn queue_action_keys_need_no_modifier() {
        let before = app_with(mixed_queue(), (80, 24));
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for key in ['p', 'i', 'r', 'c'] {
                let event = AppEvent::Key(KeyEvent::new(KeyCode::Char(key), modifiers));
                assert_eq!(update(before.clone(), event), before, "{key} {modifiers:?}");
            }
            let event = AppEvent::Key(KeyEvent::new(KeyCode::Enter, modifiers));
            assert_eq!(update(before.clone(), event), before);
        }
    }

    #[test]
    fn queue_selection_is_clamped_for_actions_as_it_is_for_the_marker() {
        let mut app = app_with(mixed_queue(), (80, 24));
        app.selected.insert(Screen::Queue, 99);
        app = press(app, KeyCode::Char('c'));
        assert_eq!(app.outbox, []);
        assert!(app.notice.is_some_and(|n| n.contains("task 4")));
    }

    // --- the remaining actions ---------------------------------------------

    /// A journal event about `task`, as it would arrive from the bus.
    fn journal(task: u32, kind: EventKind) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(task)),
            kind,
        })
    }

    /// An interface over `count` queued tasks whose state the journal has
    /// moved on: each `(task, event)` is folded in.
    fn journaled(count: u32, events: Vec<(u32, EventKind)>) -> Harness {
        let mut harness = Harness::new(80, 24);
        for id in 1..=count {
            harness.send(queued_event(id));
        }
        for (task, kind) in events {
            harness.send(journal(task, kind));
        }
        harness
    }

    fn shifted(harness: &mut Harness, c: char) {
        harness.send(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::SHIFT,
        )));
    }

    fn told(harness: &Harness) -> Option<&str> {
        harness.app().notice.as_deref()
    }

    #[test]
    fn queue_actions_resume_is_asked_for_while_work_is_left_and_nothing_runs() {
        for states in [
            vec!["Queued"],
            vec!["Done", "Failed"],
            vec!["Cancelled", "Paused", "Done"],
            vec!["PublishedVerified", "Queued"],
        ] {
            let tasks = states
                .iter()
                .zip(1..)
                .map(|(state, id)| view(id, state, None, 0))
                .collect();
            let mut harness = harness_with(tasks);
            shifted(&mut harness, 'R');
            assert_eq!(harness.app().outbox, [Action::Resume], "{states:?}");
            assert_eq!(told(&harness), None);
        }
    }

    #[test]
    fn queue_actions_resume_is_capital_r_and_lower_case_r_is_still_retry() {
        let mut harness = harness_with(vec![view(1, "Queued", None, 0)]);
        harness.key('R');
        assert_eq!(harness.app().outbox, [Action::Resume]);
        // Lower case is retry, and refused for a task that has not failed.
        harness.key('r');
        assert_eq!(harness.app().outbox, [Action::Resume]);
        assert!(told(&harness).is_some_and(|n| n.starts_with("retry: task 1 is queued")));
    }

    #[test]
    fn queue_actions_resume_of_a_drained_queue_says_there_is_nothing_to_resume() {
        let want = "resume: the queue is drained; there is nothing to resume";
        let drained = vec![
            view(1, "Done", None, 1),
            view(2, "Acknowledged", None, 1),
            view(3, "Cancelled", None, 0),
            view(4, "PublishedVerified", None, 1),
        ];
        for tasks in [drained, Vec::new()] {
            let mut harness = harness_with(tasks);
            harness.key('R');
            assert!(harness.app().outbox.is_empty());
            assert_eq!(told(&harness), Some(want));
        }
    }

    #[test]
    fn queue_actions_resume_is_refused_while_a_task_is_running() {
        for state in [
            "Preflight",
            "Running",
            "Remediating",
            "Verifying",
            "Publishing",
        ] {
            let mut harness = harness_with(vec![view(1, "Done", None, 1), view(2, state, None, 1)]);
            harness.key('R');
            assert!(harness.app().outbox.is_empty(), "{state}");
            let want = format!(
                "resume: task 2 is {}; a run is already in progress",
                state.to_lowercase()
            );
            assert_eq!(told(&harness), Some(want.as_str()));
        }
    }

    #[test]
    fn queue_actions_acknowledge_names_the_selected_task_at_a_human_gate() {
        let mut harness = journaled(
            3,
            vec![(
                2,
                EventKind::Paused {
                    reason: PauseReason::HumanGate,
                },
            )],
        );
        harness.key('j');
        shifted(&mut harness, 'A');
        assert_eq!(
            harness.app().outbox,
            [Action::Acknowledge {
                task: Some(TaskId::new(2))
            }]
        );
        assert_eq!(told(&harness), None);
    }

    #[test]
    fn queue_actions_acknowledge_of_a_task_not_at_the_gate_names_the_state_it_is_in() {
        let mut harness = journaled(
            2,
            vec![(
                2,
                EventKind::Paused {
                    reason: PauseReason::HumanGate,
                },
            )],
        );
        shifted(&mut harness, 'A');
        assert!(harness.app().outbox.is_empty());
        assert_eq!(
            told(&harness),
            Some("ack: task 1 is queued, not at a human gate")
        );
    }

    #[test]
    fn queue_actions_acknowledge_says_when_no_gate_is_pending_at_all() {
        for events in [
            Vec::new(),
            vec![(
                1,
                EventKind::Paused {
                    reason: PauseReason::Input,
                },
            )],
        ] {
            let mut harness = journaled(1, events);
            shifted(&mut harness, 'A');
            assert!(harness.app().outbox.is_empty());
            assert_eq!(told(&harness), Some("ack: no human gate is pending"));
        }
        let mut harness = Harness::new(80, 24);
        harness.key('A');
        assert_eq!(told(&harness), Some("ack: no task is selected"));
    }

    #[test]
    fn queue_actions_read_a_tasks_state_from_the_journal_before_the_row() {
        // The row still says it was queued; the journal has moved it on.
        let mut harness = journaled(1, vec![(1, EventKind::PreflightStarted)]);
        assert_eq!(harness.app().tasks[0].state, "Queued");
        harness.key('p');
        harness.key('i');
        assert_eq!(harness.app().outbox, [Action::Pause, Action::Interrupt]);
        harness.key('R');
        assert_eq!(
            told(&harness),
            Some("resume: task 1 is preflight; a run is already in progress")
        );
        harness.key('a');
        assert_eq!(harness.app().screen, Screen::LiveRun);
    }

    #[test]
    fn queue_actions_rerun_gate_is_asked_for_a_task_nothing_is_working() {
        for state in ["Queued", "Paused", "Failed", "PublishedVerified"] {
            let mut harness = harness_with(vec![view(1, state, None, 1)]);
            harness.key('x');
            assert_eq!(
                harness.app().outbox,
                [Action::RerunGate {
                    task: TaskId::new(1),
                    gate: None
                }],
                "{state}"
            );
            assert_eq!(told(&harness), None, "{state}");
        }
    }

    #[test]
    fn queue_actions_rerun_gate_refuses_a_finished_task_and_one_being_worked() {
        for state in ["Done", "Acknowledged", "Cancelled"] {
            let mut harness = harness_with(vec![view(1, state, None, 1)]);
            harness.key('x');
            assert!(harness.app().outbox.is_empty(), "{state}");
            let want = format!(
                "rerun-gate: task 1 is {}; a finished task has nothing left to verify",
                state.to_lowercase()
            );
            assert_eq!(told(&harness), Some(want.as_str()));
        }
        for state in [
            "Preflight",
            "Running",
            "Remediating",
            "Verifying",
            "Publishing",
        ] {
            let mut harness = harness_with(vec![view(1, state, None, 1)]);
            harness.key('x');
            assert!(harness.app().outbox.is_empty(), "{state}");
            let want = format!(
                "rerun-gate: task 1 is {} and a supervisor is still working it; interrupt it first",
                state.to_lowercase()
            );
            assert_eq!(told(&harness), Some(want.as_str()));
        }
    }

    #[test]
    fn queue_actions_rerun_gate_acts_on_the_selected_task() {
        let mut harness = harness_with(mixed_queue());
        harness.key('j');
        harness.key('x');
        assert_eq!(
            harness.app().outbox,
            [Action::RerunGate {
                task: TaskId::new(2),
                gate: None
            }]
        );
        harness.key('j');
        harness.key('j');
        harness.key('x');
        assert_eq!(harness.app().outbox.len(), 1);
        assert!(told(&harness).is_some_and(|n| n.starts_with("rerun-gate: task 4 is done")));
    }

    #[test]
    fn queue_actions_attach_shows_the_live_run_following_and_asks_for_nothing() {
        let mut app = app_with(mixed_queue(), (80, 24));
        app.follow = false;
        let mut harness = Harness::from_app(app);
        harness.key('a');
        assert_eq!(harness.app().screen, Screen::LiveRun);
        assert!(harness.app().follow);
        assert!(harness.app().outbox.is_empty());
        assert!(harness.text().starts_with("2 Live run"));
    }

    #[test]
    fn queue_actions_attach_with_nothing_running_says_so_and_stays() {
        let mut harness =
            harness_with(vec![view(1, "Failed", None, 1), view(2, "Queued", None, 0)]);
        harness.key('a');
        assert_eq!(harness.app().screen, Screen::Queue);
        assert_eq!(
            told(&harness),
            Some("attach: no task is running; there is nothing to attach to")
        );
    }

    #[test]
    fn queue_actions_open_diff_shows_the_git_screen_for_the_selected_task() {
        let mut harness = harness_with(mixed_queue());
        harness.key('j');
        harness.key('j');
        harness.key('d');
        assert_eq!(harness.app().screen, Screen::Git);
        assert_eq!(harness.app().selected.get(&Screen::Queue), Some(&2));
        assert!(harness.app().outbox.is_empty());
        assert!(harness.text().starts_with("8 Git"));
    }

    #[test]
    fn queue_actions_open_diff_needs_a_selected_task() {
        let mut harness = Harness::new(80, 24);
        harness.key('d');
        assert_eq!(harness.app().screen, Screen::Queue);
        assert_eq!(told(&harness), Some("open-diff: no task is selected"));
    }

    #[test]
    fn queue_actions_new_keys_are_ignored_off_the_queue_screen_and_under_an_overlay() {
        for key in ['R', 'A', 'x', 'a', 'd'] {
            let before = App {
                screen: Screen::Logs,
                ..app_with(mixed_queue(), (80, 24))
            };
            let after = press(before.clone(), KeyCode::Char(key));
            assert_eq!(after.screen, Screen::Logs, "{key}");
            assert!(after.outbox.is_empty(), "{key}");
            let before = App {
                overlay: Some(Overlay::KeyMap),
                ..app_with(mixed_queue(), (80, 24))
            };
            assert_eq!(press(before.clone(), KeyCode::Char(key)), before, "{key}");
        }
    }

    #[test]
    fn queue_actions_new_keys_need_no_modifier_but_shift() {
        let before = app_with(mixed_queue(), (80, 24));
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for key in ['R', 'A', 'x', 'a', 'd'] {
                let event = AppEvent::Key(KeyEvent::new(KeyCode::Char(key), modifiers));
                assert_eq!(update(before.clone(), event), before, "{key} {modifiers:?}");
            }
        }
    }

    #[test]
    fn queue_actions_bar_lists_every_operation_with_its_key_on_a_terminal_of_80_columns() {
        let harness = harness_with(vec![view(1, "Failed", None, 1)]);
        let text = harness.text();
        let bar: Vec<&str> = text.lines().skip(21).take(2).collect();
        for entry in [
            "p pause",
            "i interrupt",
            "r retry",
            "c cancel",
            "Enter inspect",
            "R resume",
            "A ack",
            "x rerun",
            "a attach",
            "d diff",
        ] {
            assert!(
                bar.iter().any(|row| row.contains(entry)),
                "{entry}: {bar:?}"
            );
        }
    }

    #[test]
    fn queue_actions_notice_takes_the_place_of_both_bar_rows() {
        let mut harness = harness_with(vec![view(1, "Done", None, 1)]);
        harness.key('x');
        let text = harness.text();
        let rows: Vec<&str> = text.lines().collect();
        assert!(
            rows[22].starts_with("rerun-gate: task 1 is done"),
            "{rows:?}"
        );
        assert!(!rows[21].contains("p pause"), "{rows:?}");
        assert!(!text.contains("a attach"));
    }

    #[test]
    fn queue_actions_bar_rows_never_hold_more_than_the_width_or_more_rows_than_allowed() {
        let app = app_with(mixed_queue(), (80, 24));
        for max_rows in 0..=3 {
            for width in 0..=110 {
                let rows = action_bar(&app, width, max_rows);
                assert!(rows.len() <= max_rows, "{width} wide, {max_rows} rows");
                for row in &rows {
                    assert!(row.width() <= width, "{width} wide: {row:?}");
                }
            }
        }
    }

    #[test]
    fn queue_actions_bar_wraps_whole_entries_onto_the_second_row() {
        let app = app_with(mixed_queue(), (80, 24));
        let text = |rows: Vec<Line<'static>>| -> Vec<String> {
            rows.iter().map(ToString::to_string).collect()
        };
        assert_eq!(
            text(action_bar(&app, 80, 2)),
            [
                "p pause  i interrupt  r retry  c cancel  Enter inspect  R resume  A ack  x rerun",
                "a attach  d diff"
            ]
        );
        assert_eq!(
            text(action_bar(&app, 120, 2)),
            [
                "p pause  i interrupt  r retry  c cancel  Enter inspect  R resume  A ack  x rerun  \
              a attach  d diff"
            ]
        );
        // Out of rows, the last entry that fits is cut, and none after it shows.
        assert_eq!(
            text(action_bar(&app, 60, 1)),
            ["p pause  i interrupt  r retry  c cancel  Enter inspect  R re"]
        );
        assert!(action_bar(&app, 80, 0).is_empty());
    }

    #[test]
    fn queue_actions_notice_leaves_the_footer_alone() {
        let mut harness = harness_with(vec![view(1, "Done", None, 1)]);
        harness.key('x');
        let text = harness.text();
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[22].starts_with("rerun-gate:"), "{rows:?}");
        assert_eq!(rows[23].trim_end(), "Press ? for the key map");
        assert_eq!(rows[21].trim_end(), "");
    }
}
