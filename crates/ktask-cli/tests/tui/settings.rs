//! The settings screen from the queue: `s` opens it, Tab and Shift-Tab move the focus between
//! the project's settings, each value is editable, Ctrl-S saves the focused one through the
//! same use case `ktask-rs settings set` runs, Esc leaves everything as it was, and a refused
//! value shows the same refusal the CLI would.

use std::path::PathBuf;

use super::navigate::{ESC, Fixture, ROWS, quit};
use super::pty::{Terminal, lines_inside_frame};
use super::repo::scratch;
use super::support::{Result, Sandbox};
use super::tracked_branch::cloned_repository;

const BACKSPACE: &str = "\x7f";
const SUBMIT: &str = "\x13";
const TAB: &str = "\t";

/// Opens the queue screen and the settings screen over it.
fn open_settings(fixture: &Fixture) -> Result<Terminal> {
    enter_settings(fixture.open(ROWS)?)
}

/// Sends `s` to `terminal` and waits for the settings screen to be drawn.
fn enter_settings(mut terminal: Terminal) -> Result<Terminal> {
    terminal.send("s")?;
    terminal.wait_for("the settings screen", |screen| {
        screen.contents().contains("Settings")
    })?;
    Ok(terminal)
}

/// What `ktask-rs settings` prints for the project, as text.
fn cli_settings(fixture: &Fixture) -> Result<String> {
    cli_settings_at(&fixture.sandbox, &fixture.repository)
}

/// What `ktask-rs settings` prints for the project at `repository`, as text.
fn cli_settings_at(sandbox: &Sandbox, repository: &std::path::Path) -> Result<String> {
    let outcome = sandbox.run(repository, &["settings"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(outcome.stdout)
}

/// Closes the settings screen with Esc, back to the queue, then quits.
fn quit_from_settings(mut terminal: Terminal) -> Result<()> {
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    quit(terminal)
}

/// A sandbox whose repository is a clone of a local bare repository, tracking it as `origin`
/// and checked out on `main` — so `origin/main` names a real remote branch, unlike [`Fixture`]'s
/// own repository, which has no remote at all.
struct ClonedFixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl ClonedFixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = cloned_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn open(&self, rows: u16) -> Result<Terminal> {
        super::open(&self.sandbox, &self.repository, rows, super::COLS)
    }

    fn cli_settings(&self) -> Result<String> {
        cli_settings_at(&self.sandbox, &self.repository)
    }
}

#[test]
fn s_opens_the_settings_screen_on_every_setting_focused_on_the_first() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = open_settings(&fixture)?;

    let screen = terminal.wait_for("the cursor shown in the field", |screen| {
        !screen.hide_cursor()
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Settings");
    assert_eq!(lines[3], "Attempt timeout, in seconds (default):");
    assert_eq!(lines[4], "> 14400");
    assert_eq!(lines[6], "Health check command (default):");
    assert_eq!(lines[7], "");
    assert_eq!(lines[9], "Tracked branch (remote/branch) (default):");
    assert_eq!(lines[10], "");
    let bottom = screen
        .lines()
        .nth(usize::from(ROWS) - 1)
        .unwrap_or_default();
    assert!(
        bottom.starts_with("└ Tab, Shift-Tab field · Ctrl-S save · Esc cancel"),
        "{bottom}"
    );
    quit_from_settings(terminal)
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
    assert_eq!(
        cli_settings(&fixture)?,
        "attempt-timeout\t7200\tcustom\nhealth-check\t\tdefault\ntracked-branch\t\tdefault\n"
    );
    quit(terminal)
}

#[test]
fn tab_moves_to_the_health_check_field_and_ctrl_s_saves_it_leaving_the_timeout_untouched()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_settings(&fixture)?;

    terminal.send(TAB)?;
    terminal.wait_for("the focus on the health-check field", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(7).is_some_and(|line| line == ">")
    })?;
    terminal.send("cargo test")?;
    terminal.wait_for("the typed command", |screen| {
        screen.contents().contains("> cargo test")
    })?;

    terminal.send(SUBMIT)?;

    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    assert_eq!(
        cli_settings(&fixture)?,
        "attempt-timeout\t14400\tdefault\nhealth-check\tcargo test\tcustom\ntracked-branch\t\tdefault\n"
    );
    quit(terminal)
}

#[test]
fn tab_tab_moves_to_the_tracked_branch_field_and_ctrl_s_saves_a_valid_value() -> Result<()> {
    let fixture = ClonedFixture::new()?;
    let mut terminal = enter_settings(fixture.open(ROWS)?)?;

    terminal.send(&format!("{TAB}{TAB}"))?;
    terminal.wait_for("the focus on the tracked-branch field", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(10).is_some_and(|line| line == ">")
    })?;
    terminal.send("origin/main")?;
    terminal.wait_for("the typed value", |screen| {
        screen.contents().contains("> origin/main")
    })?;

    terminal.send(SUBMIT)?;

    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Settings")
    })?;
    assert_eq!(
        fixture.cli_settings()?,
        "attempt-timeout\t14400\tdefault\nhealth-check\t\tdefault\ntracked-branch\torigin/main\tcustom\n"
    );
    quit(terminal)
}

#[test]
fn an_invalid_tracked_branch_shows_the_same_refusal_the_cli_would_and_saves_nothing() -> Result<()>
{
    let fixture = Fixture::new()?;
    let before = cli_settings(&fixture)?;
    let mut terminal = open_settings(&fixture)?;
    terminal.send(&format!("{TAB}{TAB}not-a-branch"))?;
    terminal.wait_for("the typed value", |screen| {
        screen.contents().contains("> not-a-branch")
    })?;

    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for("the refusal", |screen| screen.contents().contains("! "))?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines[2].contains("must name a remote and a branch"),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "> not-a-branch"),
        "{lines:?}"
    );
    assert_eq!(cli_settings(&fixture)?, before);
    quit_from_settings(terminal)
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
