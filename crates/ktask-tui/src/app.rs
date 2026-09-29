//! The state of the terminal interface and how events change it.

use ktask_core::{
    AppendError, CancelError, Placement, Project, QueueView, SettingView, TaskDraft, TaskId,
    TaskStatus,
};
use ratatui::crossterm::event::KeyCode;

use crate::form::Form;
use crate::import_form::ImportForm;
use crate::registration_form::RegistrationForm;
use crate::settings_form::SettingsForm;

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
    /// A key that named a task was refused at once, without asking or opening anything: shown
    /// until the next key, or until the condition that raised it no longer holds.
    pub refused: Option<Refusal>,
    /// The task whose removal was confirmed: the loop carries it out and clears this.
    pub removal: Option<TaskId>,
    /// The form a new task is written in, while it is open; it covers the queue.
    pub(crate) form: Option<Form>,
    /// The task the form was submitted with and where it goes: the loop adds it and answers
    /// with [`Event::Added`] or [`Event::Rejected`], and clears this.
    pub submission: Option<(TaskDraft, Placement)>,
    /// Set when the operator asked to start executing the pending tasks: the loop starts a
    /// run, exactly as `ktask-rs run` does, and clears this.
    pub run_requested: Option<()>,
    /// The path a file of tasks is imported from, while it is being written; it covers the
    /// queue like the task form does.
    pub(crate) import: Option<ImportForm>,
    /// The path the import form was submitted with, for the loop to import, and clears this.
    pub import_submission: Option<String>,
    /// The result of the last thing this screen started that prints its own report — a run,
    /// or an import — the same words `ktask-rs run` or `ktask-rs import` itself would print,
    /// one per line, whether it succeeded, was refused, or (for a run) stopped partway —
    /// shown in place of the task list until a key that is not one of the ones that scroll it
    /// dismisses it.
    pub message: Option<Vec<String>>,
    /// The first of `message`'s lines shown, when there are more than fit; reset to `0`
    /// whenever a fresh message arrives.
    pub message_offset: usize,
    /// The settings screen, while it is open; it covers the queue like the task form does.
    pub(crate) settings: Option<SettingsForm>,
    /// Set when the operator asked to open the settings screen: the loop loads the
    /// project's settings and opens it with [`Event::SettingsLoaded`], and clears this.
    pub settings_requested: Option<()>,
    /// The settings screen's focused field, submitted as its setting's name and its value
    /// typed, for the loop to save, and clears this.
    pub setting_submission: Option<(&'static str, String)>,
    /// The registered projects, while the picker that lets the operator work on another
    /// one's queue is open; it covers the queue like the task form does — the same list
    /// `ktask-rs project list` prints, from the same use case.
    pub(crate) projects: Option<Vec<Project>>,
    /// The index into `projects` the picker's selection is on.
    pub(crate) project_selection: usize,
    /// Why the picker's last submission switched nothing; shown until the next one.
    pub(crate) project_problem: Option<String>,
    /// Set when the operator asked to open the project picker: the loop loads the registered
    /// projects and opens it with [`Event::ProjectsLoaded`], and clears this.
    pub projects_requested: Option<()>,
    /// The name of the project the picker was submitted with, for the loop to switch to, and
    /// clears this.
    pub project_switch: Option<String>,
    /// While the picker is open, the name of the project `d` there asked to forget: the picker
    /// shows this question in place of the list, and the answer is the next key.
    pub(crate) project_forgetting: Option<String>,
    /// The name of the project the picker's forget question was confirmed with, for the loop to
    /// forget — the same outcome `ktask-rs project forget --yes <NAME>` gives for the same
    /// name — and clears this.
    pub project_forget: Option<String>,
    /// The refusal that made a name necessary to register the current directory under, and the
    /// name being typed for it, while this is open. It is the whole screen — there is no
    /// project resolved yet, so no queue to show behind it — and it is only ever open before
    /// the first queue is loaded.
    pub(crate) registration: Option<RegistrationForm>,
    /// The name the registration screen was submitted with, for the loop to register the
    /// current directory under, and clears this.
    pub registration_submission: Option<String>,
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

/// Why [`App::refused`] holds what it does: the same reason, in the same words, that
/// `ktask-rs remove` or `ktask-rs add` gives for the same situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `d` on the selected task, which is running: `ktask-rs remove` would refuse it too.
    Running(TaskId),
    /// `d` on the selected task, which is cancelled already: `ktask-rs remove` would refuse
    /// it too.
    AlreadyCancelled(TaskId),
    /// `o` or `O` next to the selected task, which is cancelled: `ktask-rs add` would refuse
    /// a task placed next to it too.
    NextToCancelled(TaskId),
}

impl Refusal {
    /// The message shown for this refusal, word for word what the CLI command it mirrors
    /// would print.
    pub(crate) fn message(self) -> String {
        match self {
            Self::Running(id) => CancelError::Running(id).to_string(),
            Self::AlreadyCancelled(id) => CancelError::AlreadyCancelled(id).to_string(),
            Self::NextToCancelled(id) => AppendError::CancelledTask(id).to_string(),
        }
    }
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
    /// The run this screen started has ended, or could not start, printing this — the same
    /// words `ktask-rs run` itself would show.
    RunMessage(String),
    /// The import this screen started has finished, printing this — the same words
    /// `ktask-rs import` itself would show, whether it succeeded or was refused.
    ImportMessage(String),
    /// The project's settings were loaded: the settings screen opens on these values.
    SettingsLoaded(Vec<SettingView>),
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
    /// and gives the registered projects that remain, the same list `ktask-rs project list`
    /// prints afterward; the picker stays open, showing them.
    ProjectForgotten(Vec<Project>),
    /// Forgetting the project the picker's question named did not happen, for this reason — the
    /// same words `ktask-rs project forget` gives for the same conflict: the picker stays open
    /// and shows it.
    ProjectForgetFailed(String),
    /// The current directory was registered under the name the registration screen was
    /// submitted with: its queue opens, exactly as if it had been loaded from the start.
    Registered(QueueView),
    /// The registration screen's submission registered nothing, for this reason — the same
    /// words `ktask-rs project register --name` gives for the same conflict: it stays open,
    /// keeps what was typed, and shows it.
    RegistrationFailed(String),
}

/// The app after `event` happened to `app`. `Loaded` and a first Ctrl-C are handled here,
/// unconditionally, ahead of everything else that depends on which screen is open.
#[must_use]
pub fn update(app: App, event: Event) -> App {
    match event {
        Event::Loaded(queue) => return update_loaded(app, queue),
        Event::Ctrl('c') => return ctrl_c(app),
        _ => {}
    }
    match update_overlay_event(app, event) {
        Overlay::Handled(app) => app,
        Overlay::Unhandled(app, event) => update_rest(app, event),
    }
}

/// What [`update_overlay_event`] made of an event: the app it produced, or, when the event was
/// not one the settings screen or the project picker owns, the app and the event handed back
/// unhandled for [`update_rest`] to try in its turn.
enum Overlay {
    Handled(App),
    Unhandled(App, Event),
}

/// The app after `event`, when it belongs to the settings screen, the project picker or the
/// registration screen — opening one of them, a key pressed while one is open, or how one's
/// submission came back.
fn update_overlay_event(app: App, event: Event) -> Overlay {
    match update_settings_or_projects(app, event) {
        Overlay::Handled(app) => Overlay::Handled(app),
        Overlay::Unhandled(app, event) => update_registration_overlay(app, event),
    }
}

/// The part of [`update_overlay_event`] that belongs to the settings screen or the project
/// picker, handing the event back unhandled when it belongs to neither.
fn update_settings_or_projects(app: App, event: Event) -> Overlay {
    match event {
        Event::Key(key) if app.settings.is_some() => {
            Overlay::Handled(update_settings_key(app, key))
        }
        Event::Ctrl(letter) if app.settings.is_some() => {
            Overlay::Handled(update_settings_ctrl(app, letter))
        }
        Event::SettingsLoaded(views) => Overlay::Handled(App {
            settings: Some(SettingsForm::new(&views)),
            ..app
        }),
        Event::SettingSaved => Overlay::Handled(App {
            settings: None,
            ..app
        }),
        Event::SettingRejected(message) => {
            Overlay::Handled(in_settings(app, |form| SettingsForm {
                problem: Some(message),
                ..form
            }))
        }
        Event::Key(key) if app.projects.is_some() => {
            Overlay::Handled(update_projects_key(app, key))
        }
        Event::ProjectsLoaded(projects) => Overlay::Handled(open_projects(app, projects)),
        Event::ProjectSwitched(queue) => Overlay::Handled(project_switched(app, queue)),
        Event::ProjectSwitchFailed(message) => Overlay::Handled(App {
            project_problem: Some(message),
            ..app
        }),
        Event::ProjectForgotten(projects) => Overlay::Handled(project_forgotten(app, projects)),
        Event::ProjectForgetFailed(message) => Overlay::Handled(App {
            project_forgetting: None,
            project_problem: Some(message),
            ..app
        }),
        other => Overlay::Unhandled(app, other),
    }
}

/// The part of [`update_overlay_event`] that belongs to the registration screen, handing the
/// event back unhandled when it belongs to none of the overlays.
fn update_registration_overlay(app: App, event: Event) -> Overlay {
    match event {
        Event::Key(key) if app.registration.is_some() => {
            Overlay::Handled(update_registration_key(app, key))
        }
        Event::Ctrl(letter) if app.registration.is_some() => {
            Overlay::Handled(update_registration_ctrl(app, letter))
        }
        Event::Registered(queue) => Overlay::Handled(registered(app, queue)),
        Event::RegistrationFailed(problem) => Overlay::Handled(in_registration(app, |form| {
            RegistrationForm { problem, ..form }
        })),
        other => Overlay::Unhandled(app, other),
    }
}

/// The app after `event`, once it is known to be neither `Loaded`, a first Ctrl-C, nor one
/// [`update_overlay_event`] owns: a discard question's answer, a form's own keys, a
/// background action's result, or a plain key on the queue itself.
fn update_rest(app: App, event: Event) -> App {
    match event {
        Event::Key(key) if app.confirming == Some(Confirming::Discard) => {
            update_discard_key(app, key)
        }
        Event::Ctrl(_) if app.confirming == Some(Confirming::Discard) => app,
        Event::Key(key) if app.form.is_some() => update_form_key(app, key),
        Event::Ctrl(letter) if app.form.is_some() => update_form_ctrl(app, letter),
        Event::Key(key) if app.import.is_some() => update_import_key(app, key),
        Event::Ctrl(letter) if app.import.is_some() => update_import_ctrl(app, letter),
        Event::Added(id) => added(app, id),
        Event::Rejected(problems) => in_form(app, |form| Form { problems, ..form }),
        Event::RunMessage(text) | Event::ImportMessage(text) => shown_message(app, &text),
        Event::Key(KeyCode::Char('q')) => App { quit: true, ..app },
        Event::Key(key) if app.message.is_some() => update_message_key(app, key),
        Event::Key(key) if app.confirming.is_some() => update_removal_confirm_key(app, key),
        Event::Key(key) if app.help => update_help_key(app, key),
        Event::Key(key) => update_queue_key(app, key),
        _ => app,
    }
}

/// The app after the queue is (re)loaded: keeps the selection, a pending removal confirmation
/// and a refusal only as long as they still make sense against the fresh queue.
fn update_loaded(app: App, queue: QueueView) -> App {
    let selected = reselect(&app, &queue);
    let confirming = app.confirming.filter(|confirm| match confirm {
        Confirming::Removal(id) => removable(&queue, *id),
        Confirming::Discard => true,
    });
    let refused = app.refused.filter(|refusal| match refusal {
        Refusal::Running(id) => is_running(&queue, *id),
        Refusal::AlreadyCancelled(id) | Refusal::NextToCancelled(id) => cancelled(&queue, *id),
    });
    App {
        queue: Some(queue),
        selected,
        confirming,
        refused,
        ..app
    }
}

/// A key while the settings screen is open.
fn update_settings_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Esc => App {
            settings: None,
            ..app
        },
        KeyCode::Tab => in_settings(app, |form| form.moved(true)),
        KeyCode::BackTab => in_settings(app, |form| form.moved(false)),
        _ => in_settings(app, |form| form.press(key)),
    }
}

/// A Ctrl-letter while the settings screen is open.
fn update_settings_ctrl(app: App, letter: char) -> App {
    match letter {
        's' => submit_setting(app),
        _ => app,
    }
}

/// A key while the project picker is open: while it is asking to confirm forgetting a project,
/// only that question's own keys answer.
fn update_projects_key(app: App, key: KeyCode) -> App {
    if app.project_forgetting.is_some() {
        return update_forget_confirm_key(app, key);
    }
    match key {
        KeyCode::Esc => App {
            projects: None,
            project_problem: None,
            ..app
        },
        KeyCode::Char('j') | KeyCode::Down => move_project_selection(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_project_selection(app, -1),
        KeyCode::Enter => submit_project_switch(app),
        KeyCode::Char('d') => ask_forget_selected(app),
        _ => app,
    }
}

/// The name of the picker's selected project, when there is one.
fn selected_project_name(app: &App) -> Option<String> {
    app.projects
        .as_ref()
        .and_then(|projects| projects.get(app.project_selection))
        .map(|project| project.name.clone())
}

/// The app with the picker asking to confirm forgetting its selected project.
fn ask_forget_selected(app: App) -> App {
    let Some(name) = selected_project_name(&app) else {
        return app;
    };
    App {
        project_forgetting: Some(name),
        project_problem: None,
        ..app
    }
}

/// A key while the picker is asking to confirm forgetting the project it named.
fn update_forget_confirm_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Char('y') => App {
            project_forget: app.project_forgetting.clone(),
            project_forgetting: None,
            ..app
        },
        KeyCode::Char('n') | KeyCode::Esc => App {
            project_forgetting: None,
            ..app
        },
        _ => app,
    }
}

/// The app once the picker's forget question is carried out: the fresh registered projects
/// replace the ones it showed, the selection stays inside them, and the question closes.
fn project_forgotten(app: App, projects: Vec<Project>) -> App {
    let last = projects.len().saturating_sub(1);
    App {
        project_selection: app.project_selection.min(last),
        projects: Some(projects),
        project_forgetting: None,
        project_problem: None,
        ..app
    }
}

/// The picker's selection moved by `delta`, staying inside the list.
fn move_project_selection(app: App, delta: isize) -> App {
    let Some(last) = app.projects.as_ref().and_then(|p| p.len().checked_sub(1)) else {
        return app;
    };
    App {
        project_selection: app.project_selection.saturating_add_signed(delta).min(last),
        ..app
    }
}

/// The app with the picker's selected project's name left for the loop to switch to.
fn submit_project_switch(app: App) -> App {
    let Some(name) = app
        .projects
        .as_ref()
        .and_then(|projects| projects.get(app.project_selection))
        .map(|project| project.name.clone())
    else {
        return app;
    };
    App {
        project_switch: Some(name),
        ..app
    }
}

/// The app with the project picker open on `projects`, the selection on the one whose queue
/// is currently shown, or the first when none matches.
fn open_projects(app: App, projects: Vec<Project>) -> App {
    let current = app.queue.as_ref().map(|queue| queue.project.name.as_str());
    let selection = projects
        .iter()
        .position(|project| Some(project.name.as_str()) == current)
        .unwrap_or(0);
    App {
        projects: Some(projects),
        project_selection: selection,
        project_problem: None,
        ..app
    }
}

/// The app once the picker's submission switched the project: its queue replaces the one on
/// show — with nothing carried over from the old project's screen, since a task ID there means
/// nothing in this one — and the picker closes.
fn project_switched(app: App, queue: QueueView) -> App {
    update_loaded(
        App {
            projects: None,
            project_problem: None,
            queue: None,
            selected: None,
            show_cancelled: false,
            confirming: None,
            refused: None,
            ..app
        },
        queue,
    )
}

/// A key while the registration screen is open: any key but Esc is typed into the name field.
/// There is no queue behind this screen to fall back to, so unlike the other overlays, Esc does
/// not close it back onto one — it quits, the same as `q` would elsewhere.
fn update_registration_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Esc => App { quit: true, ..app },
        _ => in_registration(app, |form| form.press(key)),
    }
}

/// A Ctrl-letter while the registration screen is open.
fn update_registration_ctrl(app: App, letter: char) -> App {
    match letter {
        's' => submit_registration(app),
        _ => app,
    }
}

/// The app with `change` made to the registration screen, if it is open.
fn in_registration(app: App, change: impl FnOnce(RegistrationForm) -> RegistrationForm) -> App {
    App {
        registration: app.registration.map(change),
        ..app
    }
}

/// The app with the registration screen's typed name left for the loop to register the current
/// directory under.
fn submit_registration(app: App) -> App {
    let name = app
        .registration
        .as_ref()
        .map(|form| form.name.text())
        .unwrap_or_default();
    App {
        registration_submission: Some(name),
        ..app
    }
}

/// The app once the current directory is registered: the registration screen closes and its
/// queue shows, exactly as if it had been loaded from the start.
fn registered(app: App, queue: QueueView) -> App {
    update_loaded(
        App {
            registration: None,
            ..app
        },
        queue,
    )
}

/// A key while a first Ctrl-C is asking whether to discard the open form.
fn update_discard_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Char('y') => App { quit: true, ..app },
        KeyCode::Char('n') | KeyCode::Esc => App {
            confirming: None,
            ..app
        },
        _ => app,
    }
}

/// A key while the task form is open.
fn update_form_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Esc => App { form: None, ..app },
        KeyCode::Tab => in_form(app, |form| form.moved(true)),
        KeyCode::BackTab => in_form(app, |form| form.moved(false)),
        _ => in_form(app, |form| form.press(key)),
    }
}

/// A Ctrl-letter while the task form is open.
fn update_form_ctrl(app: App, letter: char) -> App {
    match letter {
        's' => submit(app),
        'n' => in_form(app, Form::with_criterion),
        'd' => in_form(app, Form::without_criterion),
        _ => app,
    }
}

/// A key while the import form is open.
fn update_import_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Esc => App {
            import: None,
            ..app
        },
        _ => in_import(app, |form| form.press(key)),
    }
}

/// A Ctrl-letter while the import form is open.
fn update_import_ctrl(app: App, letter: char) -> App {
    match letter {
        's' => submit_import(app),
        _ => app,
    }
}

/// A key while removing the selected task is being confirmed.
fn update_removal_confirm_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Char('y') => confirm_removal(app),
        KeyCode::Char('n') | KeyCode::Esc => App {
            confirming: None,
            ..app
        },
        _ => app,
    }
}

/// A key while the help screen is open.
fn update_help_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Esc | KeyCode::Char('?') => App { help: false, ..app },
        _ => app,
    }
}

/// The app once a run or an import this screen started has finished, showing `text` — its own
/// report, the same words `ktask-rs run` or `ktask-rs import` itself would print — in place
/// of the task list.
fn shown_message(app: App, text: &str) -> App {
    App {
        message: Some(text.lines().map(str::to_owned).collect()),
        message_offset: 0,
        ..app
    }
}

/// A key while the last run's or import's results are shown in place of the task list:
/// `j`/`Down` and `k`/`Up` scroll one line, `g`/`G` jump to the first or last line, and any
/// other key dismisses the results, then is handled as it would be on the plain queue screen
/// — so, for example, `r` both dismisses a shown message and starts a fresh run.
fn update_message_key(app: App, key: KeyCode) -> App {
    match key {
        KeyCode::Char('j') | KeyCode::Down => scroll_message(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll_message(app, -1),
        KeyCode::Char('g') => App {
            message_offset: 0,
            ..app
        },
        KeyCode::Char('G') => App {
            message_offset: last_message_line(&app),
            ..app
        },
        _ => update_queue_key(
            App {
                message: None,
                message_offset: 0,
                ..app
            },
            key,
        ),
    }
}

/// The index of `app.message`'s last line, or `0` when there is none.
fn last_message_line(app: &App) -> usize {
    app.message
        .as_ref()
        .map_or(0, |lines| lines.len().saturating_sub(1))
}

/// `app.message_offset` moved by `delta`, clamped to stay within the message's lines.
fn scroll_message(app: App, delta: isize) -> App {
    let offset = app
        .message_offset
        .saturating_add_signed(delta)
        .min(last_message_line(&app));
    App {
        message_offset: offset,
        ..app
    }
}

/// A key on the plain queue screen: no form, import form, settings, confirmation, help or
/// message in the way.
fn update_queue_key(app: App, key: KeyCode) -> App {
    // `refused` is a one-shot notice: any key past the one that raised it dismisses it,
    // whether or not that key is `d` again.
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
        KeyCode::Char('j') | KeyCode::Down => select(app, |index, _| index.saturating_add(1)),
        KeyCode::Char('k') | KeyCode::Up => select(app, |index, _| index.saturating_sub(1)),
        KeyCode::Char('n') => open_form(app, Placement::End),
        KeyCode::Char('o') => open_form_next_to(app, Placement::After),
        KeyCode::Char('O') => open_form_next_to(app, Placement::Before),
        KeyCode::Char('d') => press_d(app),
        KeyCode::Char('r') => App {
            run_requested: Some(()),
            ..app
        },
        KeyCode::Char('i') => App {
            import: Some(ImportForm::new()),
            ..app
        },
        KeyCode::Char('s') => App {
            settings_requested: Some(()),
            ..app
        },
        KeyCode::Char('p') => App {
            projects_requested: Some(()),
            ..app
        },
        KeyCode::Char('g') => select(app, |_, _| 0),
        KeyCode::Char('G') => select(app, |_, len| len.saturating_sub(1)),
        _ => app,
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

/// The app with `change` made to the import form, if it is open.
fn in_import(app: App, change: impl FnOnce(ImportForm) -> ImportForm) -> App {
    App {
        import: app.import.map(change),
        ..app
    }
}

/// The app with the import form's path left for the loop to import, and the form closed.
fn submit_import(app: App) -> App {
    let path = app
        .import
        .as_ref()
        .map(|form| form.path.text())
        .unwrap_or_default();
    App {
        import: None,
        import_submission: Some(path),
        ..app
    }
}

/// The app with `change` made to the settings screen, if it is open.
fn in_settings(app: App, change: impl FnOnce(SettingsForm) -> SettingsForm) -> App {
    App {
        settings: app.settings.map(change),
        ..app
    }
}

/// The app with the settings screen's focused field left for the loop to save, if it is open.
fn submit_setting(app: App) -> App {
    let submission = app.settings.as_ref().and_then(|form| {
        let name = form.name()?;
        Some((name, form.value()))
    });
    App {
        setting_submission: submission,
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
/// `beside` says; at the end when nothing is selected. Refuses at once, without opening the
/// form, when the selected task is cancelled.
fn open_form_next_to(app: App, beside: fn(TaskId) -> Placement) -> App {
    let Some(id) = app.selected else {
        return open_form(app, Placement::End);
    };
    if app.queue.as_ref().is_some_and(|queue| cancelled(queue, id)) {
        return App {
            refused: Some(Refusal::NextToCancelled(id)),
            ..app
        };
    }
    open_form(app, beside(id))
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
    queue
        .tasks
        .iter()
        .any(|task| task.id == id && !cancelled(queue, id) && !is_running(queue, id))
}

/// Whether `queue` shows the task `id` as running.
fn is_running(queue: &QueueView, id: TaskId) -> bool {
    queue
        .tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Running)
}

/// Whether `queue` shows the task `id` as cancelled.
fn cancelled(queue: &QueueView, id: TaskId) -> bool {
    queue
        .tasks
        .iter()
        .any(|task| task.id == id && task.status == TaskStatus::Cancelled)
}

/// The app after `d` on the selected task: it asks to confirm removing it when it can be
/// removed, and otherwise refuses at once, naming why — running, or cancelled already —
/// without asking; with nothing selected, changes nothing.
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
            refused: Some(Refusal::Running(id)),
            ..app
        }
    } else if cancelled(queue, id) {
        App {
            refused: Some(Refusal::AlreadyCancelled(id)),
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
    fn d_with_nothing_selected_asks_nothing() {
        assert_eq!(press(loaded(&[]), &[KeyCode::Char('d')]).confirming, None);
    }

    #[test]
    fn d_on_a_running_task_refuses_without_asking_and_changes_nothing_else() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue.clone()));

        let app = press(app, &[KeyCode::Char('d')]);

        assert_eq!(app.confirming, None);
        assert_eq!(app.removal, None);
        assert_eq!(app.refused, Some(Refusal::Running(TaskId(1))));
        assert_eq!(app.queue, Some(queue));
    }

    #[test]
    fn d_on_a_cancelled_task_refuses_without_asking_and_changes_nothing_else() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Cancelled;
        let app = update(App::default(), Event::Loaded(queue.clone()));

        let app = press(app, &[KeyCode::Char('d')]);

        assert_eq!(app.confirming, None);
        assert_eq!(app.removal, None);
        assert_eq!(app.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));
        assert_eq!(
            Refusal::AlreadyCancelled(TaskId(1)).message(),
            "task 1 is already cancelled"
        );
        assert_eq!(app.queue, Some(queue));
    }

    #[test]
    fn the_refusal_is_dismissed_by_the_next_key_that_is_not_d_again() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue));
        let refused = press(app, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::Running(TaskId(1))));

        for key in [KeyCode::Char('j'), KeyCode::Char('x')] {
            assert_eq!(press(refused.clone(), &[key]).refused, None);
        }
        // The task is still running, so d again just shows the same refusal afresh.
        assert_eq!(
            press(refused, &[KeyCode::Char('d')]).refused,
            Some(Refusal::Running(TaskId(1)))
        );
    }

    #[test]
    fn the_running_refusal_goes_when_its_task_stops_running_from_elsewhere() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Running;
        let app = update(App::default(), Event::Loaded(queue));
        let refused = press(app, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::Running(TaskId(1))));

        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Done;
        let reloaded = update(refused, Event::Loaded(queue));
        assert_eq!(reloaded.refused, None);
    }

    #[test]
    fn the_cancelled_refusal_survives_a_reload_and_goes_when_the_task_is_gone() {
        let mut queue = queue_of(&[1, 2]);
        queue.tasks[0].status = TaskStatus::Cancelled;
        let app = update(App::default(), Event::Loaded(queue.clone()));
        let refused = press(app, &[KeyCode::Char('d')]);
        assert_eq!(refused.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));

        let reloaded = update(refused, Event::Loaded(queue));
        assert_eq!(reloaded.refused, Some(Refusal::AlreadyCancelled(TaskId(1))));

        let gone = update(reloaded, Event::Loaded(queue_of(&[2])));
        assert_eq!(gone.refused, None);
    }

    #[test]
    fn r_asks_the_loop_to_start_a_run_and_changes_nothing_else() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('j'), KeyCode::Char('r')]);
        assert_eq!(app.run_requested, Some(()));
        assert_eq!(on(&app), Some(2));
        assert_eq!(app.queue, Some(queue_of(&[1, 2])));
    }

    #[test]
    fn i_opens_the_import_form_on_an_empty_path_and_esc_closes_it_changing_nothing_else() {
        let before = loaded(&[1, 2]);
        let open = press(before.clone(), &[KeyCode::Char('i')]);
        assert_eq!(
            open.import.as_ref().map(|form| form.path.text()),
            Some(String::new())
        );
        assert_eq!(open.queue, before.queue);
        assert_eq!(press(open, &[KeyCode::Esc]), before);
    }

    #[test]
    fn while_the_import_form_is_open_letters_are_typed_and_no_queue_key_acts() {
        let open = press(loaded(&[1, 2]), &[KeyCode::Char('i')]);
        let app = typed(open, "qjdna?");
        assert!(!app.quit && !app.help && app.confirming.is_none() && app.import.is_some());
        assert_eq!(
            app.import.as_ref().map(|form| form.path.text()),
            Some("qjdna?".to_owned())
        );
        assert_eq!(on(&app), Some(1));
    }

    #[test]
    fn ctrl_s_submits_the_typed_path_and_closes_the_import_form() {
        let app = typed(
            press(loaded(&[1]), &[KeyCode::Char('i')]),
            "/tmp/tasks.json",
        );
        let app = update(app, Event::Ctrl('s'));
        assert_eq!(app.import, None);
        assert_eq!(app.import_submission, Some("/tmp/tasks.json".to_owned()));
    }

    #[test]
    fn an_import_message_is_shown_until_a_key_that_does_not_scroll_it_dismisses_it() {
        let app = update(loaded(&[1]), Event::ImportMessage("1\n2\n".to_owned()));
        assert_eq!(
            app.message.as_deref(),
            Some(["1".to_owned(), "2".to_owned()].as_slice())
        );
        assert_eq!(press(app.clone(), &[KeyCode::Char('x')]).message, None);
    }

    #[test]
    fn a_run_message_is_shown_until_a_key_that_does_not_scroll_it_dismisses_it() {
        let app = update(
            loaded(&[1]),
            Event::RunMessage("nothing is pending".to_owned()),
        );
        assert_eq!(
            app.message.as_deref(),
            Some(["nothing is pending".to_owned()].as_slice())
        );

        // `j`, `k`, the arrows, `g` and `G` scroll the message rather than dismiss it.
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            assert_eq!(
                press(app.clone(), &[key]).message,
                app.message,
                "{key:?} should scroll, not dismiss"
            );
        }

        assert_eq!(press(app.clone(), &[KeyCode::Char('x')]).message, None);
    }

    #[test]
    fn j_and_k_scroll_a_run_message_that_does_not_fit_and_g_and_shift_g_jump_to_its_ends() {
        let text = (1..=5)
            .map(|n| format!("task {n}: done"))
            .collect::<Vec<_>>()
            .join("\n");
        let app = update(loaded(&[1]), Event::RunMessage(text));
        assert_eq!(app.message_offset, 0);

        let scrolled = press(app.clone(), &[KeyCode::Char('j'), KeyCode::Char('j')]);
        assert_eq!(scrolled.message_offset, 2);
        assert_eq!(scrolled.message, app.message);

        let back = press(scrolled.clone(), &[KeyCode::Char('k')]);
        assert_eq!(back.message_offset, 1);

        let bottom = press(app.clone(), &[KeyCode::Char('G')]);
        assert_eq!(bottom.message_offset, 4);

        // Scrolling past either end holds at it rather than wrapping or panicking.
        let held = press(app.clone(), &[KeyCode::Char('k')]);
        assert_eq!(held.message_offset, 0);
        let past_bottom = press(bottom, &[KeyCode::Char('j')]);
        assert_eq!(past_bottom.message_offset, 4);

        let top = press(scrolled, &[KeyCode::Char('g')]);
        assert_eq!(top.message_offset, 0);
    }

    #[test]
    fn r_again_both_dismisses_a_shown_message_and_requests_a_fresh_run() {
        let app = update(
            loaded(&[1]),
            Event::RunMessage("nothing is pending".to_owned()),
        );
        let app = press(app, &[KeyCode::Char('r')]);
        assert_eq!(app.message, None);
        assert_eq!(app.run_requested, Some(()));
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
    fn o_and_capital_o_next_to_a_cancelled_task_refuse_at_once_without_opening_the_form() {
        let mut queue = queue_of(&[1, 2, 3]);
        queue.tasks[1].status = TaskStatus::Cancelled;
        for key in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let app = update(App::default(), Event::Loaded(queue.clone()));
            let app = press(app, &[KeyCode::Char('j'), key]);
            assert_eq!(app.refused, Some(Refusal::NextToCancelled(TaskId(2))));
            assert_eq!(
                Refusal::NextToCancelled(TaskId(2)).message(),
                "task 2 is cancelled"
            );
            assert!(app.form.is_none(), "{key:?} opened the form");
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
    fn s_asks_the_loop_to_load_the_settings_and_changes_nothing_else() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('j'), KeyCode::Char('s')]);
        assert_eq!(app.settings_requested, Some(()));
        assert_eq!(on(&app), Some(2));
        assert_eq!(app.settings, None);
    }

    #[test]
    fn settings_loaded_opens_the_screen_on_the_first_setting() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let settings = app.settings.as_ref().expect("the settings screen is open");
        assert_eq!(settings.name(), Some("attempt-timeout"));
        assert_eq!(settings.value(), "14400");
        assert!(settings.fields[0].is_default);
    }

    #[test]
    fn esc_closes_the_settings_screen_without_submitting_anything() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = press(app, &[KeyCode::Esc]);
        assert_eq!(app.settings, None);
        assert_eq!(app.setting_submission, None);
    }

    #[test]
    fn typing_in_the_settings_screen_edits_the_field_and_no_queue_key_acts() {
        let app = update(
            loaded(&[1, 2]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = press(
            app,
            &[
                KeyCode::Backspace,
                KeyCode::Backspace,
                KeyCode::Backspace,
                KeyCode::Backspace,
                KeyCode::Backspace,
                KeyCode::Char('6'),
                KeyCode::Char('0'),
                KeyCode::Char('q'),
                KeyCode::Char('d'),
            ],
        );
        assert_eq!(app.settings.as_ref().expect("still open").value(), "60qd");
        assert!(!app.quit);
        assert_eq!(app.confirming, None);
    }

    #[test]
    fn tab_and_shift_tab_move_the_focus_between_settings_and_wrap() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = press(app, &[KeyCode::Tab]);
        assert_eq!(app.settings.as_ref().unwrap().name(), Some("health-check"));
        let app = press(app, &[KeyCode::Tab]);
        assert_eq!(
            app.settings.as_ref().unwrap().name(),
            Some("attempt-timeout")
        );
        let app = press(app, &[KeyCode::BackTab]);
        assert_eq!(app.settings.as_ref().unwrap().name(), Some("health-check"));
    }

    #[test]
    fn typing_after_tab_edits_the_health_check_field_leaving_the_timeout_untouched() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = press(app, &[KeyCode::Tab]);
        let app = "cargo test"
            .chars()
            .fold(app, |app, c| update(app, Event::Key(KeyCode::Char(c))));
        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.value(), "cargo test");
        assert_eq!(settings.fields[0].text.text(), "14400");
    }

    #[test]
    fn ctrl_s_submits_the_focused_fields_name_and_value_and_leaves_the_screen_open() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = update(app, Event::Ctrl('s'));
        assert_eq!(
            app.setting_submission,
            Some(("attempt-timeout", "14400".to_owned()))
        );
        assert!(app.settings.is_some());

        let app = press(app, &[KeyCode::Tab]);
        let app = update(app, Event::Ctrl('s'));
        assert_eq!(
            app.setting_submission,
            Some(("health-check", String::new()))
        );
    }

    #[test]
    fn setting_saved_closes_the_screen() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = update(app, Event::SettingSaved);
        assert_eq!(app.settings, None);
    }

    #[test]
    fn setting_rejected_keeps_the_screen_open_with_its_value_and_shows_why() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        let app = press(app, &[KeyCode::Char('x')]);
        let app = update(app, Event::SettingRejected("not a number".to_owned()));
        let settings = app.settings.as_ref().expect("still open");
        assert_eq!(settings.problem.as_deref(), Some("not a number"));
        assert_eq!(settings.value(), "14400x");
    }

    #[test]
    fn ctrl_c_quits_from_the_settings_screen() {
        let app = update(
            loaded(&[1]),
            Event::SettingsLoaded(settings_views("14400", true)),
        );
        assert!(update(app, Event::Ctrl('c')).quit);
    }

    fn other_project(name: &str) -> Project {
        Project {
            name: name.to_owned(),
            path: PathBuf::from(format!("/work/{name}")),
            registered_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn projects() -> Vec<Project> {
        vec![other_project("app"), other_project("other")]
    }

    fn other_queue_of(name: &str, ids: &[u64]) -> QueueView {
        QueueView {
            project: other_project(name),
            summary: StatusSummary::default(),
            tasks: ids.iter().map(|id| task(*id)).collect(),
            attempts: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn p_asks_the_loop_to_load_the_registered_projects_and_changes_nothing_else() {
        let app = press(loaded(&[1, 2]), &[KeyCode::Char('j'), KeyCode::Char('p')]);
        assert_eq!(app.projects_requested, Some(()));
        assert_eq!(on(&app), Some(2));
        assert_eq!(app.projects, None);
    }

    #[test]
    fn projects_loaded_opens_the_picker_with_the_current_project_selected() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        assert_eq!(app.projects, Some(projects()));
        assert_eq!(app.project_selection, 0);
    }

    #[test]
    fn esc_closes_the_picker_without_switching_anything() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = press(app, &[KeyCode::Esc]);
        assert_eq!(app.projects, None);
        assert_eq!(app.project_switch, None);
    }

    #[test]
    fn j_and_k_move_the_selection_and_stay_inside_the_list() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = press(app, &[KeyCode::Char('j'), KeyCode::Char('j')]);
        assert_eq!(app.project_selection, 1);
        let app = press(app, &[KeyCode::Char('k'), KeyCode::Char('k')]);
        assert_eq!(app.project_selection, 0);
    }

    #[test]
    fn while_the_picker_is_open_only_its_own_keys_and_ctrl_c_are_heard() {
        let open = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        for key in [KeyCode::Char('q'), KeyCode::Char('a'), KeyCode::Char('?')] {
            assert_eq!(press(open.clone(), &[key]), open);
        }
        assert!(update(open, Event::Ctrl('c')).quit);
    }

    #[test]
    fn enter_submits_the_selected_projects_name_and_leaves_the_picker_open() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = press(app, &[KeyCode::Char('j'), KeyCode::Enter]);
        assert_eq!(app.project_switch, Some("other".to_owned()));
        assert!(app.projects.is_some());
    }

    #[test]
    fn project_switched_replaces_the_queue_and_closes_the_picker_selecting_the_first_task() {
        let app = press(loaded(&[7, 8]), &[KeyCode::Char('G'), KeyCode::Char('p')]);
        let app = press(app, &[KeyCode::Enter]);
        let app = update(
            app,
            Event::ProjectSwitched(other_queue_of("other", &[1, 2])),
        );
        assert_eq!(app.projects, None);
        assert_eq!(app.queue, Some(other_queue_of("other", &[1, 2])));
        assert_eq!(on(&app), Some(1));
        assert!(!app.show_cancelled);
    }

    #[test]
    fn project_switch_failed_keeps_the_picker_open_and_shows_why() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = update(app, Event::ProjectSwitchFailed("cannot open it".to_owned()));
        assert_eq!(app.project_problem.as_deref(), Some("cannot open it"));
        assert!(app.projects.is_some());
    }

    #[test]
    fn d_in_the_picker_asks_to_confirm_forgetting_the_selected_project() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = press(app, &[KeyCode::Char('j'), KeyCode::Char('d')]);
        assert_eq!(app.project_forgetting.as_deref(), Some("other"));
        assert_eq!(app.project_forget, None);
        assert!(app.projects.is_some());
    }

    #[test]
    fn while_the_forget_question_is_open_only_y_n_and_esc_answer_it() {
        let asked = press(
            update(loaded(&[1]), Event::ProjectsLoaded(projects())),
            &[KeyCode::Char('d')],
        );
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Enter,
            KeyCode::Char('q'),
        ] {
            assert_eq!(press(asked.clone(), &[key]), asked);
        }
    }

    #[test]
    fn y_confirms_the_forget_question_leaving_the_name_for_the_loop_to_forget() {
        let app = update(loaded(&[1]), Event::ProjectsLoaded(projects()));
        let app = press(app, &[KeyCode::Char('d'), KeyCode::Char('y')]);
        assert_eq!(app.project_forget, Some("app".to_owned()));
        assert_eq!(app.project_forgetting, None);
        assert!(app.projects.is_some());
    }

    #[test]
    fn n_and_esc_drop_the_forget_question_and_forget_nothing() {
        let asked = press(
            update(loaded(&[1]), Event::ProjectsLoaded(projects())),
            &[KeyCode::Char('d')],
        );
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            let app = press(asked.clone(), &[key]);
            assert_eq!(app.project_forgetting, None);
            assert_eq!(app.project_forget, None);
            assert!(app.projects.is_some());
        }
    }

    #[test]
    fn project_forgotten_shows_the_fresh_list_and_closes_the_question() {
        let app = press(
            update(loaded(&[1]), Event::ProjectsLoaded(projects())),
            &[KeyCode::Char('d'), KeyCode::Char('y')],
        );
        // The loop takes `project_forget` before handing the outcome back as an event, the
        // same as it does for `project_switch`; `update` itself never clears it.
        let app = App {
            project_forget: None,
            ..app
        };
        let remaining = vec![other_project("other")];
        let app = update(app, Event::ProjectForgotten(remaining.clone()));
        assert_eq!(app.projects, Some(remaining));
        assert_eq!(app.project_forgetting, None);
        assert_eq!(app.project_forget, None);
        assert_eq!(app.project_selection, 0);
    }

    #[test]
    fn project_forgotten_clamps_the_selection_when_it_ran_past_the_fresh_list() {
        let app = press(
            update(loaded(&[1]), Event::ProjectsLoaded(projects())),
            &[KeyCode::Char('j'), KeyCode::Char('d'), KeyCode::Char('y')],
        );
        let remaining = vec![other_project("app")];
        let app = update(app, Event::ProjectForgotten(remaining));
        assert_eq!(app.project_selection, 0);
    }

    #[test]
    fn project_forget_failed_keeps_the_picker_open_and_shows_why() {
        let app = press(
            update(loaded(&[1]), Event::ProjectsLoaded(projects())),
            &[KeyCode::Char('d'), KeyCode::Char('y')],
        );
        let app = update(
            app,
            Event::ProjectForgetFailed("unknown project".to_owned()),
        );
        assert_eq!(app.project_problem.as_deref(), Some("unknown project"));
        assert_eq!(app.project_forgetting, None);
        assert!(app.projects.is_some());
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

    fn awaiting_registration(problem: &str) -> App {
        App {
            registration: Some(RegistrationForm::new(problem.to_owned())),
            ..App::default()
        }
    }

    fn registration_of(app: &App) -> &RegistrationForm {
        app.registration
            .as_ref()
            .expect("the registration screen is open")
    }

    #[test]
    fn opening_on_a_name_conflict_shows_it_with_an_empty_name_and_no_queue() {
        let app = awaiting_registration(
            "cannot register /work/app as project \"app\": \
            that name is already registered for /elsewhere/app",
        );
        assert_eq!(
            registration_of(&app).problem,
            "cannot register /work/app as project \"app\": that name is already registered for \
             /elsewhere/app"
        );
        assert_eq!(registration_of(&app).name.text(), "");
        assert_eq!(app.queue, None);
    }

    #[test]
    fn typing_edits_the_name_and_no_queue_key_acts() {
        let app = typed(awaiting_registration("conflict"), "my-app-two");
        assert_eq!(registration_of(&app).name.text(), "my-app-two");
        assert!(!app.quit);
        assert_eq!(app.queue, None);
    }

    #[test]
    fn ctrl_s_submits_the_typed_name_and_leaves_the_screen_open() {
        let app = typed(awaiting_registration("conflict"), "my-app-two");
        let app = update(app, Event::Ctrl('s'));
        assert_eq!(app.registration_submission, Some("my-app-two".to_owned()));
        assert!(app.registration.is_some());
    }

    #[test]
    fn esc_quits_since_there_is_no_queue_to_fall_back_to() {
        let app = press(awaiting_registration("conflict"), &[KeyCode::Esc]);
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_quits_from_the_registration_screen() {
        let app = update(awaiting_registration("conflict"), Event::Ctrl('c'));
        assert!(app.quit);
    }

    #[test]
    fn registration_failed_keeps_the_screen_open_with_the_name_and_shows_why() {
        let app = typed(awaiting_registration("conflict"), "taken");
        let app = update(
            app,
            Event::RegistrationFailed("that name is taken too".to_owned()),
        );
        let form = registration_of(&app);
        assert_eq!(form.problem, "that name is taken too");
        assert_eq!(form.name.text(), "taken");
    }

    #[test]
    fn registered_closes_the_screen_and_opens_the_fresh_queue() {
        let app = awaiting_registration("conflict");
        let app = update(app, Event::Registered(queue_of(&[1, 2])));
        assert_eq!(app.registration, None);
        assert_eq!(app.queue, Some(queue_of(&[1, 2])));
        assert_eq!(on(&app), Some(1));
    }
}
