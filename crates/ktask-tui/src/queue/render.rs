//! Drawing the queue screen: the header, the question or notice line, the key map, the task
//! list, and, above it when there is one, the last run's or import's own report.

use crate::presentation;
use ktask_core::{AttemptLine, DoneMark, QueueView, TaskId};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::scroll::first_shown;
use crate::widgets::{elide, key_map_entries};

use super::Queue;

mod lines;
mod row;

use lines::{step_lines_named, windowed};
use row::{Columns, task_line};

/// Every key the queue screen itself answers, and what it does. Keys that only work while a
/// question, a form or another screen is up are that context's own — shown there, in its own
/// question line or footer — and left out of this key map, so no key map here shows a key
/// that does not work in the context it is shown in, and no key is listed twice.
const KEYS: [(&str, &str); 22] = [
    ("j, Down", "select the next task"),
    ("k, Up", "select the previous task"),
    ("g", "select the first task"),
    ("G", "select the last task"),
    ("a", "show or hide cancelled, skipped and superseded tasks"),
    ("n", "add a task at the end, written in a form"),
    ("o", "add a task below the selected one, written in a form"),
    ("O", "add a task above the selected one, written in a form"),
    ("d", "remove the selected task, after asking"),
    (
        "t",
        "retry the selected task, once it is failed, failed-unknown or blocked",
    ),
    (
        "A",
        "answer the selected task's question, once it is blocked",
    ),
    ("D", "mark the selected task done by hand"),
    ("H", "acknowledge the selected human task"),
    ("l", "show the selected task's output"),
    (
        "r",
        "start executing the queue, exactly as `ktask-rs run` does",
    ),
    (
        "i",
        "import the tasks of a JSON file, asked for by its path",
    ),
    ("s", "open the project's settings"),
    ("v", "open the project's providers"),
    ("p", "work on another registered project's queue"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map"),
    ("q", "quit"),
];

/// The keys shown by `?` while the removal question is asking — the question itself already
/// shows them inline, but a long enough title still leaves `?` as the one way to see them
/// without cutting anything.
const REMOVAL_QUESTION_KEYS: [(&str, &str); 2] = [("y", "remove the task"), ("n, Esc", "keep it")];

impl Queue {
    /// Draws the queue screen over the whole of `area`.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let Some(view) = &self.view else {
            Paragraph::new("Loading the queue…").render(area, buf);
            return;
        };
        if self.help {
            key_map_entries(self.help_keys(), area, buf);
            return;
        }
        let [header, notice, list] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(self.message_height(area.height)),
            Constraint::Min(0),
        ])
        .areas(area);
        Paragraph::new(self.header_lines(view, usize::from(header.width))).render(header, buf);
        if let Some(message) = &self.message {
            let height = usize::from(notice.height);
            Paragraph::new(message_lines(self.message_offset, message, height)).render(notice, buf);
        }
        let height = usize::from(list.height);
        let width = usize::from(list.width);
        Paragraph::new(task_lines(self.selected, view, height, width)).render(list, buf);
    }

    /// How many of `total` rows go to the last run's or import's own report, above the task
    /// list: as many as it has, so a short one wastes nothing, but never more than half of
    /// what is left after the header, so the list it is about — and the selection on it — is
    /// never pushed off screen by it, however long the report runs.
    fn message_height(&self, total: u16) -> u16 {
        let Some(message) = &self.message else {
            return 0;
        };
        let cap = (total.saturating_sub(3) / 2).max(1);
        u16::try_from(message.len()).unwrap_or(u16::MAX).min(cap)
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
            Line::from(
                format!(
                    "pending {} running {} done {} failed {} blocked {} unknown {} cancelled {} \
                 skipped {} superseded {}",
                    summary.pending,
                    summary.running,
                    summary.done,
                    summary.failed,
                    summary.blocked,
                    summary.failed_unknown,
                    summary.cancelled,
                    summary.skipped,
                    summary.superseded
                ) + &format!(" · {}", presentation::usage_text(queue_usage(view))),
            ),
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

fn queue_usage(view: &QueueView) -> ktask_core::Usage {
    view.attempts
        .values()
        .fold(ktask_core::Usage::default(), |total, attempt| {
            total.plus(attempt.usage)
        })
        .plus(
            view.history
                .values()
                .flatten()
                .fold(ktask_core::Usage::default(), |total, attempt| {
                    total.plus(attempt.usage)
                }),
        )
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
/// with `>` and shown reversed; a cancelled, skipped or superseded one is dimmed and says so
/// in its status.
/// A task
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
        let steps = task_step_lines(view, task.id, attempt, width);
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

/// How many lines each task in `view.tasks` takes: one for the task itself, plus one when it
/// was marked done by hand, plus one per step of every earlier attempt it has, oldest first,
/// plus one per step its current attempt, if any, has run so far.
fn block_heights(view: &QueueView) -> Vec<usize> {
    view.tasks
        .iter()
        .map(|task| {
            let current = view
                .attempts
                .get(&task.id)
                .map_or(0, |attempt| attempt.steps.len());
            let history: usize = view.history.get(&task.id).map_or(0, |attempts| {
                attempts.iter().map(|attempt| attempt.steps.len()).sum()
            });
            let done_mark = usize::from(view.done_by_user.contains_key(&task.id));
            1 + done_mark + history + current
        })
        .collect()
}

/// The reason and when task `id` was marked done by hand, as one dimmed line — shown ahead of
/// its attempt's own steps, since it is not one of them.
fn done_mark_line(mark: &DoneMark, width: usize) -> Line<'static> {
    let prefix = format!("      {}", presentation::done_mark_prefix());
    let suffix = presentation::done_mark_suffix(mark);
    let budget = width.saturating_sub(prefix.chars().count() + suffix.chars().count());
    Line::styled(
        format!("{prefix}{}{suffix}", elide(&mark.reason, budget)),
        Style::new().add_modifier(Modifier::DIM),
    )
}

/// `attempt {number}: `, ahead of every step line of a real attempt, so each says which
/// attempt it belongs to, the current attempt included — `""` for the synthetic attempt
/// number `0` a gate stop before any attempt ever began carries, which belongs to none.
/// Every step line task `id` shows: the reason and when it was marked done by hand, when it
/// was, then one earlier attempt's own steps after another, oldest first, each step named with
/// its attempt's number ahead of it, then `current`, the current attempt's steps, last, named
/// with its own number too, the same way — "the next run adds attempt N+1 under" the ones
/// already there.
fn task_step_lines(
    view: &QueueView,
    id: TaskId,
    current: Option<&AttemptLine>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(mark) = view.done_by_user.get(&id) {
        lines.push(done_mark_line(mark, width));
    }
    if let Some(history) = view.history.get(&id) {
        for attempt in history {
            lines.extend(step_lines_named(
                &attempt.steps,
                width,
                &presentation::attempt_label(attempt.number),
                None,
            ));
        }
    }
    if let Some(attempt) = current {
        lines.extend(step_lines_named(
            &attempt.steps,
            width,
            &presentation::attempt_label(attempt.number),
            attempt.output_activity.as_ref(),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use ktask_core::{
        AttemptOutcome, IMPLEMENTATION, Outcome, Project, StatusSummary, StepLine, Task, TaskId,
        TaskKind, TaskStatus,
    };
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::KeyCode;
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
            provider: None,
            model: None,
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
            history: HashMap::new(),
            done_by_user: HashMap::new(),
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
            provider: None,
            model: None,
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
            "pending 0 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0 \
             superseded 0"
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
        let rows = drawn(&queue, 60, 22);
        let screen = rows.join("\n");
        for key in [
            "j, Down", "k, Up", "g ", "G ", "a ", "d ", "t ", "A ", "D ", "r ", "i ", "s ", "? ",
            "Esc", "q ",
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
    fn a_run_message_shows_above_the_task_list_which_stays_on_show_beneath_it() {
        let text = "task 1: done\ntask 2: failed: it broke\nnothing else is pending";
        let queue = loaded(&[1]).run_message(text.to_owned());
        let rows = drawn(&queue, 60, 8);
        assert_eq!(row(&rows, 3), "task 1: done");
        assert_eq!(row(&rows, 4), "task 2: failed: it broke");
        assert_eq!(row(&rows, 5), ">0  #1  pending  agent  task 1");
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
            model: None,
            session: None,
            time_spent: Duration::from_secs(seconds),
            outcome,
            reason: None,
            waiting: None,
            limit_wait: None,
            limit_warning: None,
            output_activity: None,
            usage: ktask_core::Usage::default(),
            steps: vec![StepLine {
                step: IMPLEMENTATION.to_owned(),
                provider: Some(provider.to_owned()),
                model: None,
                session: None,
                time_spent: Duration::from_secs(seconds),
                outcome,
                reason: None,
                waiting: None,
                limit_wait: None,
                limit_warning: None,
                usage: ktask_core::Usage::default(),
            }],
        }
    }

    #[test]
    fn a_task_with_an_attempt_shows_the_same_line_status_would_for_it() {
        let mut attempts = HashMap::new();
        attempts.insert(TaskId(10), attempt("echo", 12, AttemptOutcome::Running));
        let queue = loaded_with_attempts(vec![task_named(1, "first", TaskKind::Agent)], attempts);
        let rows = drawn(&queue, 60, 8);
        assert_eq!(
            row(&rows, 4),
            "      attempt 1: implementation · echo · 12s · running · us…"
        );
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
        assert_eq!(
            row(&rows, 4),
            "      attempt 1: implementation · echo · 3s · done · usage …"
        );
    }

    #[test]
    fn a_task_marked_done_by_the_user_shows_the_reason_and_when_above_its_attempts_own_steps() {
        let task = {
            let mut task = task_named(1, "first", TaskKind::Agent);
            task.status = TaskStatus::Done;
            task
        };
        let mut attempts = HashMap::new();
        attempts.insert(
            task.id,
            attempt("echo", 3, AttemptOutcome::Reported(Outcome::Failed)),
        );
        let mut done_by_user = HashMap::new();
        done_by_user.insert(
            task.id,
            DoneMark {
                reason: "fixed by hand".to_owned(),
                at: SystemTime::UNIX_EPOCH + Duration::from_mins(15),
            },
        );
        let project = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        };
        let queue = Queue::default().loaded(QueueView {
            project,
            summary: StatusSummary {
                done: 1,
                ..StatusSummary::default()
            },
            tasks: vec![task],
            attempts,
            history: HashMap::new(),
            done_by_user,
        });
        let rows = drawn(&queue, 100, 8);
        assert_eq!(row(&rows, 3), ">1  #10  done  agent  first");
        let line = row(&rows, 4);
        assert!(
            line.contains("marked done by the user: fixed by hand (at"),
            "{line}"
        );
        assert_eq!(
            row(&rows, 5),
            "      attempt 1: implementation · echo · 3s · failed · usage none"
        );
    }

    #[test]
    fn a_retried_tasks_earlier_attempt_shows_above_its_current_one_named_with_its_number() {
        let task = task_named(1, "first", TaskKind::Agent);
        let mut attempts = HashMap::new();
        attempts.insert(
            task.id,
            AttemptLine {
                number: 2,
                ..attempt("echo", 5, AttemptOutcome::Running)
            },
        );
        let mut history = HashMap::new();
        history.insert(
            task.id,
            vec![attempt(
                "echo",
                9,
                AttemptOutcome::Reported(Outcome::Failed),
            )],
        );
        let project = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        };
        let queue = Queue::default().loaded(QueueView {
            project,
            summary: StatusSummary {
                running: 1,
                ..StatusSummary::default()
            },
            tasks: vec![task],
            attempts,
            history,
            done_by_user: HashMap::new(),
        });
        let rows = drawn(&queue, 60, 8);
        assert_eq!(
            row(&rows, 4),
            "      attempt 1: implementation · echo · 9s · failed · usag…"
        );
        assert_eq!(
            row(&rows, 5),
            "      attempt 2: implementation · echo · 5s · running · usa…"
        );
    }
}
