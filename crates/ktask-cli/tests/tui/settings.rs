//! The settings screen from the queue: `s` opens it, the value is editable, Ctrl-S saves
//! through the same use case `ktask-rs settings set` runs, Esc leaves everything as it was,
//! and a refused value shows the same refusal the CLI would.

use super::navigate::{ESC, Fixture, ROWS, quit};
use super::pty::{Terminal, lines_inside_frame};
use super::support::Result;

const BACKSPACE: &str = "\x7f";
const SUBMIT: &str = "\x13";

/// Opens the queue screen and the settings screen over it.
fn open_settings(fixture: &Fixture) -> Result<Terminal> {
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("s")?;
    terminal.wait_for("the settings screen", |screen| {
        screen.contents().contains("Settings")
    })?;
    Ok(terminal)
}

/// What `ktask-rs settings` prints for the project, as one line.
fn cli_settings(fixture: &Fixture) -> Result<String> {
    let outcome = fixture.sandbox.run(&fixture.repository, &["settings"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(outcome.stdout)
}

#[test]
fn s_opens_the_settings_screen_on_the_default_value() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = open_settings(&fixture)?;

    let screen = terminal.wait_for("the cursor shown in the field", |screen| {
        !screen.hide_cursor()
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Settings");
    assert_eq!(lines[3], "Attempt timeout, in seconds (default):");
    assert_eq!(lines[4], "> 14400");
    let bottom = screen
        .lines()
        .nth(usize::from(ROWS) - 1)
        .unwrap_or_default();
    assert!(bottom.starts_with("└ Ctrl-S save · Esc cancel"), "{bottom}");
    quit_from_settings(terminal)
}

/// Closes the settings screen with Esc, back to the queue, then quits.
fn quit_from_settings(mut terminal: Terminal) -> Result<()> {
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    quit(terminal)
}

#[test]
fn the_queue_key_map_lists_s() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    let screen = terminal.wait_for_text("Keys")?;
    assert!(
        screen.contains("s        open the project's settings"),
        "{screen}"
    );
    Ok(())
}

#[test]
fn esc_leaves_the_setting_exactly_as_it_was() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = cli_settings(&fixture)?;
    let mut terminal = open_settings(&fixture)?;
    terminal.send(&format!(
        "{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}600"
    ))?;
    terminal.wait_for("the edited value", |screen| {
        screen.contents().contains("> 600")
    })?;

    terminal.send(ESC)?;

    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    assert_eq!(cli_settings(&fixture)?, before);
    quit(terminal)
}

#[test]
fn editing_and_ctrl_s_saves_through_the_same_use_case_settings_set_runs() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_settings(&fixture)?;
    terminal.send(&format!(
        "{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}7200"
    ))?;
    terminal.wait_for("the edited value", |screen| {
        screen.contents().contains("> 7200")
    })?;

    terminal.send(SUBMIT)?;

    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    assert_eq!(cli_settings(&fixture)?, "attempt-timeout\t7200\tcustom\n");
    quit(terminal)
}

#[test]
fn an_invalid_value_shows_the_same_refusal_the_cli_would_and_saves_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = cli_settings(&fixture)?;
    let mut terminal = open_settings(&fixture)?;
    terminal.send(&format!(
        "{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}soon"
    ))?;

    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for("the refusal", |screen| screen.contents().contains("! "))?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines[2].contains("is not a whole number of seconds"),
        "{lines:?}"
    );
    assert!(lines.iter().any(|line| line == "> soon"), "{lines:?}");
    assert_eq!(cli_settings(&fixture)?, before);
    quit_from_settings(terminal)
}
