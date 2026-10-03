//! Carrying out whatever background action `App` has pending — a task added or removed, a
//! setting loaded or saved, a project loaded, switched to or forgotten, a directory
//! registered, a file imported — through [`Application`], and turning the result into the
//! [`Event`] that closes or updates the screen that asked for it.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use ratatui::crossterm::event::Event as Input;

use crate::application::Application;
use crate::{App, Event, update};

use super::Wake;
use super::report_text::{import_text, report_text};

/// Removes, retries, answers, marks done or adds the task `app` has pending, if any, through
/// `application`: `(app, true)` when one was, `(app, false)`, unchanged, otherwise.
fn handle_task_action(mut app: App, application: &impl Application) -> Result<(App, bool), String> {
    if let Some(id) = app.removal.take() {
        application
            .remove_task(id)
            .map_err(|error| error.to_string())?;
        return Ok((app, true));
    }
    if let Some(id) = app.retrial.take() {
        application
            .retry_task(id)
            .map_err(|error| error.to_string())?;
        return Ok((app, true));
    }
    if let Some((id, text)) = app.answer_submission.take() {
        application
            .answer_task(id, &text)
            .map_err(|error| error.to_string())?;
        return Ok((app, true));
    }
    if let Some((id, reason)) = app.done_submission.take() {
        application
            .done_task(id, &reason)
            .map_err(|error| error.to_string())?;
        return Ok((app, true));
    }
    if let Some((id, message)) = app.acknowledge_submission.take() {
        application
            .acknowledge_task(id, (!message.trim().is_empty()).then_some(message.as_str()))
            .map_err(|error| error.to_string())?;
        return Ok((app, true));
    }
    if let Some((draft, placement)) = app.submission.take() {
        let added = match application.add_task(&draft, placement) {
            Ok(id) => Event::Added(id),
            Err(problems) => Event::Rejected(problems.iter().map(ToString::to_string).collect()),
        };
        return Ok((update(app, added), true));
    }
    Ok((app, false))
}

/// Loads the settings screen or saves the setting `app` has pending, if either, through
/// `application`: `(app, true)` when one was, `(app, false)`, unchanged, otherwise.
fn handle_settings_action(
    mut app: App,
    application: &impl Application,
) -> Result<(App, bool), String> {
    if app.settings_requested.take().is_some() {
        let views = application
            .load_settings()
            .map_err(|error| error.to_string())?;
        return Ok((update(app, Event::SettingsLoaded(views)), true));
    }
    if let Some((name, value)) = app.setting_submission.take() {
        let event = match application.save_setting(name, &value) {
            Ok(_) => Event::SettingSaved,
            Err(error) => Event::SettingRejected(error.to_string()),
        };
        return Ok((update(app, event), true));
    }
    Ok((app, false))
}

/// Loads the project picker, switches to the project `app` has pending, or forgets the one its
/// confirmation named, whichever `app` has pending, through `application`: `(app, true)` when
/// one was, `(app, false)`, unchanged, otherwise.
fn handle_projects_action(
    mut app: App,
    application: &impl Application,
) -> Result<(App, bool), String> {
    if app.projects_requested.take().is_some() {
        let projects = application
            .load_projects()
            .map_err(|error| error.to_string())?;
        return Ok((update(app, Event::ProjectsLoaded(projects)), true));
    }
    if let Some(name) = app.project_switch.take() {
        let event = match application.switch_project(&name) {
            Ok(queue) => Event::ProjectSwitched(queue),
            Err(error) => Event::ProjectSwitchFailed(error.to_string()),
        };
        return Ok((update(app, event), true));
    }
    if let Some(name) = app.project_forget.take() {
        let event = match application.forget_project(&name) {
            Ok(projects) => Event::ProjectForgotten(projects),
            Err(error) => Event::ProjectForgetFailed(error.to_string()),
        };
        return Ok((update(app, event), true));
    }
    Ok((app, false))
}

/// Registers the current directory under the name `app`'s registration screen was submitted
/// with, if any, through `application`. Succeeding gives the fresh queue to show, exactly as
/// if it had been loaded from the start, so `(app, true)`, the same as every other background
/// action, tells the loop to load it. Failing keeps the screen open and shows why, the same
/// words `ktask-rs project register --name` gives for the same conflict, so another name can
/// be tried — `(app, false)` here, unlike every other background action's own failure, since
/// there is no project open yet for the loop to load a queue from.
fn handle_registration_action(mut app: App, application: &impl Application) -> (App, bool) {
    let Some(name) = app.registration_submission.take() else {
        return (app, false);
    };
    match application.register(&name) {
        Ok(queue) => (update(app, Event::Registered(queue)), true),
        Err(error) => (
            update(app, Event::RegistrationFailed(error.to_string())),
            false,
        ),
    }
}

/// Imports the file `app` has pending, if any, through `application`: `(app, true)` when one
/// was, `(app, false)`, unchanged, otherwise. Shows the same result, or the same refusal,
/// `ktask-rs import` itself would print, one line per line of it — built here, from the typed
/// value `application.import` gives, not received already rendered into text — and selects the
/// first task added, when any was.
fn handle_import_action(mut app: App, application: &impl Application) -> (App, bool) {
    let Some(path) = app.import_submission.take() else {
        return (app, false);
    };
    let (text, first) = match application.import(&path) {
        Ok(import) => (import_text(&import), import.tasks.first().map(|t| t.id)),
        Err(problem) => (problem.to_string(), None),
    };
    (update(app, Event::ImportMessage(text, first)), true)
}

/// Evaluates to the app a `(App, bool)` pair carries once its `bool` is read, returning at
/// once with `Ok((app, true))` when it was `true` — the shared shape of trying one background
/// action after another in [`handle_input`], in the order the first one pending wins.
macro_rules! or_return_handled {
    ($pair:expr) => {{
        let (app, handled) = $pair;
        if handled {
            return Ok((app, true));
        }
        app
    }};
}

/// Tries registering the current directory, removing, retrying or adding a task, loading or
/// saving a setting, and loading, switching or forgetting a project, in that order: the first
/// one `app` has pending wins. `(app, true)` when one did, `(app, false)` otherwise.
fn try_background_actions(app: App, application: &impl Application) -> Result<(App, bool), String> {
    let app = or_return_handled!(handle_registration_action(app, application));
    let app = or_return_handled!(handle_task_action(app, application)?);
    let app = or_return_handled!(handle_settings_action(app, application)?);
    handle_projects_action(app, application)
}

/// Applies whatever [`Event`] `input` translates to, then tries every background action `app`
/// may now have pending — in order, the first one pending wins — including starting a run on a
/// thread of its own when the operator asked for it. Returns whether the queue is worth
/// loading again before the next frame: because a background action changed something, or
/// because the operator's own key changed whether cancelled tasks are shown.
pub(super) fn handle_input<A: Application + Send + Sync + 'static>(
    mut app: App,
    application: &Arc<A>,
    input: &Input,
    sender: &Sender<Wake>,
) -> Result<(App, bool), String> {
    let asked = app.queue.show_cancelled();
    if let Some(event) = super::translate(input) {
        app = update(app, event);
    }
    let app = or_return_handled!(try_background_actions(app, application.as_ref())?);
    let mut app = or_return_handled!(handle_import_action(app, application.as_ref()));
    if app.run_requested.take().is_some() {
        spawn_run(Arc::clone(application), sender.clone());
        return Ok((app, true));
    }
    let should_reload = app.queue.show_cancelled() != asked;
    Ok((app, should_reload))
}

/// What `application.start_run()` gave, worded — built here, from the typed value it gives,
/// not received already rendered into text.
fn run_result_text<E: std::fmt::Display>(result: Result<ktask_core::RunReport, E>) -> String {
    match result {
        Ok(report) => report_text(&report),
        Err(refusal) => refusal.to_string(),
    }
}

/// Calls `application.start_run()` on a thread of its own, so the loop stays responsive for
/// however long the run it starts takes, and raises [`Wake::RunMessage`] with what it printed
/// once it returns, whether it ran to completion, stopped partway, or refused to start at all.
fn spawn_run<A: Application + Send + Sync + 'static>(application: Arc<A>, sender: Sender<Wake>) {
    std::thread::spawn(move || {
        let text = run_result_text(application.start_run());
        let _ = sender.send(Wake::RunMessage(text));
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fmt;

    use ktask_core::{
        ATTEMPT_TIMEOUT, Import, Placement, Project, QueueView, RunEnd, RunReport, SettingView,
        StatusSummary, TaskDraft, TaskId, TaskStatus,
    };

    use super::{
        handle_import_action, handle_projects_action, handle_registration_action,
        handle_settings_action, handle_task_action,
    };
    use crate::App;
    use crate::application::Application;
    use crate::projects::ProjectsScreen;
    use crate::registration_screen::RegistrationScreen;
    use crate::settings::SettingsScreen;
    use crate::task_form::TaskFormScreen;

    /// An error a [`Fake`] gives back, carrying whatever text the test gave it: proof that
    /// `run.rs` turns it into that same text only when the failed action is handled, never
    /// earlier — a plain `String` could not tell the two apart.
    #[derive(Debug, Clone)]
    struct Failure(String);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "failed: {}", self.0)
        }
    }

    /// What each use case of a [`Fake`] gives back, queued in the order the test expects them
    /// to be called.
    #[derive(Default)]
    struct Fake {
        add: RefCell<Vec<Result<TaskId, Vec<Failure>>>>,
        remove: RefCell<Vec<Result<(), Failure>>>,
        retry: RefCell<Vec<Result<(), Failure>>>,
        answer: RefCell<Vec<Result<(), Failure>>>,
        done: RefCell<Vec<Result<(), Failure>>>,
        acknowledge: RefCell<Vec<Result<(), Failure>>>,
        settings: RefCell<Vec<Result<Vec<SettingView>, Failure>>>,
        save_setting: RefCell<Vec<Result<SettingView, Failure>>>,
        import: RefCell<Vec<Result<Import, Failure>>>,
        projects: RefCell<Vec<Result<Vec<Project>, Failure>>>,
        switch: RefCell<Vec<Result<QueueView, Failure>>>,
        forget: RefCell<Vec<Result<Vec<Project>, Failure>>>,
        register: RefCell<Vec<Result<QueueView, Failure>>>,
        start_run: RefCell<Vec<Result<RunReport, Failure>>>,
    }

    fn empty_queue() -> QueueView {
        QueueView {
            project: Project {
                name: "p".to_owned(),
                path: std::path::PathBuf::from("/p"),
                registered_at: std::time::SystemTime::UNIX_EPOCH,
            },
            tasks: Vec::new(),
            summary: StatusSummary::default(),
            attempts: std::collections::HashMap::new(),
            history: std::collections::HashMap::new(),
            done_by_user: std::collections::HashMap::new(),
        }
    }

    impl Application for Fake {
        type LoadError = Failure;
        type RemoveError = Failure;
        type RetryError = Failure;
        type AnswerError = Failure;
        type DoneError = Failure;
        type AcknowledgeError = Failure;
        type AddProblem = Failure;
        type SettingsError = Failure;
        type SaveSettingError = Failure;
        type ProjectsError = Failure;
        type SwitchError = Failure;
        type ForgetError = Failure;
        type RegisterError = Failure;
        type ImportError = Failure;
        type RunRefusal = Failure;

        fn load_queue(&self, _show_cancelled: bool) -> Result<QueueView, Failure> {
            Ok(empty_queue())
        }

        fn remove_task(&self, _id: TaskId) -> Result<(), Failure> {
            self.remove.borrow_mut().remove(0)
        }

        fn retry_task(&self, _id: TaskId) -> Result<(), Failure> {
            self.retry.borrow_mut().remove(0)
        }

        fn answer_task(&self, _id: TaskId, _text: &str) -> Result<(), Failure> {
            self.answer.borrow_mut().remove(0)
        }

        fn done_task(&self, _id: TaskId, _reason: &str) -> Result<(), Failure> {
            self.done.borrow_mut().remove(0)
        }

        fn acknowledge_task(&self, _id: TaskId, _message: Option<&str>) -> Result<(), Failure> {
            self.acknowledge.borrow_mut().remove(0)
        }

        fn add_task(
            &self,
            _draft: &TaskDraft,
            _placement: Placement,
        ) -> Result<TaskId, Vec<Failure>> {
            self.add.borrow_mut().remove(0)
        }

        fn load_settings(&self) -> Result<Vec<SettingView>, Failure> {
            self.settings.borrow_mut().remove(0)
        }

        fn save_setting(&self, _name: &str, _value: &str) -> Result<SettingView, Failure> {
            self.save_setting.borrow_mut().remove(0)
        }

        fn import(&self, _path: &str) -> Result<Import, Failure> {
            self.import.borrow_mut().remove(0)
        }

        fn load_projects(&self) -> Result<Vec<Project>, Failure> {
            self.projects.borrow_mut().remove(0)
        }

        fn switch_project(&self, _name: &str) -> Result<QueueView, Failure> {
            self.switch.borrow_mut().remove(0)
        }

        fn forget_project(&self, _name: &str) -> Result<Vec<Project>, Failure> {
            self.forget.borrow_mut().remove(0)
        }

        fn register(&self, _name: &str) -> Result<QueueView, Failure> {
            self.register.borrow_mut().remove(0)
        }

        fn start_run(&self) -> Result<RunReport, Failure> {
            self.start_run.borrow_mut().remove(0)
        }
    }

    fn setting() -> SettingView {
        SettingView {
            name: ATTEMPT_TIMEOUT,
            value: "1".to_owned(),
            is_default: true,
        }
    }

    fn empty_draft() -> TaskDraft {
        TaskDraft {
            title: String::new(),
            body: String::new(),
            criteria: Vec::new(),
            kind: ktask_core::TaskKind::default(),
            links: Vec::new(),
        }
    }

    #[test]
    fn a_rejected_task_shows_the_error_types_own_message_not_a_pre_rendered_one() {
        let fake = Fake::default();
        *fake.add.borrow_mut() = vec![Err(vec![Failure("empty title".to_owned())])];
        let app = App {
            form: Some(TaskFormScreen::new(Placement::End)),
            submission: Some((empty_draft(), Placement::End)),
            ..App::default()
        };

        let (app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.form.expect("form stays open").problems(),
            ["failed: empty title".to_owned()]
        );
    }

    #[test]
    fn an_added_task_closes_the_form() {
        let fake = Fake::default();
        *fake.add.borrow_mut() = vec![Ok(TaskId(7))];
        let app = App {
            form: Some(TaskFormScreen::new(Placement::End)),
            submission: Some((empty_draft(), Placement::End)),
            ..App::default()
        };

        let (app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(handled);
        assert!(app.form.is_none());
    }

    #[test]
    fn removing_a_task_that_fails_propagates_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.remove.borrow_mut() = vec![Err(Failure("no such task".to_owned()))];
        let app = App {
            removal: Some(TaskId(1)),
            ..App::default()
        };

        let error = handle_task_action(app, &fake).unwrap_err();

        assert_eq!(error, "failed: no such task");
    }

    #[test]
    fn answering_a_task_that_fails_propagates_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.answer.borrow_mut() = vec![Err(Failure("task 1 is pending".to_owned()))];
        let app = App {
            answer_submission: Some((TaskId(1), "the left one".to_owned())),
            ..App::default()
        };

        let error = handle_task_action(app, &fake).unwrap_err();

        assert_eq!(error, "failed: task 1 is pending");
    }

    #[test]
    fn a_successful_answer_leaves_nothing_pending() {
        let fake = Fake::default();
        *fake.answer.borrow_mut() = vec![Ok(())];
        let app = App {
            answer_submission: Some((TaskId(1), "the left one".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(handled);
        assert!(app.answer_submission.is_none());
    }

    #[test]
    fn marking_a_task_done_that_fails_propagates_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.done.borrow_mut() = vec![Err(Failure("task 1 is pending".to_owned()))];
        let app = App {
            done_submission: Some((TaskId(1), "fixed by hand".to_owned())),
            ..App::default()
        };

        let error = handle_task_action(app, &fake).unwrap_err();

        assert_eq!(error, "failed: task 1 is pending");
    }

    #[test]
    fn a_successful_done_marking_leaves_nothing_pending() {
        let fake = Fake::default();
        *fake.done.borrow_mut() = vec![Ok(())];
        let app = App {
            done_submission: Some((TaskId(1), "fixed by hand".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(handled);
        assert!(app.done_submission.is_none());
    }

    #[test]
    fn nothing_pending_leaves_the_task_action_unhandled() {
        let fake = Fake::default();
        let app = App::default();

        let (_app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(!handled);
    }

    #[test]
    fn a_refused_setting_shows_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.save_setting.borrow_mut() = vec![Err(Failure("not a whole number".to_owned()))];
        let app = App {
            settings: Some(SettingsScreen::new(&[])),
            setting_submission: Some((ATTEMPT_TIMEOUT, "soon".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_settings_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.settings
                .expect("settings screen stays open")
                .problem()
                .map(ToOwned::to_owned),
            Some("failed: not a whole number".to_owned())
        );
    }

    #[test]
    fn loading_settings_that_fails_propagates_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.settings.borrow_mut() = vec![Err(Failure("disk on fire".to_owned()))];
        let app = App {
            settings_requested: Some(()),
            ..App::default()
        };

        let error = handle_settings_action(app, &fake).unwrap_err();

        assert_eq!(error, "failed: disk on fire");
    }

    #[test]
    fn a_saved_setting_closes_the_settings_screen() {
        let fake = Fake::default();
        *fake.save_setting.borrow_mut() = vec![Ok(setting())];
        let app = App {
            settings: Some(SettingsScreen::new(&[])),
            setting_submission: Some((ATTEMPT_TIMEOUT, "1".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_settings_action(app, &fake).unwrap();

        assert!(handled);
        assert!(app.settings.is_none());
    }

    #[test]
    fn a_refused_import_shows_the_error_types_own_message_not_a_pre_rendered_one() {
        let fake = Fake::default();
        *fake.import.borrow_mut() = vec![Err(Failure("cannot read x".to_owned()))];
        let app = App {
            import_submission: Some("x".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_import_action(app, &fake);

        assert!(handled);
        assert_eq!(
            app.queue.message(),
            Some(["failed: cannot read x".to_owned()].as_slice())
        );
    }

    #[test]
    fn a_successful_import_shows_the_count_and_ids_built_from_the_typed_value_and_selects_it() {
        let fake = Fake::default();
        *fake.import.borrow_mut() = vec![Ok(Import {
            tasks: vec![ktask_core::Task {
                id: TaskId(5),
                position: 1,
                title: "t".to_owned(),
                body: String::new(),
                criteria: vec!["it works".to_owned()],
                kind: ktask_core::TaskKind::Agent,
                links: vec![],
                status: TaskStatus::Pending,
                created_at: std::time::SystemTime::UNIX_EPOCH,
            }],
            skipped_cancelled: 1,
        })];
        let app = App {
            import_submission: Some("x".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_import_action(app, &fake);

        assert!(handled);
        assert_eq!(
            app.queue.message(),
            Some(
                [
                    "1 task added: 5".to_owned(),
                    "1 cancelled task was skipped".to_owned()
                ]
                .as_slice()
            )
        );
        assert_eq!(app.queue.selected(), Some(TaskId(5)));
    }

    #[test]
    fn a_failed_switch_shows_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.switch.borrow_mut() = vec![Err(Failure("unknown project".to_owned()))];
        let app = App {
            projects: Some(ProjectsScreen::open(vec![], None)),
            project_switch: Some("ghost".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_projects_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.projects
                .expect("picker stays open")
                .problem()
                .map(ToOwned::to_owned),
            Some("failed: unknown project".to_owned())
        );
    }

    #[test]
    fn a_failed_forget_shows_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.forget.borrow_mut() = vec![Err(Failure("unknown project".to_owned()))];
        let app = App {
            projects: Some(ProjectsScreen::open(vec![], None)),
            project_forget: Some("ghost".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_projects_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.projects
                .expect("picker stays open")
                .problem()
                .map(ToOwned::to_owned),
            Some("failed: unknown project".to_owned())
        );
    }

    #[test]
    fn loading_projects_that_fails_propagates_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.projects.borrow_mut() = vec![Err(Failure("disk on fire".to_owned()))];
        let app = App {
            projects_requested: Some(()),
            ..App::default()
        };

        let error = handle_projects_action(app, &fake).unwrap_err();

        assert_eq!(error, "failed: disk on fire");
    }

    #[test]
    fn a_failed_registration_shows_the_error_types_own_message_and_stays_open() {
        let fake = Fake::default();
        *fake.register.borrow_mut() = vec![Err(Failure("taken".to_owned()))];
        let app = App {
            registration: Some(RegistrationScreen::new("need a name".to_owned())),
            registration_submission: Some("name".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_registration_action(app, &fake);

        assert!(!handled);
        assert_eq!(
            app.registration
                .expect("registration screen stays open")
                .problem(),
            "failed: taken"
        );
    }

    #[test]
    fn nothing_pending_for_registration_is_unhandled() {
        let fake = Fake::default();
        let app = App::default();

        let (_app, handled) = handle_registration_action(app, &fake);

        assert!(!handled);
    }

    #[test]
    fn a_successful_registration_loads_the_fresh_queue() {
        let fake = Fake::default();
        *fake.register.borrow_mut() = vec![Ok(empty_queue())];
        let app = App {
            registration_submission: Some("name".to_owned()),
            ..App::default()
        };

        let (_app, handled) = handle_registration_action(app, &fake);

        assert!(handled);
    }

    #[test]
    fn a_refused_run_shows_the_error_types_own_message_not_a_pre_rendered_one() {
        let text = super::run_result_text(Result::<RunReport, _>::Err(Failure(
            "a run is already in progress".to_owned(),
        )));

        assert_eq!(text, "failed: a run is already in progress");
    }

    #[test]
    fn a_finished_run_shows_the_report_built_from_the_typed_value() {
        let text = super::run_result_text(Result::<_, Failure>::Ok(RunReport {
            attempted: vec![],
            end: RunEnd::NothingPending,
        }));

        assert_eq!(text, "nothing is pending");
    }
}
