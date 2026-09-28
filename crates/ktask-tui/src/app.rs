//! The state of the terminal interface and how events change it.

use ktask_core::{Placement, QueueView, TaskDraft, TaskId, TaskStatus};
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
    /// What the operator is being asked to confirm, if anything; the answer is the next key.
    pub confirming: Option<Confirming>,
    /// The task `d` just refused to remove because it is running: shown until the next key,
    /// or until it is no longer running. No confirmation is asked for it.
    pub refused: Option<TaskId>,
    /// The task whose removal was confirmed: the loop carries it out and clears this.
    pub removal: Option<TaskId>,
    /// The form a new task is written in, while it is open; it covers the queue.
    pub(crate) form: Option<Form>,
    /// The task the form was submitted with and where it goes: the loop adds it and answers
    /// with [`Event::Added`] or [`Event::Rejected`], and clears this.
    pub submission: Option<(TaskDraft, Placement)>,
    /// Set when the operator asked to leave.
    pub quit: bool,
}

/// What [`App::confirming`] is asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirming {
    /// Removing this task.
    Removal(TaskId),
    /// Discarding the open form's content, after a first Ctrl-C found something typed in it.
    Discard,
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
    /// The task the form was submitted with was added, and has this number: the form closes.
    Added(TaskId),
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
            let confirming = app.confirming.filter(|confirm| match confirm {
                Confirming::Removal(id) => removable(&queue, *id),
                Confirming::Discard => true,
            });
            let refused = app.refused.filter(|id| is_running(&queue, *id));
            App {
                queue: Some(queue),
                selected,
                confirming,
                refused,
                ..app
            }
        }
        Event::Ctrl('c') => ctrl_c(app),
        Event::Key(key) if app.confirming == Some(Confirming::Discard) => match key {
            KeyCode::Char('y') => App { quit: true, ..app },
            KeyCode::Char('n') | KeyCode::Esc => App {
                confirming: None,
                ..app
            },
            _ => app,
        },
        Event::Ctrl(_) if app.confirming == Some(Confirming::Discard) => app,
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
        Event::Added(id) => added(app, id),
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
        Event::Key(key) => {
            // `refused` is a one-shot notice: any key past the one that raised it dismisses
            // it, whether or not that key is `d` again.
            let app = App {
                refused: None,
                ..app
            };
            match key {
                KeyCode::Char('?') => App { help: true, ..app },
                KeyCode::Char('a') => App {
                    show_cancelled: !app.show_cancelled,
                    ..app
                },
                KeyCode::Char('j') | KeyCode::Down => {
                    select(app, |index, _| index.saturating_add(1))
                }
                KeyCode::Char('k') | KeyCode::Up => select(app, |index, _| index.saturating_sub(1)),
                KeyCode::Char('n') => open_form(app, Placement::End),
                KeyCode::Char('o') => open_form_next_to(app, Placement::After),
                KeyCode::Char('O') => open_form_next_to(app, Placement::Before),
                KeyCode::Char('d') => press_d(app),
                KeyCode::Char('g') => select(app, |_, _| 0),
                KeyCode::Char('G') => select(app, |_, len| len.saturating_sub(1)),
                _ => app,
            }
        }
        Event::Ctrl(_) | Event::Resize => app,
    }
}

/// The app after Ctrl-C: it quits from every screen, except that a first Ctrl-C in a form
/// that holds anything typed asks to discard it instead, and a second one then quits.
fn ctrl_c(app: App) -> App {
    let already_asked = app.confirming == Some(Confirming::Discard);
    if already_asked || !app.form.as_ref().is_some_and(Form::has_content) {
        return App { quit: true, ..app };
    }
    App {
        confirming: Some(Confirming::Discard),
        ..app
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
        submission: app.form.as_ref().map(|form| (form.draft(), form.placement)),
        ..app
    }
}

/// The app with an empty form open, for a task that goes at `placement`.
fn open_form(app: App, placement: Placement) -> App {
    App {
        form: Some(Form::new(placement)),
        ..app
    }
}

/// The app with an empty form open, for a task that goes next to the selected one the way
/// `beside` says; at the end when nothing is selected.
fn open_form_next_to(app: App, beside: fn(TaskId) -> Placement) -> App {
    let placement = app.selected.map_or(Placement::End, beside);
    open_form(app, placement)
}

/// The app once the task the form holds is added as `id`: the form closes, and a task put
/// next to the selected one is selected, so that it is already there when the queue is
/// loaded again.
fn added(app: App, id: TaskId) -> App {
    let placed_next_to_one = app
        .form
        .as_ref()
        .is_some_and(|form| form.placement != Placement::End);
    App {
        form: None,
        selected: if placed_next_to_one {
            Some(id)
        } else {
            app.selected
        },
        ..app
    }
}

/// Whether `queue` shows the task `id` and it can still be removed: a cancelled or a running
/// one cannot.
fn removable(queue: &QueueView, id: TaskId) -> bool {
    queue.tasks.iter().any(|task| {
        task.id == id && task.status != TaskStatus::Cancelled && task.status != TaskStatus::Running
    })
}

/// Whether `queue` shows the task `id` as running.
fn is_running(queue: &QueueView, id: TaskId) -> bool {
    queue
        .tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Running)
}

/// The app after `d` on the selected task: it asks to confirm removing it when it can be
/// removed, refuses without asking when it is running, and otherwise, with nothing selected
/// or the selection cancelled already, changes nothing.
fn press_d(app: App) -> App {
    let Some(id) = app.selected else {
        return app;
    };
    let Some(queue) = &app.queue else {
        return app;
    };
    if removable(queue, id) {
        App {
            confirming: Some(Confirming::Removal(id)),
            ..app
        }
    } else if is_running(queue, id) {
        App {
            refused: Some(id),
            ..app
        }
    } else {
        app
    }
}

/// The app once the removal it asks about is confirmed: the removal is left for the loop to
/// carry out, and the selection moves to the task after the one removed, or the one before
/// it when it was the last, so that it is already there when the queue is loaded again.
fn confirm_removal(app: App) -> App {
    let Some(Confirming::Removal(id)) = app.confirming else {
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
            attempts: std::collections::HashMap::new(),
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
        assert_eq!(app.confirming, Some(Confirming::Removal(TaskId(2))));
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
    fn d_on_a_running_task_refuses_without_asking_and_changes_nothing_else() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue.clone()));

        let app = press(app, &[KeyCode::Char('d')]);

        assert_eq!(app.confirming, None);
        assert_eq!(app.removal, None);
        assert_eq!(app.refused, Some(TaskId(1)));
        assert_eq!(app.queue, Some(queue));
    }

    #[test]
    fn the_refusal_is_dismissed_by_the_next_key_that_is_not_d_again() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue));
        let refused = press(app, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(TaskId(1)));

        for key in [KeyCode::Char('j'), KeyCode::Char('x')] {
            assert_eq!(press(refused.clone(), &[key]).refused, None);
        }
        // The task is still running, so d again just shows the same refusal afresh.
        assert_eq!(
            press(refused, &[KeyCode::Char('d')]).refused,
            Some(TaskId(1))
        );
    }

    #[test]
    fn the_refusal_goes_when_its_task_stops_running_from_elsewhere() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue));
        let refused = press(app, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(TaskId(1)));

        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Done;
        let reloaded = update(refused, Event::Loaded(queue));
        assert_eq!(reloaded.refused, None);
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
        assert_eq!(same.confirming, Some(Confirming::Removal(TaskId(1))));
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
        assert_eq!(form_of(&open), &Form::new(Placement::End));
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
            app.submission
                .as_ref()
                .map(|(draft, placement)| (draft.title.as_str(), *placement)),
            Some(("Title", Placement::End))
        );
        assert!(app.form.is_some());
    }

    #[test]
    fn added_closes_the_form_and_rejected_shows_why_and_keeps_what_was_typed() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        let rejected = update(app.clone(), Event::Rejected(vec!["a problem".to_owned()]));
        assert_eq!(form_of(&rejected).problems, ["a problem"]);
        assert_eq!(form_of(&rejected).draft().title, "Title");
        assert_eq!(update(app, Event::Added(TaskId(2))).form, None);
    }

    #[test]
    fn o_and_capital_o_open_a_form_for_a_task_below_or_above_the_selected_one() {
        let app = press(loaded(&[1, 2, 3]), &[KeyCode::Char('j')]);
        let below = press(app.clone(), &[KeyCode::Char('o')]);
        assert_eq!(form_of(&below).placement, Placement::After(TaskId(2)));
        let above = press(app, &[KeyCode::Char('O')]);
        assert_eq!(form_of(&above).placement, Placement::Before(TaskId(2)));
        assert_eq!(on(&above), Some(2));
    }

    #[test]
    fn o_and_capital_o_on_an_empty_queue_open_a_form_for_a_task_at_the_end() {
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let app = press(loaded(&[]), &[key]);
            assert_eq!(form_of(&app).placement, Placement::End);
        }
    }

    #[test]
    fn a_submitted_form_carries_where_its_task_goes() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('O')]);
        let app = update(typed(app, "T"), Event::Ctrl('s'));
        assert_eq!(
            app.submission.map(|(_, placement)| placement),
            Some(Placement::Before(TaskId(1)))
        );
    }

    #[test]
    fn a_task_added_next_to_the_selected_one_is_selected_and_one_added_at_the_end_is_not() {
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let app = press(loaded(&[1, 2]), &[key]);
            let app = update(app, Event::Added(TaskId(3)));
            assert_eq!((on(&app), app.form.is_none()), (Some(3), true));
            let reloaded = update(app, Event::Loaded(queue_of(&[1, 3, 2])));
            assert_eq!(on(&reloaded), Some(3));
        }
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('n')]);
        assert_eq!(on(&update(app, Event::Added(TaskId(3)))), Some(1));
    }

    #[test]
    fn ctrl_keys_do_nothing_on_the_queue() {
        let app = loaded(&[1, 2]);
        for letter in ['s', 'n', 'd', 'q'] {
            assert_eq!(update(app.clone(), Event::Ctrl(letter)), app);
        }
    }

    #[test]
    fn ctrl_c_quits_from_the_queue_the_key_map_and_a_removal_question() {
        assert!(update(loaded(&[1]), Event::Ctrl('c')).quit);
        let help = press(loaded(&[1]), &[KeyCode::Char('?')]);
        assert!(update(help, Event::Ctrl('c')).quit);
        let asked = press(loaded(&[1]), &[KeyCode::Char('d')]);
        assert!(update(asked, Event::Ctrl('c')).quit);
    }

    #[test]
    fn ctrl_c_quits_an_empty_form_without_asking_to_discard() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Ctrl('c'));
        assert!(app.quit);
        assert_eq!(app.confirming, None);
    }

    #[test]
    fn ctrl_c_in_a_form_with_content_asks_to_discard_and_a_second_ctrl_c_quits() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        let asked = update(app, Event::Ctrl('c'));
        assert!(!asked.quit && asked.form.is_some());
        assert_eq!(asked.confirming, Some(Confirming::Discard));
        assert_eq!(form_of(&asked).draft().title, "Title");
        assert!(update(asked, Event::Ctrl('c')).quit);
    }

    #[test]
    fn y_quits_after_being_asked_to_discard() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        let asked = update(app, Event::Ctrl('c'));
        assert!(update(asked, Event::Key(KeyCode::Char('y'))).quit);
    }

    #[test]
    fn n_and_esc_return_to_the_form_with_its_content_intact() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        for answer in [KeyCode::Char('n'), KeyCode::Esc] {
            let asked = update(app.clone(), Event::Ctrl('c'));
            let back = update(asked, Event::Key(answer));
            assert!(!back.quit && back.form.is_some());
            assert_eq!(back.confirming, None);
            assert_eq!(form_of(&back).draft().title, "Title");
        }
    }

    #[test]
    fn while_discard_is_asked_only_y_n_esc_and_a_second_ctrl_c_are_answered() {
        let app = typed(press(loaded(&[1]), &[KeyCode::Char('n')]), "Title");
        let asked = update(app, Event::Ctrl('c'));
        for key in [KeyCode::Char('q'), KeyCode::Char('x'), KeyCode::Enter] {
            assert_eq!(update(asked.clone(), Event::Key(key)), asked);
        }
        for letter in ['s', 'n', 'd'] {
            assert_eq!(update(asked.clone(), Event::Ctrl(letter)), asked);
        }
        assert!(update(asked, Event::Ctrl('c')).quit);
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
