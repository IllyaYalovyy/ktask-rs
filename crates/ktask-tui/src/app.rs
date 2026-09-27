//! The state of the terminal interface and how events change it.

use ktask_core::{QueueView, TaskDraft, TaskId, TaskStatus};
use ratatui::crossterm::event::KeyCode;

use crate::form::Form;

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
    /// The task the operator is being asked to confirm removing; the answer is the next key.
    pub confirming: Option<TaskId>,
    /// The task whose removal was confirmed: the loop carries it out and clears this.
    pub removal: Option<TaskId>,
    /// The form a new task is written in, while it is open; it covers the queue.
    pub(crate) form: Option<Form>,
    /// The task the form was submitted with: the loop adds it and answers with
    /// [`Event::Added`] or [`Event::Rejected`], and clears this.
    pub submission: Option<TaskDraft>,
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
    /// A letter was pressed with Ctrl held.
    Ctrl(char),
    /// The task the form was submitted with was added: the form closes.
    Added,
    /// The task the form was submitted with was not added, for these reasons: the form stays
    /// open and shows them.
    Rejected(Vec<String>),
    /// The terminal changed size; the screen is drawn again at the new size.
    Resize,
}

/// The app after `event` happened to `app`.
#[must_use]
pub fn update(app: App, event: Event) -> App {
    match event {
        Event::Loaded(queue) => {
            let selected = reselect(&app, &queue);
            let confirming = app.confirming.filter(|id| removable(&queue, *id));
            App {
                queue: Some(queue),
                selected,
                confirming,
                ..app
            }
        }
        Event::Key(key) if app.form.is_some() => match key {
            KeyCode::Esc => App { form: None, ..app },
            KeyCode::Tab => in_form(app, |form| form.moved(true)),
            KeyCode::BackTab => in_form(app, |form| form.moved(false)),
            _ => in_form(app, |form| form.press(key)),
        },
        Event::Ctrl(letter) if app.form.is_some() => match letter {
            's' => submit(app),
            'n' => in_form(app, Form::with_criterion),
            'd' => in_form(app, Form::without_criterion),
            _ => app,
        },
        Event::Added => App { form: None, ..app },
        Event::Rejected(problems) => in_form(app, |form| Form { problems, ..form }),
        Event::Key(KeyCode::Char('q')) => App { quit: true, ..app },
        Event::Key(key) if app.confirming.is_some() => match key {
            KeyCode::Char('y') => confirm_removal(app),
            KeyCode::Char('n') | KeyCode::Esc => App {
                confirming: None,
                ..app
            },
            _ => app,
        },
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
            KeyCode::Char('n') => App {
                form: Some(Form::new()),
                ..app
            },
            KeyCode::Char('d') => App {
                confirming: app.selected.filter(|id| {
                    app.queue
                        .as_ref()
                        .is_some_and(|queue| removable(queue, *id))
                }),
                ..app
            },
            KeyCode::Char('g') => select(app, |_, _| 0),
            KeyCode::Char('G') => select(app, |_, len| len.saturating_sub(1)),
            _ => app,
        },
        Event::Ctrl(_) | Event::Resize => app,
    }
}

/// The app with `change` made to the form, if one is open.
fn in_form(app: App, change: impl FnOnce(Form) -> Form) -> App {
    App {
        form: app.form.map(change),
        ..app
    }
}

/// The app with the form's task left for the loop to add, if a form is open.
fn submit(app: App) -> App {
    App {
        submission: app.form.as_ref().map(Form::draft),
        ..app
    }
}

/// Whether `queue` shows the task `id` and it can still be removed: a cancelled one cannot.
fn removable(queue: &QueueView, id: TaskId) -> bool {
    queue
        .tasks
        .iter()
        .any(|task| task.id == id && task.status != TaskStatus::Cancelled)
}

/// The app once the removal it asks about is confirmed: the removal is left for the loop to
/// carry out, and the selection moves to the task after the one removed, or the one before
/// it when it was the last, so that it is already there when the queue is loaded again.
fn confirm_removal(app: App) -> App {
    let Some(id) = app.confirming else {
        return app;
    };
    let neighbour = app.queue.as_ref().and_then(|queue| {
        let index = queue.tasks.iter().position(|task| task.id == id)?;
        let next = queue.tasks.get(index + 1);
        next.or_else(|| {
            index
                .checked_sub(1)
                .and_then(|before| queue.tasks.get(before))
        })
        .map(|task| task.id)
    });
    App {
        confirming: None,
        removal: Some(id),
        selected: neighbour.or(app.selected),
        ..app
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
    fn d_asks_about_removing_the_selected_task_and_changes_nothing_else() {
        let app = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('j'), KeyCode::Char('d')],
        );
        assert_eq!(app.confirming, Some(TaskId(2)));
        assert_eq!(app.removal, None);
        assert_eq!(on(&app), Some(2));
        assert_eq!(app.queue, Some(queue_of(&[1, 2, 3])));
    }

    #[test]
    fn d_with_nothing_selected_or_a_cancelled_task_selected_asks_nothing() {
        assert_eq!(press(loaded(&[]), &[KeyCode::Char('d')]).confirming, None);
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Cancelled;
        let app = update(App::default(), Event::Loaded(queue));
        assert_eq!(press(app, &[KeyCode::Char('d')]).confirming, None);
    }

    #[test]
    fn y_confirms_the_removal_and_moves_the_selection_to_the_next_task() {
        let app = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('j'), KeyCode::Char('d'), KeyCode::Char('y')],
        );
        assert_eq!(app.confirming, None);
        assert_eq!(app.removal, Some(TaskId(2)));
        assert_eq!(on(&app), Some(3));
    }

    #[test]
    fn confirming_the_removal_of_the_last_task_moves_the_selection_to_the_one_before() {
        let app = press(
            loaded(&[1, 2, 3]),
            &[KeyCode::Char('G'), KeyCode::Char('d'), KeyCode::Char('y')],
        );
        assert_eq!(app.removal, Some(TaskId(3)));
        assert_eq!(on(&app), Some(2));
    }

    #[test]
    fn n_and_esc_drop_the_question_and_change_nothing_else() {
        let before = loaded(&[1, 2]);
        for answer in [KeyCode::Char('n'), KeyCode::Esc] {
            let app = press(before.clone(), &[KeyCode::Char('d'), answer]);
            assert_eq!(app, before);
        }
    }

    #[test]
    fn while_a_removal_is_asked_about_only_its_answers_and_quit_are_heard() {
        let asked = press(loaded(&[1, 2]), &[KeyCode::Char('d')]);
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('G'),
            KeyCode::Char('a'),
            KeyCode::Char('?'),
            KeyCode::Char('d'),
            KeyCode::Char('x'),
        ] {
            assert_eq!(press(asked.clone(), &[key]), asked);
        }
        assert!(press(asked, &[KeyCode::Char('q')]).quit);
    }

    #[test]
    fn the_question_goes_when_its_task_is_gone_or_cancelled_from_elsewhere() {
        let asked = press(loaded(&[1, 2]), &[KeyCode::Char('d')]);
        let gone = update(asked.clone(), Event::Loaded(queue_of(&[2])));
        assert_eq!(gone.confirming, None);
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Cancelled;
        let cancelled = update(asked.clone(), Event::Loaded(queue));
        assert_eq!(cancelled.confirming, None);
        let same = update(asked, Event::Loaded(queue_of(&[1, 2, 3])));
        assert_eq!(same.confirming, Some(TaskId(1)));
    }

    fn form_of(app: &App) -> &Form {
        app.form.as_ref().expect("the form is open")
    }

    fn typed(app: App, text: &str) -> App {
        text.chars().fold(app, |app, c| {
            update(
                app,
                Event::Key(if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                }),
            )
        })
    }

    #[test]
    fn n_opens_an_empty_form_and_esc_closes_it_changing_nothing_else() {
        let before = loaded(&[1, 2]);
        let open = press(before.clone(), &[KeyCode::Char('n')]);
        assert_eq!(form_of(&open), &Form::new());
        assert_eq!(open.queue, before.queue);
        assert_eq!(press(open, &[KeyCode::Esc]), before);
    }

    #[test]
    fn while_the_form_is_open_letters_are_typed_and_no_queue_key_acts() {
        let open = press(loaded(&[1, 2]), &[KeyCode::Char('n')]);
        let app = typed(open, "qjdna?");
        assert!(!app.quit && !app.help && app.confirming.is_none() && app.form.is_some());
        assert_eq!(form_of(&app).draft().title, "qjdna?");
        assert_eq!(on(&app), Some(1));
        let app = press(app, &[KeyCode::Down, KeyCode::Char('G')]);
        assert_eq!(on(&app), Some(1));
        assert!(!press(app, &[KeyCode::Char('q')]).quit);
    }

    #[test]
    fn tab_and_shift_tab_move_the_focus_and_esc_leaves_no_trace_of_the_form() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n'), KeyCode::Tab]);
        assert_eq!(form_of(&app).focus, crate::form::Focus::Kind);
        let app = press(app, &[KeyCode::BackTab, KeyCode::BackTab]);
        assert_eq!(form_of(&app).focus, crate::form::Focus::Criterion(0));
        let closed = press(app, &[KeyCode::Esc]);
        assert_eq!(closed.form, None);
        assert_eq!(closed.submission, None);
    }

    #[test]
    fn ctrl_n_and_ctrl_d_add_and_remove_criteria() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Ctrl('n'));
        assert_eq!(form_of(&app).criteria.len(), 2);
        let app = update(update(app, Event::Ctrl('d')), Event::Ctrl('d'));
        assert!(form_of(&app).criteria.is_empty());
        let app = update(app, Event::Ctrl('x'));
        assert!(form_of(&app).criteria.is_empty());
    }

    #[test]
    fn ctrl_s_leaves_the_form_open_and_its_task_for_the_loop() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = typed(app, "Title");
        let app = update(app, Event::Ctrl('s'));
        assert_eq!(
            app.submission.as_ref().map(|draft| draft.title.as_str()),
            Some("Title")
        );
        assert!(app.form.is_some());
    }

    #[test]
    fn added_closes_the_form_and_rejected_shows_why_and_keeps_what_was_typed() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        let rejected = update(app.clone(), Event::Rejected(vec!["a problem".to_owned()]));
        assert_eq!(form_of(&rejected).problems, ["a problem"]);
        assert_eq!(form_of(&rejected).draft().title, "Title");
        assert_eq!(update(app, Event::Added).form, None);
    }

    #[test]
    fn ctrl_keys_do_nothing_on_the_queue() {
        let app = loaded(&[1, 2]);
        for letter in ['s', 'n', 'd', 'q'] {
            assert_eq!(update(app.clone(), Event::Ctrl(letter)), app);
        }
    }

    #[test]
    fn a_reload_keeps_the_form_open() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Ti");
        let app = update(app, Event::Loaded(queue_of(&[1, 2])));
        assert_eq!(form_of(&app).draft().title, "Ti");
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
