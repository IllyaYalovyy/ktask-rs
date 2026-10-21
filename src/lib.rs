/// User-facing product name; instance names stay `ktask`/`ktask-dev` for paths.
pub const PRODUCT: &str = "ktask RS";
mod actions;
mod app;
pub mod application;
mod cli;
pub(crate) mod clock;
mod configuration_ui;
mod dialog;
pub mod instance;
mod mock;
mod persistent;
mod presentation;
mod readiness;
pub mod runner;
pub mod scratch;
mod selection;
pub mod storage;
mod task_actions;
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
    if std::env::args_os().nth(1).is_some_and(|a| a == "__worker") {
        return match runner::process::worker() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                println!(
                    "{}",
                    serde_json::to_string(&runner::process::Message::Error(error.to_string()))
                        .expect("error JSON")
                );
                std::process::ExitCode::FAILURE
            }
        };
    }
    cli::launch(instance)
}

pub(crate) fn launch_tui(
    instance: instance::Instance,
    root: &std::path::Path,
    project_name: Option<&str>,
    demo: bool,
) -> io::Result<()> {
    let paths = instance.paths()?;
    let app = if demo {
        app::App::new(instance)
    } else {
        let db = storage::Database::open(&paths).map_err(io::Error::other)?;
        let service =
            application::Service::open(db, root, project_name).map_err(io::Error::other)?;
        app::App::persistent(instance, service).map_err(io::Error::other)?
    };
    let scratch = scratch::Scratch::new(&paths.cache.join("scratch"))?;
    let result = terminal_session(app);
    let cleanup = scratch.close();
    result.and(cleanup)
}

fn terminal_session(app: app::App) -> io::Result<()> {
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
    let result = setup.and_then(|_| run(&mut terminal, app));
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

fn run(terminal: &mut ratatui::DefaultTerminal, mut app: app::App) -> io::Result<()> {
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
            if event::poll(std::time::Duration::from_millis(40))? {
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
