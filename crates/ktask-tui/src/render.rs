//! Draws an [`App`] into a buffer.

use ktask_core::{
    AttemptLine, CancelError, QueueView, StepLine, Task, TaskStatus, displayed_status,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Widget};

use crate::form_screen;
use crate::settings_screen;
use crate::{App, Confirming};

/// Every key the queue screen answers, and what it does.
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
    ("s", "open the project's settings"),
    ("y", "answer yes when asked to remove a task"),
    ("n", "answer no when asked to remove a task"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map, or answer no"),
    ("q", "quit"),
];

/// What the form's frame says at the bottom: the keys that are not typing.
const FORM_KEYS: &str =
    " Ctrl-S add · Esc cancel · Tab, Shift-Tab field · Ctrl-N, Ctrl-D criterion ";

/// What the form's frame says at the bottom while it asks to discard the task.
const DISCARD_KEYS: &str = " y discard · n, Esc keep writing ";

/// What the settings screen's frame says at the bottom.
const SETTINGS_KEYS: &str = " Tab, Shift-Tab field · Ctrl-S save · Esc cancel ";

/// Draws `app` over the whole of `area`, and returns where the cursor goes when it is shown.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let discarding = app.confirming == Some(Confirming::Discard);
    let block = Block::bordered()
        .title(" ktask-rs ")
        .title_bottom(if discarding {
            DISCARD_KEYS
        } else if app.settings.is_some() {
            SETTINGS_KEYS
        } else if app.form.is_some() {
            FORM_KEYS
        } else {
            " q quit · ? keys "
        });
    let inner = block.inner(area);
    block.render(area, buf);
    if let Some(settings) = &app.settings {
        return Some(settings_screen::draw(settings, inner, buf));
    }
    if let Some(form) = &app.form {
        return form_screen::draw(form, discarding, inner, buf);
    }
    match &app.queue {
        None => Paragraph::new("Loading the queue…").render(inner, buf),
        Some(_) if app.help => key_map(inner, buf),
        Some(queue) => draw_queue(app, queue, inner, buf),
    }
    None
}

/// Draws the header — the project, the counts and the question line — over `area`'s first
/// three rows, then, under it, either the last run's results, one per line, when there are
/// any, or the task list otherwise.
fn draw_queue(app: &App, queue: &QueueView, area: Rect, buf: &mut Buffer) {
    let [header, list] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    Paragraph::new(header_lines(app, queue)).render(header, buf);
    let height = usize::from(list.height);
    match &app.run_message {
        Some(message) => Paragraph::new(run_message_lines(app, message, height)).render(list, buf),
        None => Paragraph::new(task_lines(app, queue, height)).render(list, buf),
    }
}

fn key_map(area: Rect, buf: &mut Buffer) {
    let width = KEYS
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![
        Line::styled("Keys", Style::new().add_modifier(Modifier::BOLD)),
        Line::default(),
    ];
    lines.extend(
        KEYS.iter()
            .map(|(key, does)| Line::from(format!("{key:<width$}  {does}"))),
    );
    Paragraph::new(lines).render(area, buf);
}

/// The header: the project, the counts, and the question of a removal or the refusal of one,
/// while there is either.
/// The one-line question or notice shown under the summary — a removal confirmation or a
/// running-task refusal, whichever was raised most recently — or an empty line when neither
/// applies. The last run this screen started's own results are shown in the list area below
/// instead, by [`run_message_lines`], since there can be more than one line of them.
fn question_line(app: &App, queue: &QueueView) -> Line<'static> {
    let question = app
        .confirming
        .and_then(|confirm| match confirm {
            Confirming::Removal(id) => queue.tasks.iter().find(|task| task.id == id),
            Confirming::Discard => None,
        })
        .map_or_else(Line::default, |task| {
            Line::styled(
                format!(
                    "Remove #{} {}? y to remove · n or Esc to keep",
                    task.id, task.title
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )
        });
    app.refused.map_or(question, |id| {
        Line::styled(
            CancelError::Running(id).to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )
    })
}

/// `message` — the last run this screen started's own results, one per line, the same words
/// `ktask-rs run` itself printed — windowed to the `height` lines that fit, scrolled to
/// `app.run_message_offset`, clamped so the window never runs past the last line.
fn run_message_lines(app: &App, message: &[String], height: usize) -> Vec<Line<'static>> {
    if message.is_empty() || height == 0 {
        return Vec::new();
    }
    let first = app
        .run_message_offset
        .min(message.len().saturating_sub(height));
    message
        .iter()
        .skip(first)
        .take(height)
        .cloned()
        .map(Line::from)
        .collect()
}

fn header_lines(app: &App, queue: &QueueView) -> Vec<Line<'static>> {
    let summary = queue.summary;
    vec![
        Line::styled(
            queue.project.name.clone(),
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
        question_line(app, queue),
    ]
}

/// The rows of the task list that fit in `height` lines, scrolled so that the selected task's
/// block is the last one in view when it would not be otherwise. The selected task is marked
/// with `>` and shown reversed; a cancelled one is dimmed and says so in its status. A task
/// that has an attempt carries one dimmed line per step run so far, in the same order
/// `status` prints them, from the same use case. When a task's own steps do not all fit, the
/// earliest are replaced by a single `…` line so the block still fits, keeping the most
/// recently finished steps, and the one still running, visible — never a line cut with no
/// sign of it.
fn task_lines(app: &App, queue: &QueueView, height: usize) -> Vec<Line<'static>> {
    if queue.tasks.is_empty() {
        return vec![Line::from("The queue is empty.")];
    }
    let selected = queue
        .tasks
        .iter()
        .position(|task| Some(task.id) == app.selected);
    let block_heights: Vec<usize> = queue
        .tasks
        .iter()
        .map(|task| {
            1 + queue
                .attempts
                .get(&task.id)
                .map_or(0, |attempt| attempt.steps.len())
        })
        .collect();
    let first = first_shown(&block_heights, selected, height);
    let mut lines = Vec::new();
    for (index, task) in queue.tasks.iter().enumerate().skip(first) {
        let remaining = height.saturating_sub(lines.len());
        if remaining == 0 {
            break;
        }
        let attempt = queue.attempts.get(&task.id);
        let steps = attempt.map_or_else(Vec::new, |attempt| step_lines(&attempt.steps));
        if !steps.is_empty() && remaining < 2 {
            break;
        }
        lines.push(task_line(task, attempt, Some(index) == selected));
        lines.extend(windowed(steps, remaining - 1));
    }
    lines
}

/// The index of the first task shown: it walks back from the selected task, adding earlier
/// tasks while the lines of everything from there to the selection still fit in `height`, so
/// the selection's block ends up the last one in view exactly when it would not fit otherwise.
fn first_shown(block_heights: &[usize], selected: Option<usize>, height: usize) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let mut first = selected;
    let mut shown = block_heights.get(selected).copied().unwrap_or(1);
    while first > 0 {
        let Some(&before) = block_heights.get(first - 1) else {
            break;
        };
        if shown + before > height {
            break;
        }
        first -= 1;
        shown += before;
    }
    first
}

/// One line per step of `steps`, in order — the same lines `status` prints for the same
/// attempt, from the same use case: step, provider (`-` for a step the tool ran itself, which
/// names none), time spent, outcome, and the reason when there is one.
fn step_lines(steps: &[StepLine]) -> Vec<Line<'static>> {
    steps
        .iter()
        .map(|step| {
            let provider = step.provider.as_deref().unwrap_or("-");
            let mut text = format!(
                "      {} · {provider} · {}s · {}",
                step.step,
                step.time_spent.as_secs(),
                step.outcome
            );
            if let Some(reason) = &step.reason {
                text.push_str(": ");
                text.push_str(reason);
            }
            Line::styled(text, Style::new().add_modifier(Modifier::DIM))
        })
        .collect()
}

/// `lines`, kept to at most `budget`: shown in full when they already fit; otherwise the
/// earliest are dropped in favour of one leading `…` line, so the tail — the most recently
/// finished steps, and the one still running — stays visible, and the cut is never silent.
/// The caller never asks for `budget` `0` when `lines` is not itself empty, so that case is
/// not one this needs to make room for.
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

/// `task`'s row: its position, ID, status, kind and title. `attempt` — the same line
/// `status` shows for it, from the same use case — decides the status word when it says the
/// task is shown `interrupted` rather than `task.status`'s own `running`.
fn task_line(task: &Task, attempt: Option<&AttemptLine>, selected: bool) -> Line<'static> {
    let marker = if selected { '>' } else { ' ' };
    let mut style = Style::new();
    if task.status == TaskStatus::Cancelled {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let status = displayed_status(task.status, attempt.map(|attempt| attempt.outcome));
    Line::styled(
        format!(
            "{marker}{:>3}  #{}  {}  {}  {}",
            task.position, task.id, status, task.kind, task.title
        ),
        style,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use ktask_core::{
        AttemptLine, AttemptOutcome, IMPLEMENTATION, Outcome, Project, QueueView, SettingView,
        StatusSummary, Task, TaskId, TaskKind, TaskStatus,
    };

    use crate::{Event, update};

    use super::*;

    /// The rows of `app` drawn on a `width` × `height` screen.
    fn drawn(app: &App, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(app, area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    /// What a row of the frame holds between its left and right borders, without padding.
    fn inside(row: &str) -> &str {
        row.trim_start_matches('│').trim_end_matches('│').trim_end()
    }

    fn task(position: usize, title: &str, kind: TaskKind) -> Task {
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

    fn loaded(tasks: Vec<Task>) -> App {
        loaded_with_attempts(tasks, HashMap::new())
    }

    fn loaded_with_attempts(tasks: Vec<Task>, attempts: HashMap<TaskId, AttemptLine>) -> App {
        let summary = StatusSummary {
            pending: tasks.len(),
            ..StatusSummary::default()
        };
        let project = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        };
        update(
            App::default(),
            Event::Loaded(QueueView {
                project,
                summary,
                tasks,
                attempts,
            }),
        )
    }

    /// The provider named for a step called `name`, run with `provider`: `IMPLEMENTATION`,
    /// `REVIEW_STEP` and `TEST_STEP` are run by an agent and name it; every other step is run
    /// by the tool itself and names none — the same split `ktask_core::status` makes.
    fn step_provider(name: &str, provider: &str) -> Option<String> {
        if name == IMPLEMENTATION
            || name == ktask_core::REVIEW_STEP
            || name == ktask_core::TEST_STEP
        {
            Some(provider.to_owned())
        } else {
            None
        }
    }

    /// An attempt line as `status` would build it: step [`IMPLEMENTATION`], `provider`, having
    /// run for `time_spent_secs`, ending at `outcome` with `reason`.
    fn attempt(
        provider: &str,
        time_spent_secs: u64,
        outcome: AttemptOutcome,
        reason: Option<&str>,
    ) -> AttemptLine {
        AttemptLine {
            number: 1,
            step: IMPLEMENTATION.to_owned(),
            provider: Some(provider.to_owned()),
            time_spent: Duration::from_secs(time_spent_secs),
            outcome,
            reason: reason.map(str::to_owned),
            steps: vec![StepLine {
                step: IMPLEMENTATION.to_owned(),
                provider: Some(provider.to_owned()),
                time_spent: Duration::from_secs(time_spent_secs),
                outcome,
                reason: reason.map(str::to_owned),
            }],
        }
    }

    /// An attempt line whose `steps` are built from `(step, seconds, outcome, reason)` tuples,
    /// in order; its own top-level fields carry the last one, as `status` builds it.
    fn attempt_with_steps(
        provider: &str,
        steps: &[(&str, u64, AttemptOutcome, Option<&str>)],
    ) -> AttemptLine {
        let lines: Vec<StepLine> = steps
            .iter()
            .map(|&(step, seconds, outcome, reason)| StepLine {
                step: step.to_owned(),
                provider: step_provider(step, provider),
                time_spent: Duration::from_secs(seconds),
                outcome,
                reason: reason.map(str::to_owned),
            })
            .collect();
        let last = lines.last().cloned().expect("at least one step");
        AttemptLine {
            number: 1,
            step: last.step,
            provider: last.provider,
            time_spent: last.time_spent,
            outcome: last.outcome,
            reason: last.reason,
            steps: lines,
        }
    }

    #[test]
    fn an_empty_queue_shows_the_project_the_counts_and_a_message() {
        let rows = drawn(&loaded(vec![]), 90, 8);
        assert_eq!(inside(&rows[1]), "app");
        assert_eq!(
            inside(&rows[2]),
            "pending 0  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
        );
        assert_eq!(inside(&rows[4]), "The queue is empty.");
    }

    #[test]
    fn tasks_are_listed_in_order_with_position_id_status_kind_and_title() {
        let app = loaded(vec![
            task(1, "first", TaskKind::Agent),
            task(2, "second", TaskKind::Human),
        ]);
        let rows = drawn(&app, 90, 8);
        assert_eq!(
            inside(&rows[2]),
            "pending 2  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
        );
        assert_eq!(inside(&rows[4]), ">  1  #10  pending  agent  first");
        assert_eq!(inside(&rows[5]), "   2  #20  pending  human  second");
        assert!(!rows.iter().any(|row| row.contains("The queue is empty.")));
    }

    #[test]
    fn the_frame_fills_the_area_and_shows_the_key_to_quit() {
        let rows = drawn(&loaded(vec![]), 60, 8);
        assert!(rows[0].starts_with("┌ ktask-rs ─"));
        assert!(rows[0].ends_with('┐'));
        assert!(rows[7].starts_with("└ q quit · ? keys ─"));
        assert!(rows[7].ends_with('┘'));
    }

    fn keys(app: App, keys: &[ratatui::crossterm::event::KeyCode]) -> App {
        keys.iter()
            .fold(app, |app, key| update(app, Event::Key(*key)))
    }

    #[test]
    fn the_selected_task_is_marked_and_the_mark_follows_the_selection() {
        use ratatui::crossterm::event::KeyCode::Char;
        let app = loaded(vec![
            task(1, "first", TaskKind::Agent),
            task(2, "second", TaskKind::Agent),
        ]);
        let rows = drawn(&keys(app.clone(), &[Char('j')]), 60, 8);
        assert_eq!(inside(&rows[4]), "   1  #10  pending  agent  first");
        assert_eq!(inside(&rows[5]), ">  2  #20  pending  agent  second");
    }

    #[test]
    fn a_cancelled_task_says_so_in_its_status() {
        let mut gone = task(2, "gone", TaskKind::Agent);
        gone.status = TaskStatus::Cancelled;
        let rows = drawn(&loaded(vec![gone]), 60, 8);
        assert_eq!(inside(&rows[4]), ">  2  #20  cancelled  agent  gone");
    }

    #[test]
    fn a_task_with_an_attempt_shows_the_same_line_status_would_for_it() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt("echo", 12, AttemptOutcome::Running, None),
        );
        let app = loaded_with_attempts(vec![task(1, "first", TaskKind::Agent)], attempts);
        let rows = drawn(&app, 60, 8);
        assert_eq!(inside(&rows[4]), ">  1  #10  pending  agent  first");
        assert_eq!(
            inside(&rows[5]),
            "      implementation · echo · 12s · running"
        );
    }

    #[test]
    fn a_done_attempt_carries_no_reason() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt("echo", 3, AttemptOutcome::Reported(Outcome::Done), None),
        );
        let mut task = task(1, "first", TaskKind::Agent);
        task.status = TaskStatus::Done;
        let app = loaded_with_attempts(vec![task], attempts);
        let rows = drawn(&app, 60, 8);
        assert_eq!(inside(&rows[4]), ">  1  #10  done  agent  first");
        assert_eq!(inside(&rows[5]), "      implementation · echo · 3s · done");
    }

    #[test]
    fn each_ending_shows_its_own_outcome_and_reason() {
        let cases = [
            (
                TaskStatus::Failed,
                AttemptOutcome::Reported(Outcome::Failed),
                Some("it broke"),
                "      implementation · echo · 1s · failed: it broke",
            ),
            (
                TaskStatus::Blocked,
                AttemptOutcome::Reported(Outcome::NeedsInput),
                Some("which path?"),
                "      implementation · echo · 1s · needs-input: which path?",
            ),
            (
                TaskStatus::FailedUnknown,
                AttemptOutcome::Unreported,
                Some("reported nothing"),
                "      implementation · echo · 1s · failed-unknown: reported nothing",
            ),
        ];
        for (status, outcome, reason, expected) in cases {
            let mut attempts = HashMap::new();
            attempts.insert(TaskId(10), attempt("echo", 1, outcome, reason));
            let mut task = task(1, "first", TaskKind::Agent);
            task.status = status;
            let app = loaded_with_attempts(vec![task], attempts);
            let rows = drawn(&app, 80, 8);
            assert_eq!(inside(&rows[5]), expected, "{status:?}");
        }
    }

    #[test]
    fn every_step_of_the_attempt_shows_under_the_task_in_order() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt_with_steps(
                "echo",
                &[
                    (
                        ktask_core::SYNC_STEP,
                        2,
                        AttemptOutcome::Passed,
                        Some("2 commits"),
                    ),
                    (
                        IMPLEMENTATION,
                        5,
                        AttemptOutcome::Reported(Outcome::Done),
                        None,
                    ),
                    (
                        ktask_core::REVIEW_STEP,
                        1,
                        AttemptOutcome::Reported(Outcome::Approved),
                        None,
                    ),
                    (
                        ktask_core::COMMIT_STEP,
                        1,
                        AttemptOutcome::Passed,
                        Some("committed as abc123"),
                    ),
                ],
            ),
        );
        let mut done = task(1, "first", TaskKind::Agent);
        done.status = TaskStatus::Done;
        let app = loaded_with_attempts(vec![done], attempts);
        let rows = drawn(&app, 80, 10);
        assert_eq!(inside(&rows[4]), ">  1  #10  done  agent  first");
        // The sync and commit steps are run by the tool itself, not the agent: their lines
        // name no provider.
        assert_eq!(inside(&rows[5]), "      sync · - · 2s · passed: 2 commits");
        assert_eq!(inside(&rows[6]), "      implementation · echo · 5s · done");
        assert_eq!(inside(&rows[7]), "      review · echo · 1s · approved");
        assert_eq!(
            inside(&rows[8]),
            "      commit · - · 1s · passed: committed as abc123"
        );
    }

    #[test]
    fn steps_taller_than_the_list_scroll_behind_an_ellipsis_keeping_the_tail_visible() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt_with_steps(
                "echo",
                &[
                    (ktask_core::SYNC_STEP, 1, AttemptOutcome::Passed, None),
                    (
                        ktask_core::HEALTH_CHECK_STEP,
                        1,
                        AttemptOutcome::Passed,
                        None,
                    ),
                    (
                        IMPLEMENTATION,
                        1,
                        AttemptOutcome::Reported(Outcome::Done),
                        None,
                    ),
                    (
                        ktask_core::REVIEW_STEP,
                        1,
                        AttemptOutcome::Reported(Outcome::Approved),
                        None,
                    ),
                    (ktask_core::TEST_STEP, 3, AttemptOutcome::Running, None),
                ],
            ),
        );
        let mut running = task(1, "first", TaskKind::Agent);
        running.status = TaskStatus::Running;
        let app = loaded_with_attempts(vec![running], attempts);
        // The frame and header take 5 rows; 4 are left for the list — one for the task line,
        // three for its steps: an ellipsis for the ones it has no room for, then the tail —
        // the still-running step and the one finished right before it.
        let rows = drawn(&app, 80, 9);
        assert_eq!(inside(&rows[4]), ">  1  #10  running  agent  first");
        assert_eq!(inside(&rows[5]), "      …");
        assert_eq!(inside(&rows[6]), "      review · echo · 1s · approved");
        assert_eq!(inside(&rows[7]), "      testing · echo · 3s · running");
        let screen = rows.join("\n");
        assert!(!screen.contains("sync"), "{screen}");
        assert!(!screen.contains("health check"), "{screen}");
        assert!(!screen.contains("implementation"), "{screen}");
    }

    #[test]
    fn while_running_earlier_finished_steps_of_the_same_attempt_stay_visible_above_it() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt_with_steps(
                "echo",
                &[
                    (
                        ktask_core::HEALTH_CHECK_STEP,
                        4,
                        AttemptOutcome::Passed,
                        None,
                    ),
                    (IMPLEMENTATION, 9, AttemptOutcome::Running, None),
                ],
            ),
        );
        let mut running = task(1, "first", TaskKind::Agent);
        running.status = TaskStatus::Running;
        let app = loaded_with_attempts(vec![running], attempts);
        let rows = drawn(&app, 80, 10);
        assert_eq!(inside(&rows[4]), ">  1  #10  running  agent  first");
        // The health check is run by the tool itself, not the agent: its line names no
        // provider.
        assert_eq!(inside(&rows[5]), "      health check · - · 4s · passed");
        assert_eq!(
            inside(&rows[6]),
            "      implementation · echo · 9s · running"
        );
    }

    #[test]
    fn a_task_with_an_attempt_adds_a_second_line_to_the_scrolling_blocks() {
        let mut attempts = HashMap::new();
        attempts.insert(
            TaskId(10),
            attempt("echo", 1, AttemptOutcome::Running, None),
        );
        let tasks = vec![
            task(1, "first", TaskKind::Agent),
            task(2, "second", TaskKind::Agent),
            task(3, "third", TaskKind::Agent),
        ];
        // Only five lines are left for the list; the first task's attempt line pushes the
        // third task off the bottom.
        let app = loaded_with_attempts(tasks, attempts);
        let rows = drawn(&app, 60, 9);
        assert_eq!(inside(&rows[4]), ">  1  #10  pending  agent  first");
        assert_eq!(
            inside(&rows[5]),
            "      implementation · echo · 1s · running"
        );
        assert_eq!(inside(&rows[6]), "   2  #20  pending  agent  second");
        assert_eq!(inside(&rows[7]), "   3  #30  pending  agent  third");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_in_view() {
        use ratatui::crossterm::event::KeyCode::Char;
        let tasks = (1..=10)
            .map(|n| task(n, &format!("t{n}"), TaskKind::Agent))
            .collect();
        // Five lines are left for the list under the frame and the header.
        let rows = drawn(&keys(loaded(tasks), &[Char('G')]), 60, 10);
        assert_eq!(inside(&rows[4]), "   6  #60  pending  agent  t6");
        assert_eq!(inside(&rows[8]), "> 10  #100  pending  agent  t10");
        let rows = drawn(&keys(loaded_ten(), &[Char('G'), Char('g')]), 60, 10);
        assert_eq!(inside(&rows[4]), ">  1  #10  pending  agent  t1");
    }

    fn loaded_ten() -> App {
        loaded(
            (1..=10)
                .map(|n| task(n, &format!("t{n}"), TaskKind::Agent))
                .collect(),
        )
    }

    #[test]
    fn the_key_map_lists_every_key_of_the_screen_instead_of_the_queue() {
        use ratatui::crossterm::event::KeyCode::Char;
        let app = keys(
            loaded(vec![task(1, "Write parser", TaskKind::Agent)]),
            &[Char('?')],
        );
        let rows = drawn(&app, 60, 19);
        let screen = rows.join("\n");
        for key in [
            "j, Down", "k, Up", "g ", "G ", "a ", "d ", "r ", "s ", "y ", "n ", "? ", "Esc", "q ",
        ] {
            assert!(screen.contains(key), "{key:?} in\n{screen}");
        }
        assert!(!screen.contains("Write parser"), "{screen}");
    }

    #[test]
    fn asking_to_remove_a_task_names_it_and_the_keys_that_answer() {
        use ratatui::crossterm::event::KeyCode::Char;
        let app = keys(
            loaded(vec![
                task(1, "first", TaskKind::Agent),
                task(2, "second", TaskKind::Agent),
            ]),
            &[Char('j'), Char('d')],
        );
        let rows = drawn(&app, 60, 8);
        assert_eq!(
            inside(&rows[3]),
            "Remove #20 second? y to remove · n or Esc to keep"
        );
        assert_eq!(inside(&rows[5]), ">  2  #20  pending  agent  second");
        let rows = drawn(&keys(app, &[Char('n')]), 60, 8);
        assert_eq!(inside(&rows[3]), "");
    }

    #[test]
    fn a_run_message_shows_in_place_of_the_task_list_one_line_per_line_it_printed() {
        let text = "task 1: done\ntask 2: failed: it broke\nnothing else is pending";
        let app = update(
            loaded(vec![task(1, "first", TaskKind::Agent)]),
            Event::RunMessage(text.to_owned()),
        );
        // The question line stays empty — the message is not squeezed into it.
        let rows = drawn(&app, 60, 8);
        assert_eq!(inside(&rows[3]), "");
        assert_eq!(inside(&rows[4]), "task 1: done");
        assert_eq!(inside(&rows[5]), "task 2: failed: it broke");
        assert_eq!(inside(&rows[6]), "nothing else is pending");
    }

    #[test]
    fn more_run_message_lines_than_fit_are_reached_by_scrolling() {
        use ratatui::crossterm::event::KeyCode::Char;
        let text = (1..=5)
            .map(|n| format!("task {n}: done"))
            .collect::<Vec<_>>()
            .join("\n");
        let app = update(
            loaded(vec![task(1, "first", TaskKind::Agent)]),
            Event::RunMessage(text),
        );
        // Three lines fit under the header on an 8-row screen.
        let rows = drawn(&app, 60, 8);
        assert_eq!(inside(&rows[4]), "task 1: done");
        assert_eq!(inside(&rows[5]), "task 2: done");
        assert_eq!(inside(&rows[6]), "task 3: done");

        let scrolled = keys(app, &[Char('G')]);
        let rows = drawn(&scrolled, 60, 8);
        assert_eq!(inside(&rows[4]), "task 3: done");
        assert_eq!(inside(&rows[5]), "task 4: done");
        assert_eq!(inside(&rows[6]), "task 5: done");
    }

    #[test]
    fn d_on_a_running_task_shows_the_refusal_in_place_of_the_question() {
        use ratatui::crossterm::event::KeyCode::Char;
        let mut running = task(1, "first", TaskKind::Agent);
        running.status = TaskStatus::Running;
        let app = keys(loaded(vec![running]), &[Char('d')]);
        let rows = drawn(&app, 60, 8);
        assert_eq!(
            inside(&rows[3]),
            CancelError::Running(TaskId(10)).to_string()
        );
    }

    fn draw_form(app: &App, height: u16) -> (Vec<String>, Option<Position>) {
        let area = Rect::new(0, 0, 60, height);
        let mut buf = Buffer::empty(area);
        let cursor = render(app, area, &mut buf);
        (drawn(app, 60, height), cursor)
    }

    #[test]
    fn the_form_covers_the_queue_with_its_fields_and_puts_the_cursor_in_the_title() {
        use ratatui::crossterm::event::KeyCode::Char;
        let app = keys(
            loaded(vec![task(1, "first", TaskKind::Agent)]),
            &[Char('n')],
        );
        let (rows, cursor) = draw_form(&app, 14);
        assert_eq!(inside(&rows[1]), "New task");
        assert_eq!(inside(&rows[3]), "> Title:");
        assert_eq!(inside(&rows[4]), "  Kind:      < agent >");
        assert_eq!(inside(&rows[5]), "  Links:");
        assert_eq!(inside(&rows[6]), "  Body:");
        assert_eq!(inside(&rows[8]), "  Criteria:");
        assert_eq!(inside(&rows[9]), "   1.");
        assert!(
            rows[13].starts_with("└ Ctrl-S add · Esc cancel"),
            "{rows:?}"
        );
        assert!(!rows.join("\n").contains("first"));
        assert_eq!(cursor, Some(Position::new(14, 3)));
    }

    #[test]
    fn a_long_title_scrolls_sideways_so_that_the_cursor_stays_on_the_screen() {
        use ratatui::crossterm::event::KeyCode::Char;
        let mut app = keys(loaded(vec![]), &[Char('n')]);
        app = keys(app, &"x".repeat(70).chars().map(Char).collect::<Vec<_>>());
        let (rows, cursor) = draw_form(&app, 12);
        let cursor = cursor.expect("the cursor is shown");
        assert!(cursor.x < 59, "{cursor:?}");
        assert!(inside(&rows[3]).ends_with('x'), "{rows:?}");
    }

    #[test]
    fn ctrl_c_in_a_form_with_content_shows_the_discard_question_and_the_footer_that_answers_it() {
        use ratatui::crossterm::event::KeyCode::Char;
        let app = keys(
            loaded(vec![task(1, "first", TaskKind::Agent)]),
            &[Char('n')],
        );
        let app = keys(app, &"Title".chars().map(Char).collect::<Vec<_>>());
        let app = update(app, Event::Ctrl('c'));
        let (rows, _) = draw_form(&app, 14);
        assert_eq!(inside(&rows[1]), "New task");
        assert_eq!(
            inside(&rows[2]),
            "Discard this task? y to discard · n or Esc to keep writing"
        );
        assert!(
            rows[13].starts_with("└ y discard · n, Esc keep writing"),
            "{rows:?}"
        );
        assert!(inside(&rows[4]).ends_with("Title"), "{rows:?}");
    }

    /// The two settings, as `show_settings` would give them: attempt-timeout at `value`,
    /// health-check unset.
    fn settings_views(value: &str, is_default: bool) -> Vec<SettingView> {
        vec![
            SettingView {
                name: "attempt-timeout",
                value: value.to_owned(),
                is_default,
            },
            SettingView {
                name: "health-check",
                value: String::new(),
                is_default: true,
            },
        ]
    }

    #[test]
    fn the_settings_screen_shows_the_value_default_tag_and_footer() {
        let app = update(
            loaded(vec![task(1, "first", TaskKind::Agent)]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let (rows, cursor) = draw_form(&app, 11);
        assert_eq!(inside(&rows[1]), "Settings");
        assert_eq!(inside(&rows[3]), "Attempt timeout, in seconds (default):");
        assert_eq!(inside(&rows[4]), "> 14400");
        assert_eq!(inside(&rows[6]), "Health check command (default):");
        assert_eq!(inside(&rows[7]), "");
        assert!(
            rows[10].starts_with("└ Tab, Shift-Tab field · Ctrl-S save · Esc cancel"),
            "{rows:?}"
        );
        assert!(!rows.join("\n").contains("first"));
        assert_eq!(cursor, Some(Position::new(8, 4)));
    }

    #[test]
    fn tab_moves_the_cursor_to_the_next_fields_value() {
        let app = update(
            loaded(vec![]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = update(app, Event::Key(ratatui::crossterm::event::KeyCode::Tab));
        let (rows, cursor) = draw_form(&app, 11);
        assert_eq!(inside(&rows[7]), ">");
        assert_eq!(cursor, Some(Position::new(3, 7)));
    }

    #[test]
    fn a_refused_setting_shows_the_reason_above_the_field() {
        let app = update(
            loaded(vec![]),
            Event::SettingsLoaded(settings_views("60", false)),
        );
        let app = update(app, Event::SettingRejected("not a number".to_owned()));
        let (rows, _) = draw_form(&app, 8);
        assert_eq!(inside(&rows[2]), "! not a number");
        assert_eq!(inside(&rows[4]), "Attempt timeout, in seconds (custom):");
        assert_eq!(inside(&rows[5]), "> 60");
    }

    #[test]
    fn before_the_queue_is_loaded_the_screen_says_so() {
        let rows = drawn(&App::default(), 40, 5);
        assert_eq!(inside(&rows[1]), "Loading the queue…");
    }
}
