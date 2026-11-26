/// User-facing product name; instance names stay `ktask`/`ktask-dev` for paths.
pub const PRODUCT: &str = "ktask RS";
/// A diagnostic line on stderr. Unlike `eprintln!` it never panics: a closed or full error
/// stream (a detached runner's launcher is gone, a pipe reader exited) loses the line only.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod actions;
mod app;
pub mod application;
mod backend;
pub mod build_cache;
mod cache_ui;
mod category_ui;
mod cli;
pub(crate) mod clock;
mod configuration_ui;
mod demo;
mod dialog;
mod fix_ui;
mod forms;
pub mod instance;
mod lens_ui;
mod mock;
mod persistent;
mod presentation;
mod profile_ui;
mod projection;
mod projects;
mod readiness;
mod report_ui;
pub(crate) mod resource;
pub mod runner;
pub mod scratch;
mod selection;
pub mod storage;
mod ui;

use std::io;

use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};

pub fn launch(instance: instance::Instance) -> std::process::ExitCode {
    if std::env::args_os().nth(1).is_some_and(|a| a == "__logger") {
        let Some(path) = std::env::args_os().nth(2) else {
            return std::process::ExitCode::FAILURE;
        };
        return match runner::rotate_log(
            std::io::stdin(),
            std::path::Path::new(&path),
            resource::limit("KTASK_RUNNER_LOG_BYTES", 8 * 1024 * 1024),
        ) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(_) => std::process::ExitCode::FAILURE,
        };
    }
    if std::env::args_os().nth(1).is_some_and(|a| a == "__worker") {
        return match runner::process::worker() {
            Ok(runner::process::WorkerResult::Delivered) => std::process::ExitCode::SUCCESS,
            Ok(runner::process::WorkerResult::ResourceUndelivered(resource)) => {
                std::process::ExitCode::from(resource.exit_code())
            }
            Err(storage::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                // A stalled output pipe cannot carry a fallback diagnostic.
                std::process::ExitCode::FAILURE
            }
            Err(error) => {
                // The supervisor re-raises this as a conflict; send the bare message once.
                let message = match error {
                    storage::Error::Conflict(message) => message,
                    other => other.to_string(),
                };
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stdout(),
                    "{}",
                    serde_json::to_string(&runner::process::Message::Error(message))
                        .expect("error JSON")
                );
                std::process::ExitCode::FAILURE
            }
        };
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "__investigate")
    {
        let project = std::env::args().nth(2).unwrap_or_default();
        return runner::investigate::main(instance, &project);
    }
    if std::env::args_os().nth(1).is_some_and(|a| a == "__fix") {
        let project = std::env::args().nth(2).unwrap_or_default();
        return runner::fix::main(instance, &project);
    }
    if std::env::args_os().nth(1).is_some_and(|a| a == "__fix_git") {
        return runner::fix::guarded_git();
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "__android_sidecar")
    {
        return if runner::android_transport::sidecar().is_ok() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        };
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "__android_client")
    {
        let mut args = std::env::args().skip(2);
        let Some(operation) = args.next() else {
            return std::process::ExitCode::FAILURE;
        };
        let token = std::env::var("KTASK_ANDROID_TOKEN").unwrap_or_default();
        return match runner::callback::android(&token, &operation, &args.collect::<Vec<_>>()) {
            Ok(output) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&output.stdout);
                let _ = std::io::stderr().write_all(&output.stderr);
                std::process::ExitCode::from(output.code.clamp(0, 255) as u8)
            }
            Err(error) => {
                note!("{error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "__dependency_probe")
    {
        let mut args = std::env::args().skip(2);
        let (Some(kind), Some(item)) = (args.next(), args.next()) else {
            return std::process::ExitCode::FAILURE;
        };
        if kind == "executable" {
            return application::dependency::probe_executable(&item);
        }
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stdout(),
            "{}",
            application::dependency::probe_local_file(&kind, &item, args.next().as_deref())
        );
        return std::process::ExitCode::SUCCESS;
    }
    cli::launch(instance)
}

/// The project the TUI opens, and the start folder when that is not it. A registered
/// folder, an explicit choice and the first project of an instance open as themselves.
/// Any other folder opens the first project that still exists, on the Projects screen,
/// which offers to add the folder: starting elsewhere never registers a project by itself.
fn startup_project(
    paths: &instance::Paths,
    root: &std::path::Path,
    project_name: Option<&str>,
    explicit: bool,
) -> storage::Result<(application::Service, Option<std::path::PathBuf>)> {
    use application::Service;
    let open = || storage::Database::open(paths);
    if explicit {
        return Ok((Service::open(open()?, root, project_name)?, None));
    }
    if let Some(service) = Service::find(open()?, root)? {
        return Ok((service, None));
    }
    let db = open()?;
    let other = application::overview::projects(&db)?
        .into_iter()
        .find(|project| project.root.is_dir());
    Ok(match other {
        Some(project) => (
            Service::registered(db, project.id, project.name)?,
            Some(root.canonicalize()?),
        ),
        None => (Service::open(db, root, project_name)?, None),
    })
}

pub(crate) fn launch_tui(
    instance: instance::Instance,
    root: &std::path::Path,
    project_name: Option<&str>,
    explicit: bool,
    demo: bool,
) -> io::Result<()> {
    let paths = instance.paths()?;
    let session = instance::TuiSession::register(&paths)?;
    let app = if demo {
        app::App::demo(instance)
    } else {
        let (service, unregistered) =
            startup_project(&paths, root, project_name, explicit).map_err(io::Error::other)?;
        app::App::persistent(instance, service, unregistered).map_err(io::Error::other)?
    };
    let scratch = scratch::Scratch::new_in_background(&paths.cache.join("scratch"))?;
    let workers = paths.cache.join("workers");
    let worker_reclaim = std::thread::spawn(move || scratch::Scratch::reclaim(&workers));
    let result = terminal_session(app, &session);
    let worker_cleanup = worker_reclaim
        .join()
        .map_err(|_| io::Error::other("worker scratch reclamation panicked"))?;
    let scratch_cleanup = scratch.close();
    let session_cleanup = session.close();
    result
        .and(worker_cleanup)
        .and(scratch_cleanup)
        .and(session_cleanup)
}

fn terminal_session(app: app::App, session: &instance::TuiSession) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut input_restore = RestoreInput {
        keyboard_pushed: false,
    };
    let setup = execute!(
        terminal.backend_mut(),
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    input_restore.keyboard_pushed = setup.is_ok();
    let result = setup.and_then(|_| run(&mut terminal, app, session));
    let mouse_restored = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let keyboard_restored = if input_restore.keyboard_pushed {
        let restored = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
        if restored.is_ok() {
            input_restore.keyboard_pushed = false;
        }
        restored
    } else {
        Ok(())
    };
    ratatui::restore();
    result.and(mouse_restored).and(keyboard_restored)
}

struct RestoreInput {
    keyboard_pushed: bool,
}
impl Drop for RestoreInput {
    fn drop(&mut self) {
        // Also runs during unwinding; ratatui's panic hook restores raw/alternate mode.
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        if self.keyboard_pushed {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
    }
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: app::App,
    session: &instance::TuiSession,
) -> io::Result<()> {
    let mut captured = false;
    loop {
        if captured != app.mouse_enabled {
            if app.mouse_enabled {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            }
            captured = app.mouse_enabled;
        }
        execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let drawn = terminal
            .draw(|frame| ui::draw(frame, &mut app))
            .map(|frame| frame.area.as_size());
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
        let drawn_size = drawn?;
        if let Some(copy) = app.copy_pending.take() {
            let encoded = selection::base64(copy.as_bytes());
            use std::io::Write;
            terminal
                .backend_mut()
                .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())?;
            terminal.backend_mut().flush()?;
        }
        loop {
            let has_event = event::poll(std::time::Duration::from_millis(40))?;
            if session.close_requested()? {
                return Ok(());
            }
            if has_event {
                let fits = ui::fits(terminal.size()?);
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key, fits),
                    Event::Mouse(mouse) if mouse.kind == event::MouseEventKind::Moved => {
                        // Hover does not change the UI, but must not starve live
                        // state refresh or resize handling during continuous motion.
                        if app.tick() || terminal.size()? != drawn_size {
                            break;
                        }
                        continue;
                    }
                    Event::Mouse(mouse) => app.mouse(mouse, fits),
                    Event::Paste(text) => app.paste(&text, fits),
                    _ => {}
                }
                break;
            }
            // Resize notifications can be lost (e.g. a sandbox blocks the signal wakeup).
            if app.tick() || terminal.size()? != drawn_size {
                break;
            }
        }
        if app.quit {
            return Ok(());
        }
        app.tick();
    }
}
