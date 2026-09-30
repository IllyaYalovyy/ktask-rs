//! The loop that owns the terminal and feeds events into [`update`].

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ktask_core::{JournalWatch, QueueView};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use crate::application::Application;
use crate::registration_form::RegistrationForm;
use crate::{App, Event, render, update};

/// What the loop starts on: the project's queue, already resolved, or a name still needed to
/// register the current directory under, because its own folder name is already registered for
/// another path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// The project was resolved already; the loop starts by loading its queue.
    Ready,
    /// The directory could not be resolved to a project: `message` is the refusal that says
    /// why, the same words `ktask-rs` gives for the same conflict, and the loop starts by
    /// asking for a name to register the directory under instead.
    NameTaken {
        /// The refusal to show above the name field.
        message: String,
    },
}

/// Something the loop is woken by: a key (or resize) at the terminal, the journal having
/// changed under it, the process being told to stop, a run this screen started ending or
/// refusing to start, or — only while a task is running — the tick that keeps its elapsed
/// time moving.
enum Wake {
    /// An input arrived at the terminal.
    Input(Input),
    /// The journal changed; the queue is stale and worth loading again.
    Changed,
    /// SIGTERM or SIGHUP arrived — the terminal went away, or the operator or the system
    /// asked the process to stop. There is nothing to weigh against a form's content here:
    /// the terminal is leaving regardless, so the loop ends at once.
    Stop,
    /// No other wake arrived before [`TICK`] passed, while a task was running: the queue is
    /// loaded again so the running task's elapsed time moves even though nothing else changed.
    Tick,
    /// The run this screen started has ended, or could not start at all, printing this — the
    /// same words `ktask-rs run` itself would show, one line per line it wrote.
    RunMessage(String),
}

/// How often the loop wakes on its own to refresh a running task's elapsed time, while one is
/// running. Nothing wakes it on a timer otherwise.
const TICK: Duration = Duration::from_secs(1);

/// How long a burst of [`Wake::Changed`] is given to settle before it is read: a task's
/// attempt appends several events close together — its status, then each step's own outcome —
/// so reading right on the first of them risks a reload caught between two of a related
/// group, showing one without the other. Short enough that a human never notices the delay.
const SETTLE: Duration = Duration::from_millis(20);

/// Starts a terminal synchronized update: a reader that stops at the matching end marker never
/// sees a frame half drawn.
const SYNC_START: &[u8] = b"\x1b[?2026h";
/// Ends a terminal synchronized update.
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Draws one frame of `app`, wrapped in the terminal's synchronized-update markers.
fn draw(terminal: &mut DefaultTerminal, app: &App) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot draw the screen: {e}");
    terminal.backend_mut().write_all(SYNC_START).map_err(fail)?;
    terminal
        .draw(|frame| {
            if let Some(cursor) = render(app, frame.area(), frame.buffer_mut()) {
                frame.set_cursor_position(cursor);
            }
        })
        .map_err(fail)?;
    let backend = terminal.backend_mut();
    backend.write_all(SYNC_END).map_err(fail)?;
    backend.flush().map_err(fail)
}

/// Runs the terminal interface until the operator quits.
///
/// `application` is the one value every use case the screen can reach is called through — see
/// [`Application`]. Its `load_queue` is called at the start, again whenever `watch` reports the
/// journal changed, and when the operator asks for cancelled tasks or stops asking; a failure
/// ends the loop, its message the use case's own, read here where it is about to be shown.
/// `remove_task` removes a task the operator confirmed removing, after which the queue is
/// loaded again. `add_task` adds the task the operator wrote in the form where the form says,
/// and gives its number, or the reasons it was not added when it was not — shown on the form
/// instead of closing it. `import` imports the tasks of the file whose path the operator wrote
/// in the import form, giving what to show for it — the same words `ktask-rs import` itself
/// would print — shown on the screen once it returns. `start_run` starts executing the
/// pending tasks, exactly as `ktask-rs run` does, and is called on a thread of its own each
/// time the operator asks for it, so the screen stays responsive for however long the run
/// takes; it blocks until the run it starts ends, or refuses to start at all, and gives what
/// it printed either way — the same words `ktask-rs run` itself would show — shown on the
/// screen once it returns, one line per line it wrote. `load_settings` fetches every project
/// setting, called each time the operator opens the settings screen. `save_setting` changes
/// the setting named by the settings screen's focused field to the value it was submitted
/// with, giving the new setting, or the reasons it was refused — an unknown setting or an
/// invalid value — shown on the screen instead of closing it. `load_projects` fetches the
/// registered projects, called each time the operator opens the project picker.
/// `switch_project` switches to the project the picker was submitted with, so every action
/// from then on applies to it, giving its fresh queue, or why the switch did not happen —
/// shown on the screen instead of closing it. `watch` blocks until the journal changes; it is
/// polled from a dedicated thread, so a task added, inserted or removed by another process — a
/// run included — shows in the next frame without the loop itself ever waking on a timer —
/// except while a task is running, when the queue is loaded again on a short timer too, so the
/// running task's elapsed time keeps moving even though nothing else changed; the loop goes
/// back to waiting with no timer once nothing is running. The terminal is put back as it was
/// on every way out: the operator quitting, SIGTERM or SIGHUP (its terminal going away sends
/// this), or an error — a run `start_run` started keeps going regardless, since it does not
/// depend on this process to finish.
///
/// # Errors
///
/// Fails when the queue or the settings cannot be loaded, a task cannot be removed or the
/// terminal cannot be used. `start` chooses what the loop shows first: `Start::Ready` loads the
/// queue with `application.load_queue` as described above; `Start::NameTaken` opens the
/// registration screen instead, showing its message, and `load_queue` is not called until a
/// name typed there is submitted and `application.register` succeeds with it, exactly as
/// `switch_project` replaces the queue on show when the picker is submitted.
pub fn run(
    start: Start,
    application: impl Application + Send + Sync + 'static,
    watch: impl JournalWatch + Send + 'static,
) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let application = Arc::new(application);
    let result = spawn_wakes(watch)
        .and_then(|(sender, wakes)| drive(start, &mut terminal, &application, &sender, &wakes));
    ratatui::restore();
    result
}

/// Starts the threads that turn keyboard input, journal changes and a termination signal into
/// a single stream the loop can block on, with no timer of its own; returns the sender they
/// share too, so [`drive`] can raise [`Wake::RunMessage`] from the thread it starts for
/// `start_run` the same way.
///
/// # Errors
///
/// Fails when SIGTERM and SIGHUP cannot be watched for.
fn spawn_wakes(
    watch: impl JournalWatch + Send + 'static,
) -> Result<(Sender<Wake>, Receiver<Wake>), String> {
    let (sender, receiver) = mpsc::channel();
    let keys = sender.clone();
    thread::spawn(move || {
        while let Ok(input) = event::read() {
            if keys.send(Wake::Input(input)).is_err() {
                return;
            }
        }
    });
    let changed = sender.clone();
    thread::spawn(move || {
        while watch.wait().is_ok() {
            if changed.send(Wake::Changed).is_err() {
                return;
            }
        }
    });
    let mut signals = Signals::new([SIGTERM, SIGHUP])
        .map_err(|e| format!("cannot watch for a termination signal: {e}"))?;
    let stop = sender.clone();
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = stop.send(Wake::Stop);
        }
    });
    Ok((sender, receiver))
}

/// The next [`Wake`] to act on: waits at most [`TICK`] while a task is running, so its
/// elapsed time keeps moving even when nothing else wakes the loop, or indefinitely
/// otherwise.
fn next_wake(wakes: &Receiver<Wake>, running: bool) -> Result<Wake, String> {
    if !running {
        return wakes
            .recv()
            .map_err(|_| "the keyboard and journal-watch threads both stopped".to_owned());
    }
    match wakes.recv_timeout(TICK) {
        Ok(wake) => Ok(wake),
        Err(RecvTimeoutError::Timeout) => Ok(Wake::Tick),
        Err(RecvTimeoutError::Disconnected) => {
            Err("the keyboard and journal-watch threads both stopped".to_owned())
        }
    }
}

/// The wake `drive`'s loop acts on next: one already drained ahead of its turn while
/// collapsing the previous burst, taken from `pending`, or [`next_wake`] otherwise.
fn next(pending: &mut Option<Wake>, wakes: &Receiver<Wake>, running: bool) -> Result<Wake, String> {
    match pending.take() {
        Some(wake) => Ok(wake),
        None => next_wake(wakes, running),
    }
}

/// Waits out [`SETTLE`] for a burst of [`Wake::Changed`] to finish landing, then drains every
/// one of it and every [`Wake::Tick`] queued alongside it, so the burst collapses into the one
/// reload it warrants, read whole rather than caught between two commits of a group appended
/// close together. Returns the first wake of a different kind found while draining, for the
/// next iteration to handle in its turn — never dropped, just deferred.
fn drain_changes(wakes: &Receiver<Wake>) -> Option<Wake> {
    thread::sleep(SETTLE);
    loop {
        match wakes.try_recv() {
            Ok(Wake::Changed | Wake::Tick) => {}
            Ok(other) => return Some(other),
            Err(_) => return None,
        }
    }
}

/// The queue to show, with the cancelled tasks when `show_cancelled`, read through
/// `application` and turned into a message, here, where it is about to leave the loop, if it
/// could not be loaded at all.
fn load(application: &impl Application, show_cancelled: bool) -> Result<QueueView, String> {
    application
        .load_queue(show_cancelled)
        .map_err(|error| error.to_string())
}

/// Removes or adds the task `app` has pending, if either, through `application`: `(app, true)`
/// when one was, `(app, false)`, unchanged, otherwise.
fn handle_task_action(mut app: App, application: &impl Application) -> Result<(App, bool), String> {
    if let Some(id) = app.removal.take() {
        application
            .remove_task(id)
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
/// `ktask-rs import` itself would print, one line per line of it.
fn handle_import_action(mut app: App, application: &impl Application) -> (App, bool) {
    let Some(path) = app.import_submission.take() else {
        return (app, false);
    };
    let text = match application.import(&path) {
        Ok(text) | Err(text) => text,
    };
    (update(app, Event::ImportMessage(text)), true)
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

/// Tries registering the current directory, removing or adding a task, loading or saving a
/// setting, and loading, switching or forgetting a project, in that order: the first one `app`
/// has pending wins. `(app, true)` when one did, `(app, false)` otherwise.
fn try_background_actions(app: App, application: &impl Application) -> Result<(App, bool), String> {
    let app = or_return_handled!(handle_registration_action(app, application));
    let app = or_return_handled!(handle_task_action(app, application)?);
    let app = or_return_handled!(handle_settings_action(app, application)?);
    handle_projects_action(app, application)
}

fn handle_input<A: Application + Send + Sync + 'static>(
    mut app: App,
    application: &Arc<A>,
    input: &Input,
    sender: &Sender<Wake>,
) -> Result<(App, bool), String> {
    let asked = app.show_cancelled;
    if let Some(event) = translate(input) {
        app = update(app, event);
    }
    let app = or_return_handled!(try_background_actions(app, application.as_ref())?);
    let mut app = or_return_handled!(handle_import_action(app, application.as_ref()));
    if app.run_requested.take().is_some() {
        spawn_run(Arc::clone(application), sender.clone());
        return Ok((app, true));
    }
    let should_reload = app.show_cancelled != asked;
    Ok((app, should_reload))
}

/// Whether `app`'s queue shows a task currently running.
fn task_running(app: &App) -> bool {
    app.queue
        .as_ref()
        .is_some_and(|queue| queue.summary.running > 0)
}

/// What handling one `wake` produces: `None` when the loop should stop; otherwise `app` to
/// carry on with, a wake drained ahead of its turn while collapsing a burst of
/// [`Wake::Changed`] for the next call to receive first, and whether the queue is worth
/// loading again before the next frame.
fn step<A: Application + Send + Sync + 'static>(
    app: App,
    wake: Wake,
    wakes: &Receiver<Wake>,
    application: &Arc<A>,
    sender: &Sender<Wake>,
) -> Result<Option<(App, Option<Wake>, bool)>, String> {
    match wake {
        Wake::Stop => Ok(None),
        // A single underlying change can be reported as a burst of several — a recursive
        // watch sees a project's own rollback-journal file appear and disappear around each
        // commit, on top of the change itself. Collapsing a burst into the one reload it
        // warrants keeps a run from being slowed down by a reload racing every one of its own
        // writes for the journal's lock. Anything drained that is not itself part of the
        // burst is kept, not lost, for the next call.
        Wake::Changed => Ok(Some((app, drain_changes(wakes), true))),
        Wake::Tick => Ok(Some((app, None, true))),
        Wake::RunMessage(text) => Ok(Some((update(app, Event::RunMessage(text)), None, true))),
        Wake::Input(input) => {
            let (app, reload) = handle_input(app, application, &input, sender)?;
            Ok(Some((app, None, reload)))
        }
    }
}

/// The app the loop starts on: the queue `application.load_queue` fetches, or the
/// registration screen open on `start`'s message, when it says a name is needed first.
fn initial_app(start: Start, application: &impl Application) -> Result<App, String> {
    Ok(match start {
        Start::Ready => update(App::default(), Event::Loaded(load(application, false)?)),
        Start::NameTaken { message } => App {
            registration: Some(RegistrationForm::new(message)),
            ..App::default()
        },
    })
}

fn drive<A: Application + Send + Sync + 'static>(
    start: Start,
    terminal: &mut DefaultTerminal,
    application: &Arc<A>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let app = initial_app(start, application.as_ref())?;
    run_loop(app, terminal, application, sender, wakes)
}

/// Draws `app`, then feeds it whatever wakes the loop until the operator quits or a fatal error
/// occurs, reloading the queue whenever handling a wake calls for it. A wake drained ahead of
/// its turn while collapsing a burst of [`Wake::Changed`] is carried in `pending`, so the next
/// iteration handles it rather than losing it.
fn run_loop<A: Application + Send + Sync + 'static>(
    mut app: App,
    terminal: &mut DefaultTerminal,
    application: &Arc<A>,
    sender: &Sender<Wake>,
    wakes: &Receiver<Wake>,
) -> Result<(), String> {
    let mut pending: Option<Wake> = None;
    loop {
        draw(terminal, &app)?;
        if app.quit {
            return Ok(());
        }
        let wake = next(&mut pending, wakes, task_running(&app))?;
        let Some((new_app, new_pending, reload)) = step(app, wake, wakes, application, sender)?
        else {
            return Ok(());
        };
        pending = new_pending;
        app = if reload {
            let queue = load(application.as_ref(), new_app.show_cancelled)?;
            update(new_app, Event::Loaded(queue))
        } else {
            new_app
        };
    }
}

/// Calls `application.start_run()` on a thread of its own, so the loop stays responsive for
/// however long the run it starts takes, and raises [`Wake::RunMessage`] with what it printed
/// once it returns, whether it ran to completion, stopped partway, or refused to start at all.
fn spawn_run<A: Application + Send + Sync + 'static>(application: Arc<A>, sender: Sender<Wake>) {
    thread::spawn(move || {
        let text = match application.start_run() {
            Ok(text) | Err(text) => text,
        };
        let _ = sender.send(Wake::RunMessage(text));
    });
}

/// The event an input from the terminal means, if it means any.
fn translate(input: &Input) -> Option<Event> {
    match input {
        Input::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char(letter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Event::Ctrl(letter))
            }
            code => Some(Event::Key(code)),
        },
        Input::Resize(..) => Some(Event::Resize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fmt;

    use ktask_core::{
        ATTEMPT_TIMEOUT, Placement, Project, QueueView, SettingView, StatusSummary, TaskDraft,
        TaskId,
    };

    use super::{
        handle_import_action, handle_projects_action, handle_registration_action,
        handle_settings_action, handle_task_action,
    };
    use crate::App;
    use crate::application::Application;
    use crate::form::Form;
    use crate::registration_form::RegistrationForm;
    use crate::settings_form::SettingsForm;

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
        settings: RefCell<Vec<Result<Vec<SettingView>, Failure>>>,
        save_setting: RefCell<Vec<Result<SettingView, Failure>>>,
        import: RefCell<Vec<Result<String, String>>>,
        projects: RefCell<Vec<Result<Vec<Project>, Failure>>>,
        switch: RefCell<Vec<Result<QueueView, Failure>>>,
        forget: RefCell<Vec<Result<Vec<Project>, Failure>>>,
        register: RefCell<Vec<Result<QueueView, Failure>>>,
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
        }
    }

    impl Application for Fake {
        type LoadError = Failure;
        type RemoveError = Failure;
        type AddProblem = Failure;
        type SettingsError = Failure;
        type SaveSettingError = Failure;
        type ProjectsError = Failure;
        type SwitchError = Failure;
        type ForgetError = Failure;
        type RegisterError = Failure;

        fn load_queue(&self, _show_cancelled: bool) -> Result<QueueView, Failure> {
            Ok(empty_queue())
        }

        fn remove_task(&self, _id: TaskId) -> Result<(), Failure> {
            self.remove.borrow_mut().remove(0)
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

        fn import(&self, _path: &str) -> Result<String, String> {
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

        fn start_run(&self) -> Result<String, String> {
            Ok(String::new())
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
            form: Some(Form::new(Placement::End)),
            submission: Some((empty_draft(), Placement::End)),
            ..App::default()
        };

        let (app, handled) = handle_task_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.form.expect("form stays open").problems,
            vec!["failed: empty title".to_owned()]
        );
    }

    #[test]
    fn an_added_task_closes_the_form() {
        let fake = Fake::default();
        *fake.add.borrow_mut() = vec![Ok(TaskId(7))];
        let app = App {
            form: Some(Form::new(Placement::End)),
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
            settings: Some(SettingsForm::new(&[])),
            setting_submission: Some((ATTEMPT_TIMEOUT, "soon".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_settings_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.settings.expect("settings screen stays open").problem,
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
            settings: Some(SettingsForm::new(&[])),
            setting_submission: Some((ATTEMPT_TIMEOUT, "1".to_owned())),
            ..App::default()
        };

        let (app, handled) = handle_settings_action(app, &fake).unwrap();

        assert!(handled);
        assert!(app.settings.is_none());
    }

    #[test]
    fn import_shows_the_same_text_on_success_or_refusal() {
        let fake = Fake::default();
        *fake.import.borrow_mut() = vec![Err("cannot read x".to_owned())];
        let app = App {
            import_submission: Some("x".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_import_action(app, &fake);

        assert!(handled);
        assert_eq!(app.message, Some(vec!["cannot read x".to_owned()]));
    }

    #[test]
    fn a_failed_switch_shows_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.switch.borrow_mut() = vec![Err(Failure("unknown project".to_owned()))];
        let app = App {
            project_switch: Some("ghost".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_projects_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.project_problem,
            Some("failed: unknown project".to_owned())
        );
    }

    #[test]
    fn a_failed_forget_shows_the_error_types_own_message() {
        let fake = Fake::default();
        *fake.forget.borrow_mut() = vec![Err(Failure("unknown project".to_owned()))];
        let app = App {
            project_forget: Some("ghost".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_projects_action(app, &fake).unwrap();

        assert!(handled);
        assert_eq!(
            app.project_problem,
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
            registration: Some(RegistrationForm::new("need a name".to_owned())),
            registration_submission: Some("name".to_owned()),
            ..App::default()
        };

        let (app, handled) = handle_registration_action(app, &fake);

        assert!(!handled);
        assert_eq!(
            app.registration
                .expect("registration screen stays open")
                .problem,
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
}
