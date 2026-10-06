//! `?` on every screen that is not a text field: the same key overlay the queue has, listing
//! that screen's own keys, closed again by `?` or Esc back onto the very same screen.

use super::navigate::{COLS, ESC, Fixture, ROWS, quit};
use super::pty::{Terminal, lines_inside_frame};
use super::support::Result;

const ENTER: &str = "\r";
const BODY: &str = "```bash\ncase \"$3\" in\nreview) ktask-rs report --token \"$1\" approved ;;\ntesting) ktask-rs report --token \"$1\" accepted ;;\n*) ktask-rs report --token \"$1\" done ;;\nesac\n```\n";

/// A queue whose one task has already run, so it has output to show.
fn open_after_a_run(fixture: &Fixture) -> Result<Terminal> {
    fixture.cli(&["settings", "set", "model", "m-impl"])?;
    fixture.cli(&[
        "add",
        "--title",
        "alpha",
        "--criterion",
        "it works",
        "--body",
        BODY,
    ])?;
    let run = fixture.sandbox.run(&fixture.repository, &["run"])?;
    assert_eq!(run.code, Some(0), "{}{}", run.stdout, run.stderr);
    let terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("alpha") && screen.contents().ends_with('┘')
    })?;
    Ok(terminal)
}

/// Presses `?` on the screen showing now and checks the overlay lists `keys`, hides the screen
/// itself, ignores other keys, and that `?` and then Esc each bring back exactly this screen.
fn key_map_lists(terminal: &mut Terminal, screen_name: &str, keys: &[&str]) -> Result<()> {
    let before = terminal.screen();
    terminal.send("?")?;
    let last = keys.last().copied().unwrap_or_default();
    let map = terminal.wait_for(&format!("the {screen_name} key map"), |screen| {
        screen.contents().contains("Keys") && screen.contents().contains(last)
    })?;
    let lines = lines_inside_frame(&map);
    for key in keys {
        assert!(lines.contains(&(*key).to_owned()), "{key:?} in\n{map}");
    }
    assert!(
        lines.iter().any(|line| line.starts_with('?')),
        "the map lists `?` itself on the {screen_name}: {map}"
    );

    // Keys other than `?` and Esc do nothing while the map is up.
    terminal.send("jkc\r")?;
    assert_eq!(terminal.screen(), map, "{screen_name} map reacted to a key");

    terminal.send("?")?;
    let closed = terminal.wait_for(&format!("the {screen_name} back after `?`"), |screen| {
        !screen.contents().contains("Keys")
    })?;
    assert_eq!(closed, before);

    terminal.send("?")?;
    terminal.wait_for(&format!("the {screen_name} key map again"), |screen| {
        screen.contents().contains("Keys")
    })?;
    terminal.send(ESC)?;
    let closed = terminal.wait_for(&format!("the {screen_name} back after Esc"), |screen| {
        !screen.contents().contains("Keys")
    })?;
    assert_eq!(closed, before);
    Ok(())
}

#[test]
fn question_mark_opens_a_key_map_on_the_queue_the_providers_the_picker_and_the_output() -> Result<()>
{
    let fixture = Fixture::empty()?;
    let mut terminal = open_after_a_run(&fixture)?;

    key_map_lists(
        &mut terminal,
        "queue",
        &["j, Down  select the next task", "q        quit"],
    )?;

    terminal.send("v")?;
    terminal.wait_for("the providers list", |screen| {
        screen.contents().contains("Providers")
    })?;
    key_map_lists(
        &mut terminal,
        "providers list",
        &[
            "Enter            show the selected provider's definition",
            "Esc              close this key map, or go back to the queue",
        ],
    )?;
    terminal.send(ENTER)?;
    terminal.wait_for("the provider definition", |screen| {
        screen.contents().contains("Provider: claude")
    })?;
    key_map_lists(
        &mut terminal,
        "provider definition",
        &[
            "c                check the provider is ready to use",
            "Esc              close this key map, or go back to the list",
        ],
    )?;
    terminal.send(ESC)?;
    terminal.wait_for("the providers list", |screen| {
        screen.contents().contains("Providers") && !screen.contents().contains("Provider: ")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| screen.contents().contains("alpha"))?;

    terminal.send("p")?;
    terminal.wait_for("the project picker", |screen| {
        screen.contents().contains("Projects")
    })?;
    key_map_lists(&mut terminal, "project picker", &["Esc    cancel"])?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| screen.contents().contains("alpha"))?;

    terminal.send("l")?;
    terminal.wait_for("the output screen", |screen| {
        screen.contents().contains("j/k step")
    })?;
    key_map_lists(
        &mut terminal,
        "output",
        &[
            "[ / ]            show the previous / next attempt",
            "l                close the output",
            "Esc              close this key map, or the output",
        ],
    )?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| screen.contents().contains("alpha"))?;
    quit(terminal)
}

#[test]
fn question_mark_is_text_on_the_screens_that_take_text() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("n")?;
    terminal.wait_for("the form", |screen| screen.contents().contains("New task"))?;

    terminal.send("why?")?;

    let screen = terminal.wait_for("the question mark typed", |screen| {
        screen.contents().contains("why?")
    })?;
    assert!(!screen.contains("Keys"), "{screen}");
    Ok(())
}
