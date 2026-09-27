//! `ktask-rs tui` on the real binary, in a pseudo-terminal: the queue screen, quitting,
//! resizing, and what happens when there is no terminal.

#[path = "support/pty.rs"]
mod pty;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use pty::Terminal;
use support::{Result, Sandbox};
use tempfile::TempDir;

const ROWS: u16 = 24;
const COLS: u16 = 80;
const SUMMARY: &str = "pending 0 · running 0 · done 0 · failed 0 · cancelled 0";

/// A scratch directory, canonical so that it can be compared with what the binary prints.
fn scratch() -> Result<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    Ok((dir, path))
}

/// A new git repository called `name` inside `parent`.
fn git_repository(sandbox: &Sandbox, parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir)?;
    let mut command = Command::new("git");
    command.args(["init", "--quiet"]);
    let status = sandbox.isolate(&mut command, &dir).status()?;
    assert!(status.success());
    Ok(dir)
}

/// Opens the terminal interface in `cwd` and waits until the queue screen is drawn whole:
/// the frame is drawn top to bottom, so it is complete once its last corner is there.
fn open(sandbox: &Sandbox, cwd: &Path, rows: u16, cols: u16) -> Result<Terminal> {
    let terminal = Terminal::launch(sandbox, cwd, &["tui"], rows, cols)?;
    terminal.wait_for("the whole queue screen", |screen| {
        let contents = screen.contents();
        contents.contains("The queue is empty.") && contents.ends_with('┘')
    })?;
    Ok(terminal)
}

/// The lines of `screen` with whatever the frame draws on either side stripped.
fn lines_inside_frame(screen: &str) -> Vec<String> {
    screen
        .lines()
        .map(|line| {
            line.trim_start_matches('│')
                .trim_end_matches('│')
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn a_new_project_shows_its_name_zero_counts_and_an_empty_queue_message() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let terminal = open(&sandbox, &repository, ROWS, COLS)?;

    let lines = lines_inside_frame(&terminal.screen());
    assert_eq!(lines[1], "my-app");
    assert_eq!(lines[2], SUMMARY);
    assert_eq!(lines[4], "The queue is empty.");
    terminal.wait_for("the full-screen mode", vt100::Screen::alternate_screen)?;
    assert!(
        !terminal.is_cooked()?,
        "a full-screen program owns the keyboard"
    );
    Ok(())
}

#[test]
fn the_screen_is_a_frame_around_the_whole_terminal_with_the_key_to_quit() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let terminal = open(&sandbox, &repository, ROWS, COLS)?;

    let screen = terminal.screen();
    let rows: Vec<&str> = screen.lines().collect();
    assert_eq!(rows.len(), usize::from(ROWS));
    assert!(rows[0].starts_with("┌ ktask-rs ─"), "{screen}");
    assert!(rows[0].ends_with('┐'), "{screen}");
    assert!(rows[rows.len() - 1].starts_with("└ q quit ─"), "{screen}");
    assert!(rows[rows.len() - 1].ends_with('┘'), "{screen}");
    Ok(())
}

#[test]
fn q_quits_with_exit_zero_and_puts_the_terminal_back() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let mut terminal = open(&sandbox, &repository, ROWS, COLS)?;

    terminal.send("q")?;

    assert_eq!(terminal.wait_for_exit()?, 0);
    terminal.wait_for("the normal screen back", |screen| {
        !screen.alternate_screen()
    })?;
    let screen = terminal.screen();
    assert!(!screen.contains("The queue is empty."), "{screen}");
    terminal.wait_for("the cursor back", |screen| !screen.hide_cursor())?;
    assert!(terminal.is_cooked()?, "line editing and echo are back on");
    Ok(())
}

#[test]
fn other_keys_do_not_quit() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let mut terminal = open(&sandbox, &repository, ROWS, COLS)?;

    terminal.send("x")?;
    // Events are handled in order, so a redraw after a resize shows that `x` was handled
    // and did not end the interface.
    terminal.resize(30, 100)?;
    terminal.wait_for("a redraw at 100x30", |screen| {
        screen.contents().lines().count() == 30 && screen.alternate_screen()
    })?;
    assert_eq!(terminal.exit(), None);
    terminal.send("q")?;

    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_first_run_in_a_project_registers_it_on_the_terminal_it_leaves_behind() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let mut terminal = open(&sandbox, &repository, ROWS, 200)?;
    terminal.send("q")?;
    terminal.wait_for_exit()?;

    let screen = terminal.wait_for_text("registered project my-app")?;

    assert!(
        screen.contains(&repository.display().to_string()),
        "{screen}"
    );
    let listed = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(listed.stdout, format!("my-app\t{}\n", repository.display()));
    Ok(())
}

#[test]
fn without_a_terminal_it_exits_two_pointing_at_list_and_registers_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let outcome = sandbox.run(&repository, &["tui"])?;

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("needs a terminal"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("`ktask-rs list`"),
        "{}",
        outcome.stderr
    );
    let listed = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(listed.stdout, "");
    Ok(())
}

#[test]
fn a_resize_redraws_at_the_new_size_exactly_as_a_fresh_start_would() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let mut terminal = open(&sandbox, &repository, ROWS, COLS)?;

    for (rows, cols) in [(30, 100), (12, 60), (ROWS, COLS)] {
        terminal.resize(rows, cols)?;
        let fresh = open(&sandbox, &repository, rows, cols)?.screen();
        let resized = terminal.wait_for(&format!("a redraw at {cols}x{rows}"), |screen| {
            screen.contents() == fresh
        })?;
        assert_eq!(resized.lines().count(), usize::from(rows), "{resized}");
    }
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn project_selects_a_registered_project_from_any_directory() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let first = git_repository(&sandbox, &work, "first-app")?;
    let second = git_repository(&sandbox, &work, "second-app")?;
    for repository in [&first, &second] {
        let shown = sandbox.run(repository, &["project", "show"])?;
        assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    }

    let mut terminal = Terminal::launch(
        &sandbox,
        &second,
        &["tui", "--project", "first-app"],
        ROWS,
        COLS,
    )?;
    terminal.wait_for_text("The queue is empty.")?;

    let lines = lines_inside_frame(&terminal.screen());
    assert_eq!(lines[1], "first-app");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn an_unknown_project_exits_two_pointing_at_project_list() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let terminal = Terminal::launch(
        &sandbox,
        &repository,
        &["tui", "--project", "ghost"],
        ROWS,
        200,
    )?;

    assert_eq!(terminal.wait_for_exit()?, 2);
    let screen = terminal.screen();
    assert!(screen.contains("unknown project \"ghost\""), "{screen}");
    assert!(screen.contains("ktask-rs project list"), "{screen}");
    assert!(!terminal.screen().contains("The queue is empty."));
    Ok(())
}
