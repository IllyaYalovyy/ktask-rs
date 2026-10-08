//! Adding a task from the queue screen next to the selected one: `o` opens the form for a
//! task below it, `O` for one above, and submitting puts it there through the same use case
//! `ktask-rs add --after` and `--before` run. What the screen shows after each step, and what
//! `ktask-rs list --json` shows of the journal.

use super::add::{SUBMIT, TAB, listed, wait_queue_with};
use super::navigate::{ESC, Fixture, ROWS, marked, quit, wait_selected, wait_selected_screen};
use super::pty::{Terminal, lines_inside_frame};
use super::support::Result;

/// Types a task's title and its one criterion into the open form and submits it.
fn write_and_submit(terminal: &mut Terminal, title: &str) -> Result<()> {
    terminal.send(title)?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}it works{SUBMIT}"))?;
    Ok(())
}

/// The queue as `list --json` shows it: each task's ID and title, in order.
fn order(fixture: &Fixture) -> Result<Vec<(u64, String)>> {
    listed(fixture)?
        .iter()
        .map(|task| {
            let id = task["id"].as_u64().ok_or("a task has no ID")?;
            let title = task["title"].as_str().ok_or("a task has no title")?;
            Ok((id, title.to_owned()))
        })
        .collect()
}

/// The IDs and titles `expected` gives, each title with its ID as `Fixture` numbers them and
/// the new task `new` as 6.
fn queue(titles: &[&str]) -> Vec<(u64, String)> {
    titles
        .iter()
        .map(|title| {
            let id = match *title {
                "alpha" => 1,
                "bravo" => 2,
                "charlie" => 3,
                "delta" => 4,
                "echo" => 5,
                _ => 6,
            };
            (id, (*title).to_owned())
        })
        .collect()
}

/// Waits for the form headed `heading` to be open over the queue.
fn wait_form(terminal: &Terminal, heading: &str) -> Result<()> {
    terminal.wait_for(&format!("the form headed {heading:?}"), |screen| {
        lines_inside_frame(&screen.contents())
            .get(1)
            .map(String::as_str)
            == Some(heading)
    })?;
    Ok(())
}

#[test]
fn capital_o_then_submit_inserts_above_the_selected_task_with_the_ids_unchanged() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;

    terminal.send("O")?;
    wait_form(&terminal, "New task above #2")?;
    write_and_submit(&mut terminal, "new")?;

    wait_queue_with(&terminal, "new")?;
    let screen = wait_selected_screen(&terminal, "new")?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(marked(&screen), [">2  #6  pending  agent  new".to_owned()]);
    assert_eq!(lines[5], " 1  #1  pending  agent  alpha");
    assert_eq!(lines[7], " 3  #2  pending  agent  bravo");
    assert_eq!(lines[10], " 6  #5  pending  agent  echo");
    assert_eq!(
        order(&fixture)?,
        queue(&["alpha", "new", "bravo", "charlie", "delta", "echo"])
    );
    quit(terminal)
}

#[test]
fn o_then_submit_inserts_below_the_selected_task_with_the_ids_unchanged() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;

    terminal.send("o")?;
    wait_form(&terminal, "New task below #2")?;
    write_and_submit(&mut terminal, "new")?;

    wait_queue_with(&terminal, "new")?;
    let screen = wait_selected_screen(&terminal, "new")?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(marked(&screen), [">3  #6  pending  agent  new".to_owned()]);
    assert_eq!(lines[6], " 2  #2  pending  agent  bravo");
    assert_eq!(lines[8], " 4  #3  pending  agent  charlie");
    assert_eq!(
        order(&fixture)?,
        queue(&["alpha", "bravo", "new", "charlie", "delta", "echo"])
    );
    quit(terminal)
}

#[test]
fn the_first_and_the_last_task_take_a_new_one_above_and_below() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;

    terminal.send("O")?;
    wait_form(&terminal, "New task above #1")?;
    write_and_submit(&mut terminal, "first")?;
    wait_queue_with(&terminal, "first")?;
    assert_eq!(
        wait_selected(&terminal, "first")?,
        ">1  #6  pending  agent  first"
    );

    terminal.send("G")?;
    wait_selected(&terminal, "echo")?;
    terminal.send("o")?;
    wait_form(&terminal, "New task below #5")?;
    write_and_submit(&mut terminal, "last")?;
    wait_queue_with(&terminal, "last")?;
    assert_eq!(
        wait_selected(&terminal, "last")?,
        ">7  #7  pending  agent  last"
    );

    let titles = order(&fixture)?;
    assert_eq!(
        titles
            .iter()
            .map(|(_, title)| title.as_str())
            .collect::<Vec<_>>(),
        [
            "first", "alpha", "bravo", "charlie", "delta", "echo", "last"
        ]
    );
    assert_eq!(
        titles.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [6, 1, 2, 3, 4, 5, 7]
    );
    quit(terminal)
}

#[test]
fn the_form_inserts_where_the_add_command_does_with_before_and_after() -> Result<()> {
    for (key, flag) in [("O", "--before"), ("o", "--after")] {
        let typed = Fixture::new()?;
        let mut terminal = typed.open(ROWS)?;
        terminal.send("jj")?;
        wait_selected(&terminal, "charlie")?;
        terminal.send(key)?;
        wait_form(
            &terminal,
            &format!("New task {} #3", if key == "O" { "above" } else { "below" }),
        )?;
        write_and_submit(&mut terminal, "same")?;
        wait_queue_with(&terminal, "same")?;

        let by_cli = Fixture::new()?;
        by_cli.cli(&[
            "add",
            "--title",
            "same",
            "--criterion",
            "it works",
            flag,
            "3",
        ])?;

        assert_eq!(listed(&typed)?, listed(&by_cli)?, "{key} against {flag}");
        quit(terminal)?;
    }
    Ok(())
}

#[test]
fn a_task_added_with_the_cli_at_a_place_is_shown_where_the_form_would_have_put_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;

    fixture.cli(&[
        "add",
        "--title",
        "via cli",
        "--criterion",
        "c",
        "--after",
        "2",
    ])?;

    terminal.wait_for("the task from the CLI below bravo", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines
            .get(6)
            .is_some_and(|line| line == ">2  #2  pending  agent  bravo")
            && lines
                .get(7)
                .is_some_and(|line| line == " 3  #6  pending  agent  via cli")
    })?;
    quit(terminal)
}

#[test]
fn on_an_empty_queue_o_and_capital_o_add_at_the_end_and_select_the_new_task() -> Result<()> {
    for key in ["o", "O"] {
        let fixture = Fixture::empty()?;
        let mut terminal = Terminal::launch(
            &fixture.sandbox,
            &fixture.repository,
            &["tui"],
            ROWS,
            super::navigate::COLS,
        )?;
        terminal.wait_for("the empty queue", |screen| {
            let contents = screen.contents();
            contents.contains("The queue is empty.") && contents.ends_with('┘')
        })?;

        terminal.send(key)?;
        wait_form(&terminal, "New task")?;
        write_and_submit(&mut terminal, "only")?;

        wait_queue_with(&terminal, "only")?;
        assert_eq!(
            wait_selected(&terminal, "only")?,
            ">1  #1  pending  agent  only"
        );
        assert_eq!(order(&fixture)?, [(1, "only".to_owned())]);
        let side = if key == "o" { "below" } else { "above" };
        terminal.send(key)?;
        wait_form(&terminal, &format!("New task {side} #1"))?;
        quit_from_form(terminal)?;
    }
    Ok(())
}

#[test]
fn a_refused_task_stays_in_the_form_and_goes_where_the_key_said_once_it_is_accepted() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("jj")?;
    wait_selected(&terminal, "charlie")?;

    terminal.send("O")?;
    wait_form(&terminal, "New task above #3")?;
    terminal.send(SUBMIT)?;
    let screen = terminal.wait_for("the refusal", |screen| {
        screen.contents().contains("! the title is empty")
    })?;
    assert_eq!(order(&fixture)?.len(), 5);
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "New task above #3");

    terminal.send("late")?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}it works{SUBMIT}"))?;

    wait_queue_with(&terminal, "late")?;
    wait_selected(&terminal, "late")?;
    assert_eq!(
        order(&fixture)?,
        [
            (1, "alpha".to_owned()),
            (2, "bravo".to_owned()),
            (6, "late".to_owned()),
            (3, "charlie".to_owned()),
            (4, "delta".to_owned()),
            (5, "echo".to_owned()),
        ]
    );
    quit(terminal)
}

#[test]
fn esc_closes_the_form_and_adds_nothing_and_the_selection_stays() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    wait_selected(&terminal, "bravo")?;

    terminal.send("o")?;
    wait_form(&terminal, "New task below #2")?;
    terminal.send("draft")?;
    terminal.send(ESC)?;

    terminal.wait_for("the queue again", |screen| {
        let contents = screen.contents();
        !contents.contains("New task") && contents.ends_with('┘')
    })?;
    wait_selected(&terminal, "bravo")?;
    assert_eq!(order(&fixture)?.len(), 5);
    // The next `n` is a form for the end of the queue, not for the place the last one asked for.
    terminal.send("n")?;
    wait_form(&terminal, "New task")?;
    quit_from_form(terminal)
}

/// Leaves a screen with the form open: Esc, then quit.
fn quit_from_form(mut terminal: Terminal) -> Result<()> {
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| {
        !screen.contents().contains("New task")
    })?;
    quit(terminal)
}

#[test]
fn a_cancelled_task_selected_refuses_a_task_next_to_it_at_once_without_opening_the_form()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.cli(&["remove", "3"])?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("a")?;
    terminal.wait_for("the cancelled task shown", |screen| {
        screen.contents().contains("cancelled  agent  charlie")
    })?;
    terminal.send("jj")?;
    let row = wait_selected(&terminal, "charlie")?;

    for key in ["o", "O"] {
        terminal.send(key)?;

        let screen = terminal.wait_for("the refusal", |screen| {
            screen.contents().contains("task 3 is cancelled")
        })?;
        let lines = lines_inside_frame(&screen);
        assert_eq!(lines[4], "task 3 is cancelled");
        assert!(!screen.contains("New task"), "{key} opened the form");
        assert_eq!(
            marked(&screen),
            vec![row.clone()],
            "the refusal asked nothing"
        );
        // The same words the CLI gives for placing a task next to the same cancelled one.
        let cli_refusal = fixture.sandbox.run(
            &fixture.repository,
            &["add", "--title", "t", "--criterion", "c", "--after", "3"],
        )?;
        assert_eq!(cli_refusal.code, Some(2));
        assert!(
            cli_refusal.stderr.contains(&lines[4]),
            "{}",
            cli_refusal.stderr
        );

        // The next key dismisses the refusal without having opened anything.
        terminal.send("k")?;
        let screen = wait_selected_screen(&terminal, "bravo")?;
        assert!(!screen.contains("task 3 is cancelled"));
        terminal.send("j")?;
        wait_selected(&terminal, "charlie")?;
    }
    assert_eq!(order(&fixture)?.len(), 5);
    quit(terminal)
}

#[test]
fn the_key_map_lists_o_and_capital_o() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    let screen = terminal.wait_for_text("Keys")?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines.contains(
            &"o, O     add a task below (o) or above (O) the selected one, written in a form"
                .to_owned()
        ),
        "{screen}"
    );
    assert!(
        lines.contains(&"n        add a task at the end, written in a form".to_owned()),
        "{screen}"
    );
    assert!(marked(&screen).is_empty());
    quit(terminal)
}
