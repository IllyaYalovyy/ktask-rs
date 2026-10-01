//! Moving around the queue screen: the selection and the keys that move it, cancelled tasks,
//! the key map, and the queue changing under an open screen.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

pub(crate) const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 80;
const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";
pub(crate) const ESC: &str = "\x1b";

/// A sandbox with a git repository called `my-app` whose queue holds `alpha`, `bravo`,
/// `charlie`, `delta` and `echo`, numbered 1 to 5.
pub(crate) struct Fixture {
    pub(crate) sandbox: Sandbox,
    pub(crate) repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left a `run` it started — through the CLI or through the TUI's `r`,
/// which starts one detached on purpose so quitting the screen alone does not stop it —
/// nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    pub(crate) fn new() -> Result<Self> {
        let fixture = Self::empty()?;
        for title in ["alpha", "bravo", "charlie", "delta", "echo"] {
            fixture.add(title)?;
        }
        Ok(fixture)
    }

    pub(crate) fn empty() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    /// Runs `ktask-rs` with `args` inside the repository and expects it to succeed.
    pub(crate) fn cli(&self, args: &[&str]) -> Result<()> {
        let outcome = self.sandbox.run(&self.repository, args)?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    pub(crate) fn add(&self, title: &str) -> Result<()> {
        self.cli(&["add", "--title", title, "--criterion", "it works"])
    }

    /// Opens the queue screen on a terminal of `rows` lines and waits until it is drawn whole,
    /// with the first task marked: the frame is drawn top to bottom, so it is complete once
    /// its last corner is there.
    pub(crate) fn open(&self, rows: u16) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], rows, COLS)?;
        terminal.wait_for("the queue with the first task selected", |screen| {
            let contents = screen.contents();
            contents.ends_with('┘')
                && marked(&contents).contains(&">1  #1  pending  agent  alpha".to_owned())
        })?;
        Ok(terminal)
    }
}

/// The rows of `screen` that carry the selection mark, without the frame.
pub(crate) fn marked(screen: &str) -> Vec<String> {
    lines_inside_frame(screen)
        .into_iter()
        .filter(|line| line.starts_with('>'))
        .collect()
}

/// Waits until the selection is on `title` and nowhere else, and returns the whole matching
/// screen, so a caller checking more of it does not have to read the screen a second time.
pub(crate) fn wait_selected_screen(terminal: &Terminal, title: &str) -> Result<String> {
    terminal.wait_for(
        &format!("the selection on {title}"),
        |screen| matches!(marked(&screen.contents()).as_slice(), [row] if row.ends_with(title)),
    )
}

/// Waits until the selection is on `title` and nowhere else, and returns the row it is on.
pub(crate) fn wait_selected(terminal: &Terminal, title: &str) -> Result<String> {
    let screen = wait_selected_screen(terminal, title)?;
    Ok(marked(&screen).remove(0))
}

/// Presses `keys` one at a time, each time waiting until the selection is on the title
/// that follows it.
fn walk(terminal: &mut Terminal, steps: &[(&str, &str)]) -> Result<()> {
    for (keys, title) in steps {
        terminal.send(keys)?;
        wait_selected(terminal, title)?;
    }
    Ok(())
}

pub(crate) fn quit(mut terminal: Terminal) -> Result<()> {
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_first_task_starts_selected_and_the_selection_is_marked_on_one_row_only() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = fixture.open(ROWS)?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  pending  agent  alpha");
    assert_eq!(lines[5], " 2  #2  pending  agent  bravo");
    assert_eq!(lines[8], " 5  #5  pending  agent  echo");
    assert_eq!(marked(&screen), [lines[4].clone()]);
    quit(terminal)
}

#[test]
fn j_and_down_move_the_selection_down_and_it_stops_at_the_last_task() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;

    walk(
        &mut terminal,
        &[("j", "bravo"), (DOWN, "charlie"), ("j", "delta")],
    )?;
    let row = wait_selected(&terminal, "delta")?;
    assert_eq!(row, ">4  #4  pending  agent  delta");
    walk(&mut terminal, &[(DOWN, "echo")])?;
    // Past the end nothing moves: the key that follows goes up from the last task.
    terminal.send("j")?;
    terminal.send(DOWN)?;
    terminal.send("k")?;
    wait_selected(&terminal, "delta")?;
    quit(terminal)
}

#[test]
fn k_and_up_move_the_selection_up_and_it_stops_at_the_first_task() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    walk(
        &mut terminal,
        &[("G", "echo"), ("k", "delta"), (UP, "charlie")],
    )?;
    walk(&mut terminal, &[(UP, "bravo"), ("k", "alpha")])?;

    // Before the start nothing moves: the key that follows goes down from the first task.
    terminal.send("k")?;
    terminal.send(UP)?;
    terminal.send("j")?;

    wait_selected(&terminal, "bravo")?;
    quit(terminal)
}

#[test]
fn g_goes_to_the_first_task_and_capital_g_to_the_last() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;

    walk(
        &mut terminal,
        &[
            ("G", "echo"),
            ("G", "echo"),
            ("g", "alpha"),
            ("g", "alpha"),
            ("j", "bravo"),
            ("j", "charlie"),
            ("g", "alpha"),
            ("j", "bravo"),
            ("j", "charlie"),
            ("G", "echo"),
        ],
    )?;
    quit(terminal)
}

#[test]
fn a_long_queue_scrolls_so_that_the_selection_stays_on_screen() -> Result<()> {
    let fixture = Fixture::new()?;
    for title in ["foxtrot", "golf", "hotel"] {
        fixture.add(title)?;
    }
    // Ten lines: the frame, three of header and five for tasks; eight tasks do not fit.
    let mut terminal = fixture.open(10)?;
    assert!(!terminal.screen().contains("hotel"));

    terminal.send("G")?;
    let screen = terminal.wait_for("the last task on screen", |screen| {
        screen.contents().contains("hotel")
    })?;
    assert_eq!(marked(&screen), [">8  #8  pending  agent  hotel"]);
    assert!(!screen.contains("alpha"), "{screen}");

    terminal.send("g")?;
    let screen = terminal.wait_for("the first task on screen", |screen| {
        screen.contents().contains("alpha")
    })?;
    assert_eq!(marked(&screen), [">1  #1  pending  agent  alpha"]);
    assert!(!screen.contains("hotel"), "{screen}");
    quit(terminal)
}

#[test]
fn a_toggles_cancelled_tasks_in_their_places_marked_as_cancelled_and_back() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.cli(&["remove", "2"])?;
    let mut terminal = fixture.open(ROWS)?;
    let lines = lines_inside_frame(&terminal.screen());
    assert_eq!(
        lines[2],
        "pending 4 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 1 skipped 0"
    );
    assert_eq!(lines[5], " 2  #3  pending  agent  charlie");
    assert!(!terminal.screen().contains("bravo"));

    terminal.send("a")?;
    let screen = terminal.wait_for("the cancelled task among the others", |screen| {
        screen.contents().contains(" 5  #5  pending    agent  echo")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 4 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 1 skipped 0"
    );
    // `cancelled` (9 characters) is now the widest status shown, so every `pending` row
    // pads out to match it.
    assert_eq!(lines[4], ">1  #1  pending    agent  alpha");
    assert_eq!(lines[5], " 2  #2  cancelled  agent  bravo");
    assert_eq!(lines[6], " 3  #3  pending    agent  charlie");
    assert_eq!(lines[7], " 4  #4  pending    agent  delta");
    assert_eq!(lines[8], " 5  #5  pending    agent  echo");

    // Walking onto the cancelled task marks it like any other.
    walk(&mut terminal, &[("j", "bravo")])?;
    walk(&mut terminal, &[("j", "charlie")])?;
    terminal.send("a")?;
    let screen = terminal.wait_for("the cancelled task hidden", |screen| {
        !screen.contents().contains("bravo")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], " 1  #1  pending  agent  alpha");
    assert_eq!(lines[5], ">2  #3  pending  agent  charlie");
    assert_eq!(lines[8], "");
    quit(terminal)
}

#[test]
fn a_selected_cancelled_task_that_is_hidden_again_gives_the_selection_to_the_next_one() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.cli(&["remove", "2"])?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("a")?;
    wait_selected(&terminal, "alpha")?;
    walk(&mut terminal, &[("j", "bravo")])?;

    terminal.send("a")?;

    wait_selected(&terminal, "charlie")?;
    quit(terminal)
}

#[test]
fn a_queue_with_nothing_cancelled_looks_the_same_with_a_pressed() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    let before = terminal.screen();

    terminal.send("a")?;
    // The next key has an effect that shows only if the toggle has been handled first.
    walk(&mut terminal, &[("j", "bravo")])?;
    terminal.send("a")?;
    terminal.send("k")?;
    let screen = wait_selected_screen(&terminal, "alpha")?;

    assert_eq!(screen, before);
    quit(terminal)
}

#[test]
fn question_mark_shows_the_key_map_and_esc_closes_it_back_to_the_queue() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    walk(&mut terminal, &[("j", "bravo")])?;

    terminal.send("?")?;
    // Waits for the last line of the key map, not just its title, so the frame is whole
    // before its lines are checked one by one.
    let screen = terminal.wait_for("the key map", |screen| {
        screen.contents().contains("q        quit")
    })?;

    let lines = lines_inside_frame(&screen);
    for key in [
        "j, Down  select the next task",
        "k, Up    select the previous task",
        "g        select the first task",
        "G        select the last task",
        "a        show or hide cancelled and skipped tasks",
        "n        add a task at the end, written in a form",
        "d        remove the selected task, after asking",
        "?        show or hide this key map",
        "Esc      close this key map",
        "q        quit",
    ] {
        assert!(lines.contains(&key.to_owned()), "{key:?} in\n{screen}");
    }
    // `y` and a second `n` only answer the removal question — a context this key map is
    // never open alongside — so this key map does not list them, and `n` names only one key.
    assert_eq!(lines.iter().filter(|line| line.starts_with('n')).count(), 1);
    assert!(!lines.iter().any(|line| line.starts_with('y')), "{screen}");
    assert!(!screen.contains("alpha"), "{screen}");
    assert!(!screen.contains("pending"), "{screen}");

    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        let contents = screen.contents();
        !contents.contains("Keys") && contents.contains("echo")
    })?;
    // The selection was not touched by looking at the keys.
    wait_selected(&terminal, "bravo")?;
    quit(terminal)
}

#[test]
fn question_mark_closes_the_key_map_too_and_opens_it_again() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;

    for _ in 0..2 {
        terminal.send("?")?;
        terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
        terminal.send("?")?;
        terminal.wait_for("the queue back", |screen| {
            screen.contents().contains("echo")
        })?;
    }
    wait_selected(&terminal, "alpha")?;
    quit(terminal)
}

#[test]
fn while_the_key_map_is_open_the_other_keys_do_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.cli(&["remove", "3"])?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    let map = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;

    for keys in ["j", DOWN, "G", "a", "k", "g", "x"] {
        terminal.send(keys)?;
    }
    // Events are handled in order: the queue is back only once the keys above were handled.
    terminal.send("?")?;
    terminal.wait_for("the queue back", |screen| {
        screen.contents().contains("echo")
    })?;

    // Neither the selection nor the hidden task changed.
    let screen = wait_selected_screen(&terminal, "alpha")?;
    assert!(!screen.contains("charlie"));
    assert_ne!(screen, map);
    quit(terminal)
}

#[test]
fn q_quits_from_the_key_map_too() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;

    terminal.send("q")?;

    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_bottom_of_the_frame_points_at_the_key_map() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = fixture.open(ROWS)?;

    let screen = terminal.wait_for_text("└ q quit · ? keys ─")?;
    let last = screen.lines().last().unwrap_or_default();
    assert!(last.starts_with("└ q quit · ? keys ─"), "{screen}");
    quit(terminal)
}

#[test]
fn a_task_added_from_the_cli_appears_and_the_selection_stays_on_the_same_task() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    walk(&mut terminal, &[("j", "bravo")])?;

    fixture.cli(&[
        "add",
        "--title",
        "zulu",
        "--criterion",
        "c",
        "--before",
        "1",
    ])?;
    fixture.add("yankee")?;

    let screen = terminal.wait_for("both new tasks", |screen| {
        screen.contents().contains("yankee")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 7 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    assert_eq!(lines[4], " 1  #6  pending  agent  zulu");
    assert_eq!(lines[5], " 2  #1  pending  agent  alpha");
    assert_eq!(lines[6], ">3  #2  pending  agent  bravo");
    assert_eq!(lines[10], " 7  #7  pending  agent  yankee");
    assert_eq!(marked(&screen), [lines[6].clone()]);
    // The selection keeps moving from where it was.
    walk(&mut terminal, &[("j", "charlie")])?;
    quit(terminal)
}

#[test]
fn a_task_added_to_an_empty_queue_appears_selected() -> Result<()> {
    let fixture = Fixture::empty()?;
    let terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for_text("The queue is empty.")?;

    fixture.add("alpha")?;

    let screen = terminal.wait_for("the new task selected", |screen| {
        !marked(&screen.contents()).is_empty()
    })?;
    assert_eq!(marked(&screen), [">1  #1  pending  agent  alpha"]);
    assert!(!screen.contains("The queue is empty."), "{screen}");
    quit(terminal)
}

#[test]
fn a_selected_task_removed_from_the_cli_gives_the_selection_to_the_one_after_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    walk(&mut terminal, &[("j", "bravo"), ("j", "charlie")])?;

    fixture.cli(&["remove", "3"])?;

    let screen = terminal.wait_for("the removed task gone", |screen| {
        !screen.contents().contains("charlie")
    })?;
    assert_eq!(marked(&screen), [">3  #4  pending  agent  delta"]);
    assert!(screen.contains("cancelled 1 skipped 0"), "{screen}");
    quit(terminal)
}

#[test]
fn a_task_removed_from_the_cli_appears_when_cancelled_tasks_are_shown() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("a")?;
    walk(&mut terminal, &[("j", "bravo")])?;

    fixture.cli(&["remove", "1"])?;

    let screen = terminal.wait_for("the removed task cancelled", |screen| {
        screen.contents().contains("cancelled  agent  alpha")
    })?;
    let lines = lines_inside_frame(&screen);
    // `cancelled` (9 characters) is now the widest status shown, so `pending` pads to match.
    assert_eq!(lines[4], " 1  #1  cancelled  agent  alpha");
    assert_eq!(lines[5], ">2  #2  pending    agent  bravo");
    quit(terminal)
}
