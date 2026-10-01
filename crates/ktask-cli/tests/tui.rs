//! `ktask-rs tui` on the real binary, in a pseudo-terminal: the queue screen, quitting,
//! resizing, and what happens when there is no terminal. Moving around the queue is in
//! `tui/navigate.rs`; removing a task is in `tui/remove.rs`; retrying one is in
//! `tui/retry.rs`.

#[path = "tui/add.rs"]
mod add;
#[path = "tui/dashboard.rs"]
mod dashboard;
#[path = "tui/exit.rs"]
mod exit;
#[path = "tui/idle.rs"]
mod idle;
#[path = "tui/import.rs"]
mod import;
#[path = "tui/insert.rs"]
mod insert;
#[path = "tui/navigate.rs"]
mod navigate;
#[path = "tui/no_leftover_processes.rs"]
mod no_leftover_processes;
#[path = "tui/projects.rs"]
mod projects;
#[path = "support/pty.rs"]
mod pty;
#[path = "tui/register.rs"]
mod register;
#[path = "tui/remove.rs"]
mod remove;
#[path = "support/repo.rs"]
mod repo;
#[path = "tui/retry.rs"]
mod retry;
#[path = "tui/run.rs"]
mod run;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
#[path = "tui/settings.rs"]
mod settings;
mod support;
#[path = "support/tracked_branch.rs"]
mod tracked_branch;

use std::path::Path;

use pty::{Terminal, lines_inside_frame};
use repo::{git_repository, scratch};
use support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 80;
const SUMMARY: &str = "pending 0  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0";

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
    assert!(
        rows[rows.len() - 1].starts_with("└ q quit · ? keys ─"),
        "{screen}"
    );
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
    let screen = terminal.wait_for("the normal screen back", |screen| {
        !screen.alternate_screen()
    })?;
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
    let screen = terminal.wait_for_text("The queue is empty.")?;

    let lines = lines_inside_frame(&screen);
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

#[test]
fn project_named_before_tui_selects_the_same_project_as_after() -> Result<()> {
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
        &["--project", "first-app", "tui"],
        ROWS,
        COLS,
    )?;
    let screen = terminal.wait_for_text("The queue is empty.")?;

    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "first-app");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn project_named_twice_with_different_values_on_tui_exits_two_and_opens_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let first = git_repository(&sandbox, &work, "first-app")?;
    let second = git_repository(&sandbox, &work, "second-app")?;
    for repository in [&first, &second] {
        let shown = sandbox.run(repository, &["project", "show"])?;
        assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    }

    let terminal = Terminal::launch(
        &sandbox,
        &first,
        &["--project", "first-app", "tui", "--project", "second-app"],
        ROWS,
        200,
    )?;

    assert_eq!(terminal.wait_for_exit()?, 2);
    let screen = terminal.screen();
    assert!(screen.contains("\"first-app\""), "{screen}");
    assert!(screen.contains("\"second-app\""), "{screen}");
    assert!(!screen.contains("The queue is empty."), "{screen}");
    Ok(())
}

/// Adds a task of `kind` titled `title` from the command line.
fn add_task(sandbox: &Sandbox, repository: &Path, title: &str, kind: &str) -> Result<()> {
    let outcome = sandbox.run(
        repository,
        &[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--kind",
            kind,
        ],
    )?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(())
}

/// Opens the queue screen and waits until it shows a task, drawn whole.
fn open_with_tasks(sandbox: &Sandbox, cwd: &Path) -> Result<Terminal> {
    let terminal = Terminal::launch(sandbox, cwd, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue with its tasks", |screen| {
        let contents = screen.contents();
        contents.contains("1  #1  ") && contents.ends_with('┘')
    })?;
    Ok(terminal)
}

#[test]
fn tasks_added_from_the_cli_appear_with_the_summary_updated() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    add_task(&sandbox, &repository, "Write the parser", "agent")?;
    add_task(&sandbox, &repository, "Approve the design", "human")?;

    let mut terminal = open_with_tasks(&sandbox, &repository)?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "my-app");
    assert_eq!(
        lines[2],
        "pending 2  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  Write the parser");
    assert_eq!(lines[5], " 2  #2  pending  human  Approve the design");
    assert!(!screen.contains("The queue is empty."), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_task_added_after_the_screen_was_closed_shows_the_next_time_it_opens() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let mut terminal = open(&sandbox, &repository, ROWS, COLS)?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    add_task(&sandbox, &repository, "Late arrival", "agent")?;

    let mut terminal = open_with_tasks(&sandbox, &repository)?;
    let lines = lines_inside_frame(&terminal.screen());
    assert_eq!(
        lines[2],
        "pending 1  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  Late arrival");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

/// Adds a task titled `title` with `--before` or `--after` (`flag`) the task numbered `id`.
fn add_placed_task(
    sandbox: &Sandbox,
    repository: &Path,
    title: &str,
    flag: &str,
    id: &str,
) -> Result<()> {
    let outcome = sandbox.run(
        repository,
        &["add", "--title", title, "--criterion", "it works", flag, id],
    )?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(())
}

#[test]
fn tasks_inserted_from_the_cli_show_in_their_place_with_their_own_ids() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    add_task(&sandbox, &repository, "Second", "agent")?;
    add_task(&sandbox, &repository, "Fourth", "agent")?;
    add_placed_task(&sandbox, &repository, "Third", "--before", "2")?;
    add_placed_task(&sandbox, &repository, "First", "--before", "1")?;
    add_placed_task(&sandbox, &repository, "Fifth", "--after", "2")?;

    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], ROWS, COLS)?;
    let screen = terminal.wait_for("the queue with five tasks", |screen| {
        let contents = screen.contents();
        contents.contains("Fifth") && contents.ends_with('┘')
    })?;

    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 5  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
    );
    assert_eq!(lines[4], ">1  #4  pending  agent  First");
    assert_eq!(lines[5], " 2  #1  pending  agent  Second");
    assert_eq!(lines[6], " 3  #3  pending  agent  Third");
    assert_eq!(lines[7], " 4  #2  pending  agent  Fourth");
    assert_eq!(lines[8], " 5  #5  pending  agent  Fifth");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn tasks_imported_from_the_cli_show_in_their_place_with_their_own_ids() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    add_task(&sandbox, &repository, "a", "agent")?;
    add_task(&sandbox, &repository, "b", "agent")?;
    let file = work.join("tasks.json");
    std::fs::write(
        &file,
        r#"[{"title": "x", "criteria": ["c"]},
            {"title": "y", "criteria": ["c"], "kind": "human"}]"#,
    )?;
    let imported = sandbox.run(
        &repository,
        &["import", &file.to_string_lossy(), "--before", "2"],
    )?;
    assert_eq!(imported.stdout, "2 tasks added: 3, 4\n");
    assert_eq!(imported.code, Some(0), "{}", imported.stderr);

    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], ROWS, COLS)?;
    let screen = terminal.wait_for("the queue with four tasks", |screen| {
        let contents = screen.contents();
        contents.contains(" 4  #2  pending  agent  b") && contents.ends_with('┘')
    })?;

    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 4  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  a");
    assert_eq!(lines[5], " 2  #3  pending  agent  x");
    assert_eq!(lines[6], " 3  #4  pending  human  y");
    assert_eq!(lines[7], " 4  #2  pending  agent  b");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_task_removed_from_the_cli_is_hidden_and_counted_as_cancelled() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    add_task(&sandbox, &repository, "First", "agent")?;
    add_task(&sandbox, &repository, "Second", "human")?;
    add_task(&sandbox, &repository, "Third", "agent")?;
    let removed = sandbox.run(&repository, &["remove", "2"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);

    let mut terminal = open_with_tasks(&sandbox, &repository)?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 2  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 1"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  First");
    assert_eq!(lines[5], " 2  #3  pending  agent  Third");
    assert_eq!(lines[6], "");
    assert!(!screen.contains("Second"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_queue_with_every_task_removed_shows_as_empty_with_them_counted_as_cancelled() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    add_task(&sandbox, &repository, "Only", "agent")?;
    let removed = sandbox.run(&repository, &["remove", "1"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);

    let mut terminal = open(&sandbox, &repository, ROWS, COLS)?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 0  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 1"
    );
    assert_eq!(lines[4], "The queue is empty.");
    assert!(!screen.contains("Only"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
