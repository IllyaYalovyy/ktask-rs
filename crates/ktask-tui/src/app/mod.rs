//! Which screen is showing, and how an event changes it. Each screen — the queue, the task
//! form, the import form, settings, the project picker and the registration screen — owns its
//! own state, key handling and drawing in its own module; this one only decides which of them
//! is open, and carries out what an open screen asks for: opening another one, or leaving
//! something for the loop in [`crate::run`] to do.

use ktask_core::{
    Channel, Placement, Project, ProviderCheck, ProviderView, QueueView, SettingView, TaskDraft,
    TaskId,
};
use ratatui::crossterm::event::KeyCode;

use crate::ack_screen::AckScreen;
use crate::answer_screen::AnswerScreen;
use crate::detail_screen::DetailScreen;
use crate::done_screen::DoneScreen;
use crate::import_screen::ImportScreen;
use crate::output_screen::OutputScreen;
use crate::projects::ProjectsScreen;
use crate::providers::ProvidersScreen;
use crate::queue::{self, Queue};
use crate::registration_screen::RegistrationScreen;
use crate::settings::SettingsScreen;
use crate::task_form::TaskFormScreen;

mod detail;
mod output;
mod provider_screen;
mod screens;

use detail::try_detail;
use output::try_output;
use provider_screen::try_providers;
use screens::{
    try_acknowledge, try_answer, try_done, try_form, try_import, try_projects, try_registration,
    try_settings,
};

/// Everything the terminal interface shows and remembers. Which screen is open is decided by
/// which of `form`, `import`, `settings`, `projects` and `registration` is `Some` — at most
/// one at a time — falling back to the queue when none is. The remaining fields are a mailbox:
/// an open screen leaves something in one of them for the loop in [`crate::run`] to carry out
/// against the real project, which answers with an [`Event`] once it has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    /// The world the binary was built for, shown in the title bar of every screen.
    pub(crate) channel: Channel,
    pub(crate) queue: Queue,
    pub(crate) form: Option<TaskFormScreen>,
    pub(crate) answer: Option<AnswerScreen>,
    pub(crate) done: Option<DoneScreen>,
    pub(crate) acknowledge: Option<AckScreen>,
    pub(crate) import: Option<ImportScreen>,
    pub(crate) settings: Option<SettingsScreen>,
    pub(crate) providers: Option<ProvidersScreen>,
    pub(crate) projects: Option<ProjectsScreen>,
    pub(crate) registration: Option<RegistrationScreen>,
    pub(crate) output: Option<OutputScreen>,
    pub(crate) detail: Option<DetailScreen>,
    /// The task the form was submitted with and where it goes, for the loop to add, answered
    /// with [`Event::Added`] or [`Event::Rejected`].
    pub(crate) submission: Option<(TaskDraft, Placement)>,
    /// The path the import form was submitted with, for the loop to import.
    pub(crate) import_submission: Option<String>,
    /// The settings screen's submission, for the loop to save.
    pub(crate) setting_submission: Option<(&'static str, String)>,
    /// The task whose removal was confirmed, for the loop to carry out.
    pub(crate) removal: Option<TaskId>,
    /// The task the operator asked to retry, for the loop to carry out.
    pub(crate) retrial: Option<TaskId>,
    /// The task and text the answer form was submitted with, for the loop to record.
    pub(crate) answer_submission: Option<(TaskId, String)>,
    /// The task and reason the done form was submitted with, for the loop to record.
    pub(crate) done_submission: Option<(TaskId, String)>,
    /// The task and optional message the acknowledgement form was submitted with.
    pub(crate) acknowledge_submission: Option<(TaskId, String)>,
    /// Set when the operator asked to start executing the pending tasks.
    pub(crate) run_requested: Option<()>,
    /// Set when the operator asked to open the settings screen.
    pub(crate) settings_requested: Option<()>,
    pub(crate) providers_requested: Option<()>,
    pub(crate) provider_check_requested: Option<String>,
    /// Set when the operator asked to open the project picker.
    pub(crate) projects_requested: Option<()>,
    /// The name of the project the picker was submitted with, for the loop to switch to.
    pub(crate) project_switch: Option<String>,
    /// The name of the project the picker's forget question was confirmed with, for the loop
    /// to forget.
    pub(crate) project_forget: Option<String>,
    /// The name the registration screen was submitted with, for the loop to register the
    /// current directory under.
    pub(crate) registration_submission: Option<String>,
    /// The selected task whose output the loop must load.
    pub(crate) output_requested: Option<TaskId>,
    /// The task whose detail the loop must load, for the detail screen to open on or refresh.
    pub(crate) detail_requested: Option<TaskId>,
    /// Set when the operator asked to leave.
    pub(crate) quit: bool,
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
    /// The run this screen started has ended, or refused to start at all, printing this — the
    /// same words `ktask-rs run` itself would show.
    RunMessage(String),
    /// The import this screen started has finished, printing this — the same words
    /// `ktask-rs import` itself would print — and, when any task was added, the first one's
    /// ID, so the queue can select it.
    ImportMessage(String, Option<TaskId>),
    /// The project's settings were loaded: the settings screen opens on these values.
    SettingsLoaded(Vec<SettingView>),
    /// The provider catalogue was loaded.
    ProvidersLoaded(Vec<ProviderView>),
    /// The selected provider's readiness check completed.
    ProviderChecked(ProviderCheck),
    /// The settings screen's submission was saved: it closes.
    SettingSaved,
    /// The settings screen's submission was refused, for this reason: it stays open and
    /// shows it.
    SettingRejected(String),
    /// The registered projects were loaded: the picker opens on them, the one whose queue is
    /// on show already focused.
    ProjectsLoaded(Vec<Project>),
    /// The loop switched to the project the picker was submitted with: its queue replaces the
    /// one on show, exactly as if it had been loaded from the start, and the picker closes.
    ProjectSwitched(QueueView),
    /// The picker's submission did not switch the project, for this reason: it stays open and
    /// shows it.
    ProjectSwitchFailed(String),
    /// The picker's forget question was confirmed and carried out: the loop forgot the project
    /// and gives the registered projects that remain; the picker stays open, showing them.
    ProjectForgotten(Vec<Project>),
    /// Forgetting the project the picker's question named did not happen, for this reason: the
    /// picker stays open and shows it.
    ProjectForgetFailed(String),
    /// The current directory was registered under the name the registration screen was
    /// submitted with: its queue opens, exactly as if it had been loaded from the start.
    Registered(QueueView),
    /// The registration screen's submission registered nothing, for this reason: it stays
    /// open, keeps what was typed, and shows it.
    RegistrationFailed(String),
    /// The selected task's freshly read output of one attempt.
    OutputLoaded(crate::LoadedOutput),
    /// The selected task's detail, freshly read, for the detail screen to open on or refresh.
    /// Boxed: a task's full detail is far larger than every other event, and this one is no
    /// more frequent than they are.
    DetailLoaded(Box<ktask_core::TaskDetail>),
}

/// The app after `event` happened to `app`. `Loaded` and a first Ctrl-C are handled here,
/// unconditionally, ahead of everything else that depends on which screen is open.
#[must_use]
pub fn update(app: App, event: Event) -> App {
    match event {
        Event::Loaded(queue) => {
            return App {
                queue: app.queue.loaded(queue),
                ..app
            };
        }
        Event::Ctrl('c') => return ctrl_c(app),
        _ => {}
    }
    dispatch(app, event)
}

/// Every screen that might own an event reaching [`dispatch`], tried in this order, the first
/// match winning.
const SCREENS: [fn(App, Event) -> Tried; 11] = [
    try_output,
    try_detail,
    try_settings,
    try_providers,
    try_form,
    try_answer,
    try_done,
    try_acknowledge,
    try_import,
    try_projects,
    try_registration,
];

/// The app after `event`, once it is known to be neither `Loaded` nor a first Ctrl-C: tried
/// against each of [`SCREENS`] in turn, the first match winning; an event none of them owns
/// reaches the queue, or changes nothing.
fn dispatch(app: App, event: Event) -> App {
    let (mut app, mut event) = (app, event);
    for screen in SCREENS {
        match screen(app, event) {
            Tried::Handled(app) => return *app,
            Tried::Unhandled(unhandled_app, unhandled_event) => {
                app = *unhandled_app;
                event = *unhandled_event;
            }
        }
    }
    match event {
        Event::RunMessage(text) => App {
            queue: app.queue.run_message(text),
            ..app
        },
        Event::ImportMessage(text, first) => App {
            queue: app.queue.imported(&text, first),
            ..app
        },
        Event::Key(key) => on_queue(app, |q| q.key(key)),
        _ => app,
    }
}

/// [`Tried::Handled`] of `app`, boxing it.
fn handled(app: App) -> Tried {
    Tried::Handled(Box::new(app))
}

/// What trying an event against one screen made of it: the app it produced, or, when the
/// event was not one that screen owns, the app and the event handed back unhandled for the
/// next screen to try in its turn.
enum Tried {
    // Boxed: `App` itself is large — it carries the queue's own view and every screen's own
    // state — so without indirection here, passing a `Tried` around (every screen's own
    // `try_*` returns one) would copy it on the stack each time.
    Handled(Box<App>),
    Unhandled(Box<App>, Box<Event>),
}

/// The app after Ctrl-C: it quits from every screen, except that a first Ctrl-C in a task form
/// that holds anything typed asks to discard it instead, and a second one then quits.
fn ctrl_c(app: App) -> App {
    let should_discard = app
        .form
        .as_ref()
        .is_some_and(|form| !form.is_discarding() && form.has_content());
    if should_discard {
        return App {
            form: app.form.map(TaskFormScreen::start_discard),
            ..app
        };
    }
    App { quit: true, ..app }
}

/// The app with `f`'s answer applied to the queue.
fn on_queue(app: App, f: impl FnOnce(Queue) -> (Queue, Option<queue::Request>)) -> App {
    let (screen, request) = f(app.queue);
    let app = App {
        queue: screen,
        ..app
    };
    apply_queue_request(app, request)
}

/// `app` with `request` carried out, when `request` opens another screen over the queue — the
/// app and `request` handed back, unchanged, when it asks for something else, for
/// [`apply_queue_request`] to carry out itself; pulled out of it so that function stays within
/// the workspace's function-length limit.
fn open_screen_for_request(
    app: App,
    request: queue::Request,
) -> Result<App, Box<(App, queue::Request)>> {
    match request {
        queue::Request::OpenForm(placement) => Ok(App {
            form: Some(TaskFormScreen::new(placement)),
            ..app
        }),
        queue::Request::OpenImport => Ok(App {
            import: Some(ImportScreen::new()),
            ..app
        }),
        queue::Request::OpenAnswer(id, question) => Ok(App {
            answer: Some(AnswerScreen::new(id, question)),
            ..app
        }),
        queue::Request::OpenDone(id) => Ok(App {
            done: Some(DoneScreen::new(id)),
            ..app
        }),
        queue::Request::OpenAcknowledge(id) => Ok(App {
            acknowledge: Some(AckScreen::new(id)),
            ..app
        }),
        queue::Request::OpenOutput(id) => Ok(App {
            output: Some(OutputScreen::new(id)),
            output_requested: Some(id),
            ..app
        }),
        queue::Request::OpenDetail(id) => Ok(App {
            detail: Some(DetailScreen::new(id)),
            detail_requested: Some(id),
            ..app
        }),
        other => Err(Box::new((app, other))),
    }
}

/// The app with `request`, when the queue left one, carried out: opens the screen or sets the
/// mailbox field it asks for — pulled out of [`on_queue`] so that function stays within the
/// workspace's function-length limit.
fn apply_queue_request(app: App, request: Option<queue::Request>) -> App {
    let Some(request) = request else {
        return app;
    };
    let (app, request) = match open_screen_for_request(app, request) {
        Ok(app) => return app,
        Err(pair) => *pair,
    };
    apply_queue_mailbox_request(app, &request)
}

/// Carries out a queue request that does not open one of the queue's overlay screens.
fn apply_queue_mailbox_request(app: App, request: &queue::Request) -> App {
    match request {
        queue::Request::StartRun => App {
            run_requested: Some(()),
            ..app
        },
        queue::Request::OpenSettings => App {
            settings_requested: Some(()),
            ..app
        },
        queue::Request::OpenProviders => App {
            providers_requested: Some(()),
            ..app
        },
        queue::Request::OpenProjects => App {
            projects_requested: Some(()),
            ..app
        },
        queue::Request::Remove(id) => App {
            removal: Some(*id),
            ..app
        },
        queue::Request::Retry(id) => App {
            retrial: Some(*id),
            ..app
        },
        queue::Request::Quit => App { quit: true, ..app },
        queue::Request::OpenForm(_)
        | queue::Request::OpenImport
        | queue::Request::OpenAnswer(..)
        | queue::Request::OpenDone(_)
        | queue::Request::OpenAcknowledge(_)
        | queue::Request::OpenOutput(_)
        | queue::Request::OpenDetail(_) => {
            unreachable!("handled by open_screen_for_request above")
        }
    }
}

/// The app once the task the form holds is added as `id`: the form closes, and a task put
/// next to the selected one is selected, so that it is already there when the queue is loaded
/// again.
fn added(app: App, id: TaskId) -> App {
    let placed_next_to_one = app
        .form
        .as_ref()
        .is_some_and(|form| form.placement() != Placement::End);
    App {
        form: None,
        queue: app.queue.added(id, placed_next_to_one),
        ..app
    }
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
            attempts: std::collections::HashMap::new(),
            history: std::collections::HashMap::new(),
            done_by_user: std::collections::HashMap::new(),
        }
    }

    fn loaded(ids: &[u64]) -> App {
        update(App::default(), Event::Loaded(queue_of(ids)))
    }

    fn press(app: App, keys: &[KeyCode]) -> App {
        keys.iter()
            .fold(app, |app, key| update(app, Event::Key(*key)))
    }

    #[test]
    fn q_quits_and_keeps_the_queue() {
        let app = press(loaded(&[1]), &[KeyCode::Char('q')]);
        assert!(app.quit);
        assert_eq!(app.queue, Queue::replaced(queue_of(&[1])));
    }

    #[test]
    fn n_opens_the_task_form_covering_the_queue() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        assert!(app.form.is_some());
        let app = press(app, &[KeyCode::Esc]);
        assert!(app.form.is_none());
    }

    #[test]
    fn i_opens_the_import_form() {
        let app = press(loaded(&[1]), &[KeyCode::Char('i')]);
        assert!(app.import.is_some());
    }

    #[test]
    fn ctrl_s_in_the_import_form_closes_it_and_leaves_the_path_for_the_loop() {
        let app = press(loaded(&[1]), &[KeyCode::Char('i')]);
        let app = "x.json"
            .chars()
            .fold(app, |app, c| update(app, Event::Key(KeyCode::Char(c))));
        let app = update(app, Event::Ctrl('s'));
        assert!(app.import.is_none());
        assert_eq!(app.import_submission, Some("x.json".to_owned()));
    }

    #[test]
    fn s_asks_the_loop_to_load_settings() {
        let app = press(loaded(&[1]), &[KeyCode::Char('s')]);
        assert_eq!(app.settings_requested, Some(()));
        let app = update(app, Event::SettingsLoaded(vec![]));
        assert!(app.settings.is_some());
        let app = press(app, &[KeyCode::Esc]);
        assert!(app.settings.is_none());
    }

    #[test]
    fn p_asks_the_loop_to_load_projects_and_the_picker_opens_on_the_current_one() {
        let app = press(loaded(&[1]), &[KeyCode::Char('p')]);
        assert_eq!(app.projects_requested, Some(()));
        let others = vec![
            Project {
                name: "app".to_owned(),
                path: PathBuf::from("/work/app"),
                registered_at: SystemTime::UNIX_EPOCH,
            },
            Project {
                name: "other".to_owned(),
                path: PathBuf::from("/work/other"),
                registered_at: SystemTime::UNIX_EPOCH,
            },
        ];
        let app = update(app, Event::ProjectsLoaded(others));
        assert!(app.projects.is_some());
    }

    #[test]
    fn d_confirmed_leaves_the_removal_for_the_loop() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('d'), KeyCode::Char('y')]);
        assert_eq!(app.removal, Some(TaskId(1)));
    }

    #[test]
    fn r_asks_the_loop_to_start_a_run() {
        let app = press(loaded(&[1]), &[KeyCode::Char('r')]);
        assert_eq!(app.run_requested, Some(()));
    }

    #[test]
    fn ctrl_c_in_an_empty_form_quits_at_once() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Ctrl('c'));
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_in_a_form_with_content_asks_to_discard_and_a_second_ctrl_c_quits() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Key(KeyCode::Char('T')));
        let asked = update(app, Event::Ctrl('c'));
        assert!(!asked.quit);
        assert!(
            asked
                .form
                .as_ref()
                .is_some_and(TaskFormScreen::is_discarding)
        );
        assert!(update(asked, Event::Ctrl('c')).quit);
    }

    #[test]
    fn y_after_ctrl_c_with_content_quits() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Key(KeyCode::Char('T')));
        let asked = update(app, Event::Ctrl('c'));
        assert!(update(asked, Event::Key(KeyCode::Char('y'))).quit);
    }

    #[test]
    fn ctrl_c_quits_from_every_other_screen() {
        let with_settings = update(loaded(&[1]), Event::SettingsLoaded(vec![]));
        assert!(update(with_settings, Event::Ctrl('c')).quit);
        let with_import = press(loaded(&[1]), &[KeyCode::Char('i')]);
        assert!(update(with_import, Event::Ctrl('c')).quit);
    }

    #[test]
    fn added_closes_the_form_and_selects_a_task_placed_next_to_the_selected_one() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('o')]);
        let app = update(app, Event::Added(TaskId(3)));
        assert!(app.form.is_none());
        assert_eq!(app.queue.selected(), Some(TaskId(3)));
    }

    #[test]
    fn rejected_keeps_the_form_open_and_shows_why() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Rejected(vec!["a problem".to_owned()]));
        assert!(app.form.is_some());
    }

    #[test]
    fn a_run_message_reaches_the_queue_even_while_settings_is_open() {
        let app = update(loaded(&[1]), Event::SettingsLoaded(vec![]));
        let app = update(app, Event::RunMessage("task 1: done".to_owned()));
        assert!(app.settings.is_some());
        let app = App {
            settings: None,
            ..app
        };
        assert!(app.queue.view().is_some());
    }

    #[test]
    fn a_loaded_queue_updates_underneath_an_open_form() {
        let app = press(loaded(&[1]), &[KeyCode::Char('n')]);
        let app = update(app, Event::Loaded(queue_of(&[1, 2])));
        assert!(app.form.is_some());
        assert_eq!(app.queue.view(), Some(&queue_of(&[1, 2])));
    }

    #[test]
    fn registration_screen_esc_quits_since_there_is_no_queue_to_fall_back_to() {
        let app = App {
            registration: Some(RegistrationScreen::new("conflict".to_owned())),
            ..App::default()
        };
        let app = press(app, &[KeyCode::Esc]);
        assert!(app.quit);
    }

    #[test]
    fn registered_replaces_the_registration_screen_with_the_fresh_queue() {
        let app = App {
            registration: Some(RegistrationScreen::new("conflict".to_owned())),
            ..App::default()
        };
        let app = update(app, Event::Registered(queue_of(&[1])));
        assert!(app.registration.is_none());
        assert_eq!(app.queue.view(), Some(&queue_of(&[1])));
    }
}
