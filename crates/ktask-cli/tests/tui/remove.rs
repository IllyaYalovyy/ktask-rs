//! Removing a task from the queue screen: `d` asks, `y` removes through the same use case
//! `ktask-rs remove` runs, `n` and Esc keep the task; what the screen shows after each, and
//! what `ktask-rs list --all` shows of the journal. `d` on the running task is separate: it
//! is refused, not asked about, exactly as `ktask-rs remove` refuses it.

use std::path::Path;
use std::process::{Command, Stdio};

use super::navigate::{
    COLS, ESC, Fixture, ROWS, marked, quit, wait_selected, wait_selected_screen,
};
use super::pty::{Terminal, lines_inside_frame};
use super::support::Result;

/// The question `d` asks about the task `id` titled `title`.
fn question(id: u32, title: &str) -> String {
    format!("Remove #{id} {title}? y to remove · n or Esc to keep")
}

/// Every task the CLI knows, cancelled ones included, as `position:status:title`.
fn journal(fixture: &Fixture) -> Result<Vec<String>> {
    let listed = fixture
        .sandbox
        .run(&fixture.repository, &["list", "--all"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    listed
        .stdout
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            let [position, _id, status, _kind, title] = fields[..] else {
                return Err(format!("not a list line: {line:?}").into());
            };
            Ok(format!("{position}:{status}:{title}"))
        })
        .collect()
}

/// Opens the queue screen and waits until it is drawn whole with the selection on `title`.
fn open_on(fixture: &Fixture, title: &str) -> Result<Terminal> {
    let terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for(&format!("the queue with {title} selected"), |screen| {
        let contents = screen.contents();
        contents.ends_with('┘')
            && matches!(marked(&contents).as_slice(), [row] if row.ends_with(title))
    })?;
    Ok(terminal)
}

/// Presses `d` and waits for the question about `title`.
fn ask(terminal: &mut Terminal, id: u32, title: &str) -> Result<String> {
    terminal.send("d")?;
    terminal.wait_for(&format!("the question about {title}"), |screen| {
        screen.contents().contains(&question(id, title))
    })
}

#[test]
fn d_asks_before_removing_and_names_the_selected_task_without_changing_anything() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;
    let before = journal(&fixture)?;

    let screen = ask(&mut terminal, 2, "bravo")?;

    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[3], question(2, "bravo"));
    assert_eq!(
        lines[2],
        "pending 5  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 0"
    );
    assert_eq!(marked(&screen), [">2  #2  pending  agent  bravo"]);
    assert!(screen.contains("echo"), "{screen}");
    assert_eq!(journal(&fixture)?, before);
    quit(terminal)
}

#[test]
fn d_then_y_removes_the_selected_task_and_the_selection_moves_to_the_one_after_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;
    ask(&mut terminal, 2, "bravo")?;

    terminal.send("y")?;

    let screen = terminal.wait_for("bravo gone", |screen| !screen.contents().contains("bravo"))?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[3], "");
    assert_eq!(
        lines[2],
        "pending 4  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 1"
    );
    assert_eq!(lines[4], " 1  #1  pending  agent  alpha");
    assert_eq!(lines[5], ">2  #3  pending  agent  charlie");
    assert_eq!(lines[6], " 3  #4  pending  agent  delta");
    assert_eq!(lines[7], " 4  #5  pending  agent  echo");
    assert_eq!(lines[8], "");
    assert_eq!(marked(&screen), [lines[5].clone()]);
    assert_eq!(
        journal(&fixture)?,
        [
            "1:pending:alpha",
            "2:cancelled:bravo",
            "3:pending:charlie",
            "4:pending:delta",
            "5:pending:echo"
        ]
    );
    // The screen is back to the queue: the selection moves on from where it moved to.
    terminal.send("j")?;
    wait_selected(&terminal, "delta")?;
    quit(terminal)
}

#[test]
fn removing_the_last_task_selects_the_one_before_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("G")?;
    wait_selected(&terminal, "echo")?;
    ask(&mut terminal, 5, "echo")?;

    terminal.send("y")?;

    terminal.wait_for("echo gone", |screen| !screen.contents().contains("echo"))?;
    let row = wait_selected(&terminal, "delta")?;
    assert_eq!(row, ">4  #4  pending  agent  delta");
    assert!(journal(&fixture)?.contains(&"5:cancelled:echo".to_owned()));
    quit(terminal)
}

#[test]
fn removing_the_first_task_selects_the_one_that_takes_its_place() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    ask(&mut terminal, 1, "alpha")?;

    terminal.send("y")?;

    terminal.wait_for("alpha gone", |screen| !screen.contents().contains("alpha"))?;
    let row = wait_selected(&terminal, "bravo")?;
    assert_eq!(row, ">1  #2  pending  agent  bravo");
    quit(terminal)
}

#[test]
fn d_then_n_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;
    let before = terminal.screen();
    let journal_before = journal(&fixture)?;
    ask(&mut terminal, 2, "bravo")?;

    terminal.send("n")?;

    let screen = terminal.wait_for("the question gone", |screen| {
        !screen.contents().contains("Remove #")
    })?;
    assert_eq!(screen, before);
    // The task is still there to be removed or moved off: the next keys reach the queue.
    terminal.send("j")?;
    wait_selected(&terminal, "charlie")?;
    assert_eq!(journal(&fixture)?, journal_before);
    quit(terminal)
}

#[test]
fn d_then_esc_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;
    let before = terminal.screen();
    let journal_before = journal(&fixture)?;
    ask(&mut terminal, 2, "bravo")?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the question gone", |screen| {
        !screen.contents().contains("Remove #")
    })?;
    assert_eq!(screen, before);
    terminal.send("j")?;
    wait_selected(&terminal, "charlie")?;
    assert_eq!(journal(&fixture)?, journal_before);
    quit(terminal)
}

#[test]
fn after_n_a_second_d_asks_again_and_y_then_removes() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    ask(&mut terminal, 1, "alpha")?;
    terminal.send("n")?;
    terminal.wait_for("the question gone", |screen| {
        !screen.contents().contains("Remove #")
    })?;

    ask(&mut terminal, 1, "alpha")?;
    terminal.send("y")?;

    terminal.wait_for("alpha gone", |screen| !screen.contents().contains("alpha"))?;
    assert!(journal(&fixture)?.contains(&"1:cancelled:alpha".to_owned()));
    quit(terminal)
}

#[test]
fn while_the_question_is_open_only_y_n_esc_and_q_are_answered() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    let before = terminal.screen();
    let asked = ask(&mut terminal, 1, "alpha")?;

    for keys in ["j", "G", "a", "?", "d", "x", "k"] {
        terminal.send(keys)?;
    }
    // Keys are handled in order: once n has been, the ones before it were too.
    terminal.send("n")?;
    let screen = terminal.wait_for("the question gone", |screen| {
        !screen.contents().contains("Remove #")
    })?;

    assert_eq!(screen, before);
    assert_ne!(screen, asked);
    assert!(
        journal(&fixture)?
            .iter()
            .all(|line| !line.contains("cancelled"))
    );
    quit(terminal)
}

#[test]
fn q_quits_from_the_question_and_removes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    ask(&mut terminal, 1, "alpha")?;

    terminal.send("q")?;

    assert_eq!(terminal.wait_for_exit()?, 0);
    assert!(
        journal(&fixture)?
            .iter()
            .all(|line| !line.contains("cancelled"))
    );
    Ok(())
}

#[test]
fn removing_every_task_one_by_one_ends_on_the_empty_queue_message() -> Result<()> {
    let fixture = Fixture::empty()?;
    fixture.add("alpha")?;
    fixture.add("bravo")?;
    let mut terminal = open_on(&fixture, "alpha")?;
    ask(&mut terminal, 1, "alpha")?;
    terminal.send("y")?;
    wait_selected(&terminal, "bravo")?;
    ask(&mut terminal, 2, "bravo")?;

    terminal.send("y")?;

    let screen = terminal.wait_for("the empty queue message", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 0  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 2"
    );
    assert_eq!(lines[3], "");
    assert_eq!(lines[4], "The queue is empty.");
    assert!(marked(&screen).is_empty(), "{screen}");
    assert_eq!(
        journal(&fixture)?,
        ["1:cancelled:alpha", "2:cancelled:bravo"]
    );
    // With nothing selected, d asks nothing.
    terminal.send("d")?;
    terminal.send("?")?;
    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(!screen.contains("Remove #"), "{screen}");
    quit(terminal)
}

#[test]
fn removing_the_only_task_shows_the_empty_queue_message_and_a_task_added_later_appears()
-> Result<()> {
    let fixture = Fixture::empty()?;
    fixture.add("alpha")?;
    let mut terminal = open_on(&fixture, "alpha")?;
    ask(&mut terminal, 1, "alpha")?;

    terminal.send("y")?;

    terminal.wait_for("the empty queue message", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    fixture.add("bravo")?;
    terminal.wait_for("the new task", |screen| screen.contents().contains("bravo"))?;
    wait_selected(&terminal, "bravo")?;
    quit(terminal)
}

#[test]
fn with_cancelled_tasks_shown_the_removed_task_stays_in_its_place_and_the_selection_moves_on()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("a")?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;
    ask(&mut terminal, 2, "bravo")?;

    terminal.send("y")?;

    let screen = terminal.wait_for("bravo cancelled", |screen| {
        screen.contents().contains("cancelled  agent  bravo")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 4  running 0  done 0  failed 0  blocked 0  unknown 0  cancelled 1"
    );
    // `cancelled` (9 characters) is now the widest status shown, so every `pending` row
    // pads out to match it.
    assert_eq!(lines[4], " 1  #1  pending    agent  alpha");
    assert_eq!(lines[5], " 2  #2  cancelled  agent  bravo");
    assert_eq!(lines[6], ">3  #3  pending    agent  charlie");
    assert_eq!(marked(&screen), [lines[6].clone()]);
    quit(terminal)
}

#[test]
fn d_on_a_cancelled_task_shows_the_same_refusal_as_remove_and_asks_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.cli(&["remove", "1"])?;
    let mut terminal = open_on(&fixture, "bravo")?;
    terminal.send("a")?;
    terminal.wait_for("the cancelled task", |screen| {
        screen.contents().contains("cancelled  agent  alpha")
    })?;
    terminal.send("k")?;
    let row = wait_selected(&terminal, "alpha")?;
    let before = terminal.screen();

    terminal.send("d")?;

    let screen = terminal.wait_for("the refusal", |screen| {
        screen.contents().contains("task 1 is already cancelled")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[3], "task 1 is already cancelled");
    assert!(!screen.contains("y to remove"), "{screen}");
    assert_eq!(marked(&screen), [row], "the refusal asked nothing");
    // The same words the CLI gives for removing the same task again.
    let cli_refusal = fixture.sandbox.run(&fixture.repository, &["remove", "1"])?;
    assert_eq!(cli_refusal.code, Some(2));
    assert!(
        cli_refusal.stderr.contains(&lines[3]),
        "{}",
        cli_refusal.stderr
    );

    terminal.send("j")?;
    let screen = wait_selected_screen(&terminal, "bravo")?;
    assert!(!screen.contains("Remove #"));
    assert!(!screen.contains("is already cancelled"));

    terminal.send("k")?;
    let screen = wait_selected_screen(&terminal, "alpha")?;
    assert_eq!(screen, before);
    quit(terminal)
}

/// A bash block that waits for the file at `go` to exist, then reports `done` (or, for the
/// review step, `approved`; for the test step, `accepted`): an attempt that stays running
/// until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
}

#[test]
fn d_on_the_running_task_shows_the_same_refusal_and_asks_nothing() -> Result<()> {
    let fixture = Fixture::empty()?;
    let go = fixture.sandbox.tmpdir().join("go");
    fixture.cli(&[
        "add",
        "--title",
        "gated",
        "--criterion",
        "it works",
        "--body",
        &gated_body(&go),
    ])?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
    command.arg("run");
    fixture.sandbox.isolate(&mut command, &fixture.repository);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut run = command.spawn()?;

    let mut terminal = open_on(&fixture, "gated")?;
    let screen = terminal.wait_for("the task running", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  running"))
    })?;
    let running_row = lines_inside_frame(&screen)[4].clone();

    terminal.send("d")?;
    let screen = terminal.wait_for("the refusal", |screen| {
        screen.contents().contains("task 1 is running")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[3], "task 1 is running");
    assert!(!screen.contains("y to remove"), "{screen}");
    assert_eq!(lines[4], running_row, "the refusal asked nothing");

    terminal.send("j")?;
    let screen = terminal.wait_for("the refusal gone", |screen| {
        !screen.contents().contains("task 1 is running")
    })?;
    assert!(!screen.contains("Remove #"), "{screen}");

    std::fs::write(&go, "")?;
    let status = run.wait()?;
    assert!(status.success(), "{status:?}");
    terminal.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;

    let listed = fixture
        .sandbox
        .run(&fixture.repository, &["list", "--all"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert!(!listed.stdout.contains("cancelled"), "{}", listed.stdout);
    assert!(listed.stdout.contains("done"), "{}", listed.stdout);

    quit(terminal)
}

#[test]
fn a_task_removed_from_the_cli_while_it_is_asked_about_takes_the_question_away() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    ask(&mut terminal, 1, "alpha")?;

    fixture.cli(&["remove", "1"])?;

    let screen = terminal.wait_for("alpha gone", |screen| !screen.contents().contains("alpha"))?;
    assert!(!screen.contains("Remove #"), "{screen}");
    wait_selected(&terminal, "bravo")?;
    // A y that follows removes nothing: the question is gone.
    terminal.send("y")?;
    terminal.send("j")?;
    wait_selected(&terminal, "charlie")?;
    assert!(journal(&fixture)?.contains(&"2:pending:bravo".to_owned()));
    quit(terminal)
}
