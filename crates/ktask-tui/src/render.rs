//! Draws an [`App`] into a buffer.

use ktask_core::{QueueView, Task, TaskStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Widget};

use crate::App;

/// Every key the queue screen answers, and what it does.
const KEYS: [(&str, &str); 11] = [
    ("j, Down", "select the next task"),
    ("k, Up", "select the previous task"),
    ("g", "select the first task"),
    ("G", "select the last task"),
    ("a", "show or hide cancelled tasks"),
    ("d", "remove the selected task, after asking"),
    ("y", "answer yes when asked to remove a task"),
    ("n", "answer no when asked to remove a task"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map, or answer no"),
    ("q", "quit"),
];

/// Draws `app` over the whole of `area`.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let block = Block::bordered()
        .title(" ktask-rs ")
        .title_bottom(" q quit · ? keys ");
    let inner = block.inner(area);
    block.render(area, buf);
    match &app.queue {
        None => Paragraph::new("Loading the queue…").render(inner, buf),
        Some(_) if app.help => key_map(inner, buf),
        Some(queue) => {
            let [header, list] =
                Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(inner);
            Paragraph::new(header_lines(app, queue)).render(header, buf);
            Paragraph::new(task_lines(app, queue, usize::from(list.height))).render(list, buf);
        }
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

/// The header: the project, the counts, and the question of a removal while there is one.
fn header_lines(app: &App, queue: &QueueView) -> Vec<Line<'static>> {
    let summary = queue.summary;
    let question = app
        .confirming
        .and_then(|id| queue.tasks.iter().find(|task| task.id == id))
        .map_or_else(Line::default, |task| {
            Line::styled(
                format!(
                    "Remove #{} {}? y to remove · n or Esc to keep",
                    task.id, task.title
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )
        });
    vec![
        Line::styled(
            queue.project.name.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::from(format!(
            "pending {} · running {} · done {} · failed {} · cancelled {}",
            summary.pending, summary.running, summary.done, summary.failed, summary.cancelled
        )),
        question,
    ]
}

/// The rows of the task list that fit in `height` lines, scrolled so that the selected task
/// is the last one in view when it would not be otherwise. The selected task is marked with
/// `>` and shown reversed; a cancelled one is dimmed and says so in its status.
fn task_lines(app: &App, queue: &QueueView, height: usize) -> Vec<Line<'static>> {
    if queue.tasks.is_empty() {
        return vec![Line::from("The queue is empty.")];
    }
    let selected = queue
        .tasks
        .iter()
        .position(|task| Some(task.id) == app.selected);
    let first = selected.map_or(0, |index| (index + 1).saturating_sub(height));
    queue
        .tasks
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, task)| task_line(task, Some(index) == selected))
        .collect()
}

fn task_line(task: &Task, selected: bool) -> Line<'static> {
    let marker = if selected { '>' } else { ' ' };
    let mut style = Style::new();
    if task.status == TaskStatus::Cancelled {
        style = style.add_modifier(Modifier::DIM);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::styled(
        format!(
            "{marker}{:>3}  #{}  {}  {}  {}",
            task.position, task.id, task.status, task.kind, task.title
        ),
        style,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{Project, QueueView, StatusSummary, Task, TaskId, TaskKind, TaskStatus};

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
            }),
        )
    }

    #[test]
    fn an_empty_queue_shows_the_project_the_counts_and_a_message() {
        let rows = drawn(&loaded(vec![]), 60, 8);
        assert_eq!(inside(&rows[1]), "app");
        assert_eq!(
            inside(&rows[2]),
            "pending 0 · running 0 · done 0 · failed 0 · cancelled 0"
        );
        assert_eq!(inside(&rows[4]), "The queue is empty.");
    }

    #[test]
    fn tasks_are_listed_in_order_with_position_id_status_kind_and_title() {
        let app = loaded(vec![
            task(1, "first", TaskKind::Agent),
            task(2, "second", TaskKind::Human),
        ]);
        let rows = drawn(&app, 60, 8);
        assert_eq!(
            inside(&rows[2]),
            "pending 2 · running 0 · done 0 · failed 0 · cancelled 0"
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
        let rows = drawn(&app, 60, 17);
        let screen = rows.join("\n");
        for key in [
            "j, Down", "k, Up", "g ", "G ", "a ", "d ", "y ", "n ", "? ", "Esc", "q ",
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
    fn before_the_queue_is_loaded_the_screen_says_so() {
        let rows = drawn(&App::default(), 40, 5);
        assert_eq!(inside(&rows[1]), "Loading the queue…");
    }
}
