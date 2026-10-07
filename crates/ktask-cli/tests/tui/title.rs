//! The frame's title bar, on every screen of a dev binary: `ktask-rs [dev]`, so nobody mistakes
//! it for the installed tool. The `user` binary's plain `ktask-rs` is a unit test of the
//! frame in `ktask-tui`: no second binary is built here.

use super::key_maps::open_after_a_run;
use super::navigate::{COLS, ESC, Fixture, ROWS, quit};
use super::pty::Terminal;
use super::support::Result;

const ENTER: &str = "\r";

/// Fails unless the top border of `screen` names the dev channel, with `heading` after it —
/// empty on every screen but the key map, which adds ` · Keys`.
pub(crate) fn assert_dev_title(screen: &str, heading: &str) {
    let top = screen.lines().next().unwrap_or_default();
    let expected = format!("┌ ktask-rs [dev]{heading} ─");
    assert!(
        top.starts_with(&expected),
        "expected {expected:?}:\n{screen}"
    );
}

/// Presses `keys`, waits until `text` is on screen, and checks the title bar there.
fn open_and_check(terminal: &mut Terminal, keys: &str, text: &str, heading: &str) -> Result<()> {
    terminal.send(keys)?;
    let screen = terminal.wait_for(&format!("{text:?} on screen"), |screen| {
        screen.contents().contains(text) && screen.contents().ends_with('┘')
    })?;
    assert_dev_title(&screen, heading);
    Ok(())
}

#[test]
fn every_screen_reachable_from_the_queue_has_the_dev_title() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    assert_dev_title(&terminal.screen(), "");

    open_and_check(&mut terminal, "?", "select the next task", " · Keys")?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue again", |screen| {
        !screen.contents().contains("Keys")
    })?;
    for (keys, text) in [
        ("n", "New task"),
        ("i", "Import tasks"),
        ("s", "Settings"),
        ("p", "Projects"),
        ("D", "Mark task #1 done"),
    ] {
        open_and_check(&mut terminal, keys, text, "")?;
        terminal.send(ESC)?;
        terminal.wait_for("the queue again", |screen| {
            screen.contents().contains("alpha") && !screen.contents().contains(text)
        })?;
    }

    open_and_check(&mut terminal, "v", "Providers", "")?;
    open_and_check(&mut terminal, ENTER, "Provider: claude", "")?;
    terminal.send(ESC)?;
    terminal.wait_for("the providers list", |screen| {
        screen.contents().contains("Providers") && !screen.contents().contains("Provider: ")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue again", |screen| {
        screen.contents().contains("alpha") && !screen.contents().contains("Provider")
    })?;
    assert_dev_title(&terminal.screen(), "");
    quit(terminal)
}

#[test]
fn the_output_screen_has_the_dev_title() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = open_after_a_run(&fixture)?;
    open_and_check(&mut terminal, "l", "close  j/k step", "")?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue again", |screen| {
        !screen.contents().contains("close  j/k step")
    })?;
    quit(terminal)
}

#[test]
fn the_acknowledgement_screen_has_the_dev_title() -> Result<()> {
    let fixture = Fixture::empty()?;
    fixture.cli(&[
        "add",
        "--title",
        "Approve the design",
        "--criterion",
        "approved",
        "--kind",
        "human",
    ])?;
    let mut terminal =
        Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the human task", |screen| {
        screen.contents().contains("human  Approve the design")
    })?;
    open_and_check(&mut terminal, "H", "Acknowledge human task #1", "")?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue again", |screen| {
        !screen.contents().contains("Acknowledge")
    })?;
    quit(terminal)
}
