//! Adding a task from the queue screen: `n` opens the form, every editing key works in its
//! text areas, Ctrl-S adds through the same use case `ktask-rs add` runs, and Esc adds
//! nothing. What the screen shows after each step, and what `ktask-rs list --json` shows of
//! the journal.

use serde_json::{Value, json};

use super::navigate::{COLS, ESC, Fixture, ROWS, marked, quit};
use super::pty::{Terminal, lines_inside_frame};
use super::support::Result;

pub(crate) const TAB: &str = "\t";
const SHIFT_TAB: &str = "\x1b[Z";
const ENTER: &str = "\r";
const BACKSPACE: &str = "\x7f";
const DELETE: &str = "\x1b[3~";
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";
const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const HOME: &str = "\x1b[H";
const END: &str = "\x1b[F";
pub(crate) const SUBMIT: &str = "\x13";
const ADD_CRITERION: &str = "\x0e";
const REMOVE_CRITERION: &str = "\x04";

/// The column the text of the title and links fields starts at: the frame, the marker, the
/// label.
const TEXT: u16 = 14;
/// The screen row of the title on a form without problems; the rows below follow it.
const TITLE: usize = 3;

/// Opens the queue screen and the form over it.
pub(crate) fn open_form(fixture: &Fixture) -> Result<Terminal> {
    let mut terminal =
        Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    terminal.send("n")?;
    terminal.wait_for("the form", |screen| screen.contents().contains("New task"))?;
    Ok(terminal)
}

/// Waits until the rows of the screen inside the frame hold `rows` at their indexes and the
/// cursor is at (`row`, `col`) on the screen; returns the whole matching screen, so a caller
/// checking more of it does not have to read the screen a second time.
fn expect(terminal: &Terminal, rows: &[(usize, &str)], cursor: (usize, u16)) -> Result<String> {
    terminal.wait_for(
        &format!("{rows:?} with the cursor at {cursor:?}"),
        |screen| {
            let lines = lines_inside_frame(&screen.contents());
            let (row, col) = screen.cursor_position();
            !screen.hide_cursor()
                && (usize::from(row), col) == cursor
                && rows
                    .iter()
                    .all(|(index, text)| lines.get(*index).is_some_and(|line| line == text))
        },
    )
}

/// Waits until the rows hold `rows` at their indexes, wherever the cursor is.
fn expect_rows(terminal: &Terminal, rows: &[(usize, &str)]) -> Result<Vec<String>> {
    let screen = terminal.wait_for(&format!("{rows:?}"), |screen| {
        let lines = lines_inside_frame(&screen.contents());
        rows.iter()
            .all(|(index, text)| lines.get(*index).is_some_and(|line| line == text))
    })?;
    Ok(lines_inside_frame(&screen))
}

/// Every task `list --json` shows, without the creation time.
pub(crate) fn listed(fixture: &Fixture) -> Result<Vec<Value>> {
    let outcome = fixture
        .sandbox
        .run(&fixture.repository, &["list", "--all", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let Value::Array(mut tasks) = serde_json::from_str(&outcome.stdout)? else {
        return Err("list --json is not an array".into());
    };
    for task in &mut tasks {
        task.as_object_mut()
            .ok_or("a task is not an object")?
            .remove("created_at");
    }
    Ok(tasks)
}

/// Waits until the form is gone and the queue shows `text`.
pub(crate) fn wait_queue_with(terminal: &Terminal, text: &str) -> Result<String> {
    terminal.wait_for(&format!("the queue showing {text:?}"), |screen| {
        let contents = screen.contents();
        !contents.contains("New task") && contents.contains(text) && contents.ends_with('┘')
    })
}

#[test]
fn n_opens_an_empty_form_over_the_queue_with_the_cursor_in_the_title() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = open_form(&fixture)?;

    let screen = expect(&terminal, &[(TITLE, "> Title:")], (TITLE, TEXT))?;
    let lines = lines_inside_frame(&screen);

    assert_eq!(lines[1], "New task");
    assert_eq!(lines[2], "");
    assert_eq!(lines[3], "> Title:");
    assert_eq!(lines[4], "  Kind:      < agent >");
    assert_eq!(lines[5], "  Links:");
    assert_eq!(lines[6], "  Body:");
    assert_eq!(lines[7], "");
    assert_eq!(lines[8], "  Criteria:");
    assert_eq!(lines[9], "   1.");
    assert_eq!(lines[10], "  Provider:");
    assert_eq!(lines[11], "  Model:");
    assert!(!screen.contains("alpha"), "{screen}");
    let bottom = screen
        .lines()
        .nth(usize::from(ROWS) - 1)
        .unwrap_or_default();
    assert!(
        bottom.starts_with("└ Ctrl-S add · Esc cancel · Tab, Shift-Tab field"),
        "{bottom}"
    );
    assert!(bottom.contains("Ctrl-N, Ctrl-D criterion"), "{bottom}");
    assert!(
        !bottom.contains("Ctrl-P") && !bottom.contains("Ctrl-O"),
        "{bottom}"
    );
    assert_eq!(listed(&fixture)?.len(), 5);
    Ok(())
}

#[test]
fn ctrl_p_and_ctrl_o_do_nothing_on_the_form() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("T\x10\x0fx")?;
    expect_rows(&terminal, &[(3, "> Title:     Tx"), (10, "  Provider:")])?;
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());
    quit_after_esc(terminal)
}

fn quit_after_esc(mut terminal: Terminal) -> Result<()> {
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("New task") && screen.contents().ends_with('┘')
    })?;
    quit(terminal)
}

#[test]
fn an_unknown_provider_is_refused_in_the_form_in_the_words_of_add_provider() -> Result<()> {
    let fixture = Fixture::empty()?;
    let refused = fixture.sandbox.run(
        &fixture.repository,
        &[
            "add",
            "--title",
            "t",
            "--criterion",
            "c",
            "--provider",
            "nope",
        ],
    )?;
    let words = refused
        .stderr
        .lines()
        .find_map(|line| line.find("unknown provider").map(|at| &line[at..]))
        .ok_or("add --provider names no unknown provider")?
        .to_owned();
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("T{TAB}{TAB}{TAB}{TAB}c{TAB}nope{SUBMIT}"))?;

    expect_rows(&terminal, &[(2, &format!("! {words}"))])?;
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());

    terminal.send(&format!(
        "{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}codex{SUBMIT}"
    ))?;
    wait_queue_with(&terminal, "T · codex")?;
    assert_eq!(listed(&fixture)?[0]["provider"], "codex");
    quit(terminal)
}

#[test]
fn the_task_form_stores_its_provider_and_model_and_the_queue_shows_them() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!(
        "T{TAB}{TAB}{TAB}{TAB}c{TAB}codex{TAB}gpt-5{SUBMIT}"
    ))?;
    wait_queue_with(&terminal, "T · codex (gpt-5)")?;
    let tasks = listed(&fixture)?;
    assert_eq!(tasks[0]["provider"], "codex");
    assert_eq!(tasks[0]["model"], "gpt-5");
    quit(terminal)
}

#[test]
fn the_queue_key_map_lists_n() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    let screen = terminal.wait_for_text("Keys")?;
    assert!(
        screen.contains("n        add a task at the end, written in a form"),
        "{screen}"
    );
    Ok(())
}

#[test]
fn filling_every_field_and_submitting_adds_the_task_at_the_end_as_typed() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;

    terminal.send("Ship the form")?;
    terminal.send(TAB)?;
    terminal.send(" ")?;
    terminal.send(TAB)?;
    terminal.send("github:owner/repo#7 https://example.com/a")?;
    terminal.send(TAB)?;
    terminal.send(&format!(
        "first line{ENTER}second line{ENTER}{ENTER}fourth — ünïcode"
    ))?;
    terminal.send(TAB)?;
    terminal.send("It adds and it lists")?;
    terminal.send(ADD_CRITERION)?;
    terminal.send("It says \"no\" to a blank title")?;
    let lines = lines_inside_frame(&expect(
        &terminal,
        &[
            (11, "  Criteria:"),
            (12, "   1. It adds and it lists"),
            (13, ">  2. It says \"no\" to a blank title"),
        ],
        (13, 1 + 6 + 29),
    )?);
    assert_eq!(lines[3], "  Title:     Ship the form");
    assert_eq!(lines[4], "  Kind:      < human >");
    assert_eq!(
        lines[5],
        "  Links:     github:owner/repo#7 https://example.com/a"
    );
    assert_eq!(lines[7], "    first line");
    assert_eq!(lines[8], "    second line");
    assert_eq!(lines[9], "");
    assert_eq!(lines[10], "    fourth — ünïcode");
    // Nothing is added until it is submitted.
    assert_eq!(listed(&fixture)?.len(), 5);

    terminal.send(SUBMIT)?;

    let screen = wait_queue_with(&terminal, "Ship the form")?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 6 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  alpha");
    assert_eq!(lines[9], " 6  #6  pending  human  Ship the form");
    assert_eq!(marked(&screen), [lines[4].clone()]);
    let tasks = listed(&fixture)?;
    assert_eq!(tasks.len(), 6);
    assert_eq!(
        tasks[5],
        json!({
            "id": 6,
            "position": 6,
            "title": "Ship the form",
            "body": "first line\nsecond line\n\nfourth — ünïcode",
            "criteria": ["It adds and it lists", "It says \"no\" to a blank title"],
            "kind": "human",
            "links": ["github:owner/repo#7", "https://example.com/a"],
            "status": "pending",
        })
    );
    assert_eq!(tasks[4]["title"], "echo");
    quit(terminal)
}

#[test]
fn a_task_added_from_the_form_is_the_one_the_add_command_would_add() -> Result<()> {
    let typed = Fixture::empty()?;
    let mut terminal = open_form(&typed)?;
    terminal.send("Same task")?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}the one criterion"))?;
    terminal.send(SUBMIT)?;
    wait_queue_with(&terminal, "Same task")?;
    let by_cli = Fixture::empty()?;
    by_cli.cli(&[
        "add",
        "--title",
        "Same task",
        "--criterion",
        "the one criterion",
    ])?;

    assert_eq!(listed(&typed)?, listed(&by_cli)?);
    quit(terminal)
}

#[test]
fn submitting_an_empty_form_shows_every_problem_in_the_form_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;

    terminal.send(SUBMIT)?;

    let lines = expect_rows(
        &terminal,
        &[
            (1, "New task"),
            (2, "! the title is empty: a task needs a title"),
            (3, "! an acceptance criterion is empty"),
            (4, ""),
            (5, "> Title:"),
        ],
    )?;
    assert_eq!(lines[6], "  Kind:      < agent >");
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());
    // The form is still there, and works: the queue keys are text, not commands.
    terminal.send("q")?;
    expect(&terminal, &[(5, "> Title:     q")], (5, TEXT + 1))?;
    assert_eq!(terminal.exit(), None);
    Ok(())
}

#[test]
fn a_form_with_no_criterion_shows_that_and_a_criterion_added_lets_it_through() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("Needs a criterion")?;
    terminal.send(&format!("{SHIFT_TAB}{SHIFT_TAB}{SHIFT_TAB}"))?;
    terminal.send(REMOVE_CRITERION)?;
    expect_rows(&terminal, &[(9, "    none: Ctrl-N adds one")])?;

    terminal.send(SUBMIT)?;

    let lines = expect_rows(
        &terminal,
        &[
            (2, "! a task needs at least one acceptance criterion"),
            (3, ""),
            (4, "  Title:     Needs a criterion"),
        ],
    )?;
    assert!(
        !lines.iter().any(|line| line.contains("title is empty")),
        "{lines:?}"
    );
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());

    terminal.send(ADD_CRITERION)?;
    terminal.send("now there is one")?;
    terminal.send(SUBMIT)?;

    wait_queue_with(&terminal, "Needs a criterion")?;
    let tasks = listed(&fixture)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["criteria"], json!(["now there is one"]));
    quit(terminal)
}

#[test]
fn a_blank_title_or_a_blank_criterion_is_refused_each_on_its_own() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("   ")?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}a criterion"))?;
    terminal.send(SUBMIT)?;
    let lines = expect_rows(
        &terminal,
        &[
            (2, "! the title is empty: a task needs a title"),
            (3, ""),
            (4, "  Title:"),
        ],
    )?;
    assert!(
        !lines.iter().any(|line| line.contains("criterion is empty")),
        "{lines:?}"
    );
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("New task")
    })?;

    terminal.send("n")?;
    terminal.send(&format!("Titled{TAB}{TAB}{TAB}{TAB}  {ENTER}  "))?;
    terminal.send(SUBMIT)?;

    let lines = expect_rows(
        &terminal,
        &[(2, "! an acceptance criterion is empty"), (3, "")],
    )?;
    assert!(
        !lines.iter().any(|line| line.contains("title is empty")),
        "{lines:?}"
    );
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn a_malformed_link_is_refused_naming_it_and_nothing_is_added() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("Title{TAB}{TAB}not-a-link{TAB}{TAB}c"))?;

    terminal.send(SUBMIT)?;

    expect_rows(
        &terminal,
        &[
            (
                2,
                "! malformed link \"not-a-link\": expected github:owner/repo#NUMBER or an http(s)",
            ),
            (3, "  URL"),
            (4, ""),
        ],
    )?;
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn esc_cancels_the_form_and_adds_nothing_and_the_next_form_is_empty() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("Never added{TAB}{TAB}{TAB}{TAB}nor this"))?;
    expect_rows(&terminal, &[(3, "  Title:     Never added")])?;
    let before = listed(&fixture)?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("New task")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 5 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    assert_eq!(lines[4], ">1  #1  pending  agent  alpha");
    assert_eq!(lines[8], " 5  #5  pending  agent  echo");
    assert!(!screen.contains("Never added"), "{screen}");
    assert_eq!(listed(&fixture)?, before);
    // The queue answers its keys again, and the form starts empty.
    terminal.send("j")?;
    terminal.wait_for("bravo selected", |screen| {
        marked(&screen.contents())
            .iter()
            .any(|row| row.ends_with("bravo"))
    })?;
    terminal.send("n")?;
    expect(&terminal, &[(3, "> Title:"), (9, "   1.")], (3, TEXT))?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("New task")
    })?;
    quit(terminal)
}

#[test]
fn esc_after_a_refused_submission_leaves_the_queue_as_it_was() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(SUBMIT)?;
    expect_rows(
        &terminal,
        &[(2, "! the title is empty: a task needs a title")],
    )?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the empty queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    assert!(!screen.contains("title is empty"), "{screen}");
    assert_eq!(listed(&fixture)?, Vec::<Value>::new());
    quit(terminal)
}

#[test]
fn typing_backspace_delete_and_the_arrows_edit_the_title() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;

    terminal.send("abcd")?;
    expect(
        &terminal,
        &[(TITLE, "> Title:     abcd")],
        (TITLE, TEXT + 4),
    )?;
    terminal.send(&format!("{LEFT}{LEFT}"))?;
    expect(&terminal, &[], (TITLE, TEXT + 2))?;
    terminal.send(BACKSPACE)?;
    expect(&terminal, &[(TITLE, "> Title:     acd")], (TITLE, TEXT + 1))?;
    terminal.send("X")?;
    expect(
        &terminal,
        &[(TITLE, "> Title:     aXcd")],
        (TITLE, TEXT + 2),
    )?;
    terminal.send(DELETE)?;
    expect(&terminal, &[(TITLE, "> Title:     aXd")], (TITLE, TEXT + 2))?;
    terminal.send(RIGHT)?;
    expect(&terminal, &[], (TITLE, TEXT + 3))?;
    // At the end the cursor stays and Delete removes nothing.
    terminal.send(&format!("{RIGHT}{DELETE}"))?;
    expect(&terminal, &[(TITLE, "> Title:     aXd")], (TITLE, TEXT + 3))?;
    terminal.send(HOME)?;
    expect(&terminal, &[], (TITLE, TEXT))?;
    // At the start the cursor stays and Backspace removes nothing.
    terminal.send(&format!("{LEFT}{BACKSPACE}"))?;
    expect(&terminal, &[(TITLE, "> Title:     aXd")], (TITLE, TEXT))?;
    terminal.send(&format!("{DELETE}{END}"))?;
    expect(&terminal, &[(TITLE, "> Title:     Xd")], (TITLE, TEXT + 2))?;
    terminal.send(&format!(
        "é{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}{BACKSPACE}"
    ))?;
    expect(&terminal, &[(TITLE, "> Title:")], (TITLE, TEXT))?;
    terminal.send("Title")?;
    // A title has one line: Enter, Up and Down do nothing to it.
    terminal.send(&format!("{ENTER}{UP}{DOWN}"))?;
    terminal.send("!")?;
    expect(
        &terminal,
        &[
            (TITLE, "> Title:     Title!"),
            (4, "  Kind:      < agent >"),
        ],
        (TITLE, TEXT + 6),
    )?;
    Ok(())
}

#[test]
fn enter_arrows_home_end_backspace_and_delete_edit_the_body_across_lines() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("T{TAB}{TAB}{TAB}"))?;
    expect(&terminal, &[(6, "> Body:")], (7, 4 + 1))?;

    terminal.send(&format!("one{ENTER}two{ENTER}three"))?;
    expect(
        &terminal,
        &[
            (7, "    one"),
            (8, "    two"),
            (9, "    three"),
            (10, "  Criteria:"),
        ],
        (9, 4 + 1 + 5),
    )?;
    terminal.send(UP)?;
    expect(&terminal, &[], (8, 5 + 3))?;
    terminal.send(HOME)?;
    expect(&terminal, &[], (8, 5))?;
    terminal.send(LEFT)?;
    // Left at the start of a line goes to the end of the one above.
    expect(&terminal, &[], (7, 5 + 3))?;
    terminal.send(RIGHT)?;
    expect(&terminal, &[], (8, 5))?;
    terminal.send(&format!("{DOWN}{END}"))?;
    expect(&terminal, &[], (9, 5 + 5))?;
    terminal.send(DOWN)?;
    // Nothing below the last line.
    terminal.send(&format!("{UP}{UP}{UP}"))?;
    expect(&terminal, &[], (7, 5 + 3))?;
    // Enter in the middle of a line breaks it there.
    terminal.send(&format!("{LEFT}{ENTER}"))?;
    expect(
        &terminal,
        &[
            (7, "    on"),
            (8, "    e"),
            (9, "    two"),
            (10, "    three"),
        ],
        (8, 5),
    )?;
    // Backspace at the start of a line joins it to the one above.
    terminal.send(BACKSPACE)?;
    expect(
        &terminal,
        &[(7, "    one"), (8, "    two"), (9, "    three")],
        (7, 5 + 2),
    )?;
    // Delete at the end of a line joins the one below to it.
    terminal.send(&format!("{END}{DELETE}"))?;
    expect(
        &terminal,
        &[(7, "    onetwo"), (8, "    three"), (9, "  Criteria:")],
        (7, 5 + 3),
    )?;
    terminal.send(&format!("{DOWN}{HOME}{DELETE}{BACKSPACE}"))?;
    expect(
        &terminal,
        &[(7, "    onetwohree"), (8, "  Criteria:")],
        (7, 5 + 6),
    )?;

    terminal.send(&format!("{TAB}c{SUBMIT}"))?;
    wait_queue_with(&terminal, "#1")?;
    let tasks = listed(&fixture)?;
    assert_eq!(tasks[0]["body"], "onetwohree");
    Ok(())
}

#[test]
fn the_body_keeps_its_line_breaks_and_a_criterion_ignores_enter() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("Lines{TAB}{TAB}{TAB}a{ENTER}{ENTER}b{ENTER}"))?;
    terminal.send(&format!("{TAB}x{ENTER}y{ADD_CRITERION}{ENTER}z"))?;
    expect_rows(
        &terminal,
        &[
            (7, "    a"),
            (8, ""),
            (9, "    b"),
            (10, ""),
            (11, "  Criteria:"),
            (12, "   1. xy"),
            (13, ">  2. z"),
        ],
    )?;

    terminal.send(SUBMIT)?;

    wait_queue_with(&terminal, "#1")?;
    let tasks = listed(&fixture)?;
    assert_eq!(tasks[0]["body"], "a\n\nb\n");
    assert_eq!(tasks[0]["criteria"], json!(["xy", "z"]));
    Ok(())
}

#[test]
fn criteria_are_added_after_the_last_and_removed_from_where_the_focus_is() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("Many{TAB}{TAB}{TAB}{TAB}one"))?;
    terminal.send(&format!("{ADD_CRITERION}two{ADD_CRITERION}three"))?;
    expect(
        &terminal,
        &[(9, "   1. one"), (10, "   2. two"), (11, ">  3. three")],
        (11, 7 + 5),
    )?;

    // Removing the focused criterion renumbers the rest, and the focus takes the next.
    terminal.send(SHIFT_TAB)?;
    expect(
        &terminal,
        &[(9, "   1. one"), (10, ">  2. two"), (11, "   3. three")],
        (10, 7 + 3),
    )?;
    terminal.send(REMOVE_CRITERION)?;
    expect(
        &terminal,
        &[(9, "   1. one"), (10, ">  2. three"), (11, "  Provider:")],
        (10, 7 + 5),
    )?;
    // The last one removed leaves the focus on the one before it.
    terminal.send(REMOVE_CRITERION)?;
    expect(
        &terminal,
        &[(9, ">  1. one"), (10, "  Provider:")],
        (9, 7 + 3),
    )?;
    // Ctrl-D outside a criterion removes nothing.
    terminal.send(&format!("{SHIFT_TAB}{REMOVE_CRITERION}"))?;
    expect(&terminal, &[(8, "  Criteria:"), (9, "   1. one")], (7, 5))?;
    terminal.send(&format!("{TAB}{REMOVE_CRITERION}"))?;
    expect_rows(
        &terminal,
        &[(9, "    none: Ctrl-N adds one"), (10, "  Provider:")],
    )?;
    // With none left, Ctrl-N adds one and the focus is on it.
    terminal.send(&format!("{ADD_CRITERION}again"))?;
    expect(&terminal, &[(9, ">  1. again")], (9, 7 + 5))?;

    terminal.send(SUBMIT)?;

    wait_queue_with(&terminal, "#1")?;
    assert_eq!(listed(&fixture)?[0]["criteria"], json!(["again"]));
    Ok(())
}

/// Waits until the focus marker is on `row` and nowhere else.
fn wait_focus(terminal: &Terminal, row: usize) -> Result<()> {
    terminal.wait_for(&format!("the focus on row {row}"), |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.iter().filter(|line| line.starts_with('>')).count() == 1
            && lines.get(row).is_some_and(|line| line.starts_with('>'))
    })?;
    Ok(())
}

#[test]
fn tab_and_shift_tab_walk_every_field_and_wrap_around_in_both_directions() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("{ADD_CRITERION}{TAB}{TAB}{TAB}"))?;
    // Title, kind, links, body, criterion 1, criterion 2, provider, model: rows 3, 4, 5, 6,
    // 9, 10, 11, 12.
    wait_focus(&terminal, 3)?;
    // The cursor is in the field the focus is on, and hidden on the kind.
    for row in [4, 5, 6, 9, 10, 11, 12, 3, 4] {
        terminal.send(TAB)?;
        wait_focus(&terminal, row)?;
        terminal.wait_for("the cursor", |screen| screen.hide_cursor() == (row == 4))?;
    }
    for row in [3, 12, 11, 10, 9, 6, 5, 4, 3] {
        terminal.send(SHIFT_TAB)?;
        wait_focus(&terminal, row)?;
        terminal.wait_for("the cursor", |screen| screen.hide_cursor() == (row == 4))?;
    }
    terminal.send(&format!("{TAB}{TAB}"))?;
    expect(&terminal, &[(5, "> Links:")], (5, TEXT))?;
    terminal.send(TAB)?;
    expect(&terminal, &[(6, "> Body:")], (7, 5))?;
    Ok(())
}

#[test]
fn the_kind_changes_with_left_right_and_space_and_takes_no_typing() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(TAB)?;
    let hint = "> Kind:      < agent >   Left, Right or Space to change";
    terminal.wait_for("the hint on the kind", |screen| {
        lines_inside_frame(&screen.contents()).contains(&hint.to_owned())
    })?;

    for (key, kind) in [
        (RIGHT, "human"),
        (LEFT, "agent"),
        (" ", "human"),
        (" ", "agent"),
    ] {
        terminal.send(key)?;
        expect_rows(
            &terminal,
            &[(
                4,
                &format!("> Kind:      < {kind} >   Left, Right or Space to change"),
            )],
        )?;
    }
    terminal.send(&format!("h{ENTER}{BACKSPACE}{UP}{DOWN}{HOME}{END}{DELETE}"))?;
    terminal.send(RIGHT)?;
    expect_rows(
        &terminal,
        &[
            (3, "  Title:"),
            (4, "> Kind:      < human >   Left, Right or Space to change"),
        ],
    )?;

    terminal.send(&format!("{SHIFT_TAB}Kinded{TAB}{TAB}{TAB}{TAB}c{SUBMIT}"))?;

    wait_queue_with(&terminal, "#1")?;
    let tasks = listed(&fixture)?;
    assert_eq!(
        (tasks[0]["title"].clone(), tasks[0]["kind"].clone()),
        (json!("Kinded"), json!("human"))
    );
    quit(terminal)
}

#[test]
fn a_long_body_scrolls_to_keep_the_cursor_in_view_and_all_of_it_is_added() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send(&format!("Long{TAB}{TAB}{TAB}"))?;
    let body: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
    terminal.send(&body.join(ENTER))?;

    // The last line is at the bottom of the frame; the top of the form has scrolled away.
    let lines = lines_inside_frame(&expect(&terminal, &[(22, "    line 30")], (22, 5 + 7))?);
    assert!(
        !lines.iter().any(|line| line.contains("New task")),
        "{lines:?}"
    );
    assert!(lines.contains(&"    line 9".to_owned()), "{lines:?}");
    assert!(!lines.contains(&"    line 8".to_owned()), "{lines:?}");
    terminal.send(&format!(
        "{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}{UP}"
    ))?;
    expect(&terminal, &[(14, "    line 8")], (14, 5 + 6))?;
    terminal.send(&format!("{TAB}c{SUBMIT}"))?;

    wait_queue_with(&terminal, "#1")?;
    assert_eq!(listed(&fixture)?[0]["body"], body.join("\n"));
    Ok(())
}

#[test]
fn a_long_line_scrolls_sideways_to_keep_the_cursor_in_view() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_form(&fixture)?;
    let title = "abcdefghij".repeat(8);
    terminal.send(&title)?;

    // The row shows the end of the title, and the cursor is at the right edge of the frame.
    expect(
        &terminal,
        &[(TITLE, &format!("> Title:     {}", &title[16..]))],
        (TITLE, 78),
    )?;
    terminal.send(HOME)?;
    expect(
        &terminal,
        &[(TITLE, &format!("> Title:     {}", &title[..65]))],
        (TITLE, TEXT),
    )?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}c{SUBMIT}"))?;

    wait_queue_with(&terminal, "#1")?;
    assert_eq!(listed(&fixture)?[0]["title"], title);
    Ok(())
}

#[test]
fn ctrl_keys_do_nothing_on_the_queue_and_a_form_opens_again_afterwards() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;

    terminal.send(&format!("{SUBMIT}{ADD_CRITERION}{REMOVE_CRITERION}j"))?;

    terminal.wait_for("bravo selected and no form", |screen| {
        let contents = screen.contents();
        !contents.contains("New task") && marked(&contents).iter().any(|row| row.ends_with("bravo"))
    })?;
    assert_eq!(listed(&fixture)?.len(), 5);
    quit(terminal)
}

#[test]
fn a_task_added_from_the_cli_while_the_form_is_open_shows_once_the_form_is_closed() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("Half typed")?;

    fixture.add("from the cli")?;
    // The form is untouched by the queue changing behind it.
    terminal.send("!")?;
    expect(
        &terminal,
        &[(TITLE, "> Title:     Half typed!")],
        (TITLE, TEXT + 11),
    )?;
    terminal.send(&format!("{TAB}{TAB}{TAB}{TAB}c{SUBMIT}"))?;

    let screen = wait_queue_with(&terminal, "Half typed!")?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 7 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    assert_eq!(lines[9], " 6  #6  pending  agent  from the cli");
    assert_eq!(lines[10], " 7  #7  pending  agent  Half typed!");
    quit(terminal)
}
