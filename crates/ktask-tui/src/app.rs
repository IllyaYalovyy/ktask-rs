//! The state of the terminal interface and how events change it.

use ktask_core::{QueueView, TaskId};
use ratatui::crossterm::event::KeyCode;

/// Everything the screen shows and remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    /// The queue on show; `None` until it has been loaded.
    pub queue: Option<QueueView>,
    /// The task the selection is on; `None` while the queue shows none.
    pub selected: Option<TaskId>,
    /// Whether cancelled tasks are asked for: the queue is loaded with them, in their places.
    pub show_cancelled: bool,
    /// Whether the key map covers the queue.
    pub help: bool,
    /// Set when the operator asked to leave.
    pub quit: bool,
}

/// Something that happened: the only way an [`App`] changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The queue was loaded, as many times as it changes or is asked for again.
    Loaded(QueueView),
    /// A key was pressed.
    Key(KeyCode),
    /// The terminal changed size; the screen is drawn again at the new size.
    Resize,
}

/// The app after `event` happened to `app`.
#[must_use]
pub fn update(app: App, event: Event) -> App {
    match event {
        Event::Loaded(queue) => {
            let selected = reselect(&app, &queue);
            App {
                queue: Some(queue),
                selected,
                ..app
            }
        }
        Event::Key(KeyCode::Char('q')) => App { quit: true, ..app },
        Event::Key(key) if app.help => match key {
            KeyCode::Esc | KeyCode::Char('?') => App { help: false, ..app },
            _ => app,
        },
        Event::Key(key) => match key {
            KeyCode::Char('?') => App { help: true, ..app },
            KeyCode::Char('a') => App {
                show_cancelled: !app.show_cancelled,
                ..app
            },
            KeyCode::Char('j') | KeyCode::Down => select(app, |index, _| index.saturating_add(1)),
            KeyCode::Char('k') | KeyCode::Up => select(app, |index, _| index.saturating_sub(1)),
            KeyCode::Char('g') => select(app, |_, _| 0),
            KeyCode::Char('G') => select(app, |_, len| len.saturating_sub(1)),
            _ => app,
        },
        Event::Resize => app,
    }
}

/// The task to keep selected once `queue` replaces the one on show: the same task if it is
/// still there, otherwise the one that took its place in the list, or the last.
fn reselect(app: &App, queue: &QueueView) -> Option<TaskId> {
    let index = match (&app.queue, app.selected) {
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

/// The app with the selection moved to the index `target` picks, given the index it is at
/// and how many tasks there are. It stays inside the list.
fn select(app: App, target: impl FnOnce(usize, usize) -> usize) -> App {
    let Some(queue) = &app.queue else {
        return app;
    };
    let Some(last) = queue.tasks.len().checked_sub(1) else {
        return app;
    };
    let index = queue
        .tasks
        .iter()
        .position(|task| Some(task.id) == app.selected)
        .unwrap_or(0);
    let selected = queue
        .tasks
        .get(target(index, queue.tasks.len()).min(last))
        .map(|task| task.id);
    App { selected, ..app }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{Project, StatusSummary, Task, TaskKind, TaskStatus};

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
        }
    }

    fn loaded(ids: &[u64]) -> App {
        update(App::default(), Event::Loaded(queue_of(ids)))
    }

    fn press(app: App, keys: &[KeyCode]) -> App {
        keys.iter()
            .fold(app, |app, key| update(app, Event::Key(*key)))
    }

    fn on(app: &App) -> Option<u64> {
        app.selected.map(|id| id.0)
    }

    #[test]
    fn a_loaded_queue_is_shown_with_its_first_task_selected() {
        let app = loaded(&[3, 5, 8]);
        assert_eq!(app.queue, Some(queue_of(&[3, 5, 8])));
        assert_eq!(on(&app), Some(3));
        assert!(!app.quit && !app.help && !app.show_cancelled);
    }

    #[test]
    fn an_empty_queue_selects_nothing_and_keys_move_nothing() {
        let app = loaded(&[]);
        assert_eq!(on(&app), None);
        let app = press(app, &[KeyCode::Char('j'), KeyCode::Char('G')]);
        assert_eq!(on(&app), None);
    }

    #[test]
    fn q_quits_and_keeps_the_queue() {
        let app = press(loaded(&[1]), &[KeyCode::Char('q')]);
        assert!(app.quit);
        assert_eq!(app.queue, Some(queue_of(&[1])));
    }

    #[test]
    fn j_and_down_select_the_next_task_and_stop_at_the_last() {
        for key in [KeyCode::Char('j'), KeyCode::Down] {
            let app = loaded(&[1, 2, 3]);
            let app = press(app, &[key]);
            assert_eq!(on(&app), Some(2));
            let app = press(app, &[key, key, key]);
            assert_eq!(on(&app), Some(3));
        }
    }

    #[test]
    fn k_and_up_select_the_previous_task_and_stop_at_the_first() {
        for key in [KeyCode::Char('k'), KeyCode::Up] {
            let app = press(loaded(&[1, 2, 3]), &[KeyCode::Char('G'), key]);
            assert_eq!(on(&app), Some(2));
            let app = press(app, &[key, key, key]);
            assert_eq!(on(&app), Some(1));
        }
    }

    #[test]
    fn g_selects_the_first_task_and_capital_g_the_last() {
        let app = press(loaded(&[1, 2, 3]), &[KeyCode::Char('G')]);
        assert_eq!(on(&app), Some(3));
        let app = press(app, &[KeyCode::Char('g')]);
        assert_eq!(on(&app), Some(1));
    }

    #[test]
    fn a_reload_keeps_the_selection_on_the_same_task_when_others_come_before_it() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('j')]);
        let app = update(app, Event::Loaded(queue_of(&[7, 1, 2, 9])));
        assert_eq!(on(&app), Some(2));
    }

    #[test]
    fn a_reload_without_the_selected_task_selects_the_one_in_its_place_or_the_last() {
        let app = press(loaded(&[1, 2, 3]), &[KeyCode::Char('j')]);
        let moved = update(app.clone(), Event::Loaded(queue_of(&[1, 3])));
        assert_eq!(on(&moved), Some(3));
        let app = press(app, &[KeyCode::Char('G')]);
        let shorter = update(app, Event::Loaded(queue_of(&[1, 2])));
        assert_eq!(on(&shorter), Some(2));
        let emptied = update(shorter, Event::Loaded(queue_of(&[])));
        assert_eq!(on(&emptied), None);
        let refilled = update(emptied, Event::Loaded(queue_of(&[4, 5])));
        assert_eq!(on(&refilled), Some(4));
    }

    #[test]
    fn a_toggles_asking_for_cancelled_tasks() {
        let app = press(loaded(&[1]), &[KeyCode::Char('a')]);
        assert!(app.show_cancelled);
        let app = press(app, &[KeyCode::Char('a')]);
        assert!(!app.show_cancelled);
    }

    #[test]
    fn question_mark_opens_the_key_map_and_esc_or_question_mark_closes_it() {
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let app = press(loaded(&[1]), &[KeyCode::Char('?')]);
            assert!(app.help);
            assert!(!press(app, &[close]).help);
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
        assert!(press(open, &[KeyCode::Char('q')]).quit);
    }

    #[test]
    fn other_keys_and_resizes_change_nothing() {
        let app = loaded(&[1, 2]);
        for event in [
            Event::Key(KeyCode::Char('x')),
            Event::Key(KeyCode::Esc),
            Event::Resize,
        ] {
            assert_eq!(update(app.clone(), event), app);
        }
    }
}
