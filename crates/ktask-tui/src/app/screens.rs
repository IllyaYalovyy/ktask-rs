//! Trying an event against each screen that might own it, and applying what an owning screen
//! asks for back onto the [`App`].

use ktask_core::Project;

use crate::answer_screen::{self, AnswerScreen};
use crate::import_screen::{self, ImportScreen};
use crate::projects::{self, ProjectsScreen};
use crate::queue::Queue;
use crate::registration_screen::{self, RegistrationScreen};
use crate::settings::{self, SettingsScreen};
use crate::task_form::{self, TaskFormScreen};

use super::{App, Event, Tried, added, handled};

/// `event`, applied to the settings screen when it owns it — a key or Ctrl-letter while it is
/// open, or how its own load, save or refusal came back — or handed back for the next screen
/// to try, unchanged.
pub(super) fn try_settings(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.settings.is_some() => handled(on_settings(app, |s| s.key(key))),
        Event::Ctrl(letter) if app.settings.is_some() => {
            handled(on_settings(app, |s| s.ctrl(letter)))
        }
        Event::SettingsLoaded(views) => handled(App {
            settings: Some(SettingsScreen::new(&views)),
            ..app
        }),
        Event::SettingSaved => handled(App {
            settings: None,
            ..app
        }),
        Event::SettingRejected(message) => handled(App {
            settings: app.settings.map(|s| s.rejected(message)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `event`, applied to the task form when it owns it — a key or Ctrl-letter while it is open,
/// or how its own submission came back — or handed back for the next screen to try, unchanged.
pub(super) fn try_form(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.form.is_some() => handled(on_form(app, |f| f.key(key))),
        Event::Ctrl(letter) if app.form.is_some() => handled(on_form(app, |f| f.ctrl(letter))),
        Event::Added(id) => handled(added(app, id)),
        Event::Rejected(problems) => handled(App {
            form: app.form.map(|f| f.rejected(problems)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `event`, applied to the answer form when it owns it — a key or Ctrl-letter while it is
/// open — or handed back for the next screen to try, unchanged.
pub(super) fn try_answer(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.answer.is_some() => handled(on_answer(app, |a| a.key(key))),
        Event::Ctrl(letter) if app.answer.is_some() => handled(on_answer(app, |a| a.ctrl(letter))),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `event`, applied to the import form when it owns it — a key or Ctrl-letter while it is
/// open — or handed back for the next screen to try, unchanged.
pub(super) fn try_import(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.import.is_some() => handled(on_import(app, |i| i.key(key))),
        Event::Ctrl(letter) if app.import.is_some() => handled(on_import(app, |i| i.ctrl(letter))),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `event`, applied to the project picker when it owns it — a key while it is open, or how
/// its own load, switch or forget came back — or handed back for the next screen to try,
/// unchanged.
pub(super) fn try_projects(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.projects.is_some() => handled(on_projects(app, |p| p.key(key))),
        Event::ProjectsLoaded(projects) => handled(open_projects(app, projects)),
        Event::ProjectSwitched(view) => handled(App {
            projects: None,
            queue: Queue::replaced(view),
            ..app
        }),
        Event::ProjectSwitchFailed(message) => handled(App {
            projects: app.projects.map(|p| p.switch_failed(message)),
            ..app
        }),
        Event::ProjectForgotten(projects) => handled(App {
            projects: app.projects.map(|p| p.forgotten(projects)),
            ..app
        }),
        Event::ProjectForgetFailed(message) => handled(App {
            projects: app.projects.map(|p| p.forget_failed(message)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `event`, applied to the registration screen when it owns it — a key or Ctrl-letter while
/// it is open, or how its own submission came back — or handed back for the next screen to
/// try, unchanged.
pub(super) fn try_registration(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.registration.is_some() => {
            handled(on_registration(app, |r| r.key(key)))
        }
        Event::Ctrl(letter) if app.registration.is_some() => {
            handled(on_registration(app, |r| r.ctrl(letter)))
        }
        Event::Registered(view) => handled(App {
            registration: None,
            queue: Queue::replaced(view),
            ..app
        }),
        Event::RegistrationFailed(message) => handled(App {
            registration: app.registration.map(|r| r.rejected(message)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// The app with `f`'s answer applied to the settings screen, when it is open: closed on
/// [`settings::Request::Close`], or its submission left for the loop on
/// [`settings::Request::Submit`].
fn on_settings(
    app: App,
    f: impl FnOnce(SettingsScreen) -> (SettingsScreen, Option<settings::Request>),
) -> App {
    let Some(screen) = app.settings else {
        return app;
    };
    let (screen, request) = f(screen);
    let app = App {
        settings: Some(screen),
        ..app
    };
    match request {
        Some(settings::Request::Close) => App {
            settings: None,
            ..app
        },
        Some(settings::Request::Submit(name, value)) => App {
            setting_submission: Some((name, value)),
            ..app
        },
        None => app,
    }
}

/// The app with `f`'s answer applied to the task form, when it is open: closed on
/// [`task_form::Request::Close`], its submission left for the loop on
/// [`task_form::Request::Submit`], or the app told to quit on [`task_form::Request::Quit`] —
/// the discard question was answered `y`.
fn on_form(
    app: App,
    f: impl FnOnce(TaskFormScreen) -> (TaskFormScreen, Option<task_form::Request>),
) -> App {
    let Some(screen) = app.form else {
        return app;
    };
    let (screen, request) = f(screen);
    let app = App {
        form: Some(screen),
        ..app
    };
    match request {
        Some(task_form::Request::Close) => App { form: None, ..app },
        Some(task_form::Request::Submit(draft, placement)) => App {
            submission: Some((draft, placement)),
            ..app
        },
        Some(task_form::Request::Quit) => App { quit: true, ..app },
        None => app,
    }
}

/// The app with `f`'s answer applied to the answer form, when it is open: closed either way —
/// [`answer_screen::Request::Close`] asks nothing of the loop, and unlike every other form,
/// [`answer_screen::Request::Submit`] closes the form too, leaving the task and the typed
/// answer for the loop to record.
fn on_answer(
    app: App,
    f: impl FnOnce(AnswerScreen) -> (AnswerScreen, Option<answer_screen::Request>),
) -> App {
    let Some(screen) = app.answer else {
        return app;
    };
    let task = screen.task();
    let (screen, request) = f(screen);
    let app = App {
        answer: Some(screen),
        ..app
    };
    match request {
        Some(answer_screen::Request::Close) => App {
            answer: None,
            ..app
        },
        Some(answer_screen::Request::Submit(text)) => App {
            answer: None,
            answer_submission: Some((task, text)),
            ..app
        },
        None => app,
    }
}

/// The app with `f`'s answer applied to the import form, when it is open: closed either way —
/// [`import_screen::Request::Close`] asks nothing of the loop, and unlike every other form,
/// [`import_screen::Request::Submit`] closes the form too, leaving its path for the loop to
/// import.
fn on_import(
    app: App,
    f: impl FnOnce(ImportScreen) -> (ImportScreen, Option<import_screen::Request>),
) -> App {
    let Some(screen) = app.import else {
        return app;
    };
    let (screen, request) = f(screen);
    let app = App {
        import: Some(screen),
        ..app
    };
    match request {
        Some(import_screen::Request::Close) => App {
            import: None,
            ..app
        },
        Some(import_screen::Request::Submit(path)) => App {
            import: None,
            import_submission: Some(path),
            ..app
        },
        None => app,
    }
}

/// The app with `f`'s answer applied to the project picker, when it is open: closed on
/// [`projects::Request::Close`], or a switch or a forget left for the loop, the picker staying
/// open either way.
fn on_projects(
    app: App,
    f: impl FnOnce(ProjectsScreen) -> (ProjectsScreen, Option<projects::Request>),
) -> App {
    let Some(screen) = app.projects else {
        return app;
    };
    let (screen, request) = f(screen);
    let app = App {
        projects: Some(screen),
        ..app
    };
    match request {
        Some(projects::Request::Close) => App {
            projects: None,
            ..app
        },
        Some(projects::Request::Switch(name)) => App {
            project_switch: Some(name),
            ..app
        },
        Some(projects::Request::Forget(name)) => App {
            project_forget: Some(name),
            ..app
        },
        None => app,
    }
}

/// The app with the project picker open on `projects`, the selection on the one whose queue
/// is currently shown, or the first when none matches.
fn open_projects(app: App, projects: Vec<Project>) -> App {
    let current = app.queue.project_name().map(str::to_owned);
    App {
        projects: Some(ProjectsScreen::open(projects, current.as_deref())),
        ..app
    }
}

/// The app with `f`'s answer applied to the registration screen, when it is open: the app told
/// to quit on [`registration_screen::Request::Quit`] — there is no queue to fall back to — or
/// its submission left for the loop on [`registration_screen::Request::Submit`].
fn on_registration(
    app: App,
    f: impl FnOnce(RegistrationScreen) -> (RegistrationScreen, Option<registration_screen::Request>),
) -> App {
    let Some(screen) = app.registration else {
        return app;
    };
    let (screen, request) = f(screen);
    let app = App {
        registration: Some(screen),
        ..app
    };
    match request {
        Some(registration_screen::Request::Quit) => App { quit: true, ..app },
        Some(registration_screen::Request::Submit(name)) => App {
            registration_submission: Some(name),
            ..app
        },
        None => app,
    }
}
