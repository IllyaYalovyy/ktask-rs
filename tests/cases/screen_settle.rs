//! Which screen a wait may return, and so which one a test may record.
use crate::support::{
    self, Terminal,
    golden::Input,
    settle::{CYCLE, QUIET, Settle, Watch},
};
use rusqlite::Connection;
use std::time::{Duration, Instant};

#[test]
fn a_screen_is_settled_only_once_the_tui_can_have_refreshed_after_the_store_changed() {
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let mut settle = Settle::new(at(0));
    settle.store(Some(1), at(0));
    settle.frame("a", at(0));
    assert_eq!(settle.due(), at(0) + CYCLE);
    // A change inside the window restarts it, and so does a screen that reads differently.
    settle.store(Some(2), at(600));
    assert_eq!(settle.due(), at(600) + CYCLE);
    settle.frame("b · 9s ", at(900));
    assert_eq!(settle.due(), at(900) + CYCLE);
    // The same screen again, its duration ticked on as a live task redraws it, does not.
    settle.frame("b · 10s", at(1500));
    assert_eq!(settle.due(), at(900) + CYCLE);
    settle.settled();
    // The store has not changed since: quiet is enough, whatever the screen does.
    settle.store(Some(2), at(3000));
    settle.frame("c", at(3000));
    assert_eq!(settle.due(), at(3000) + QUIET);
    settle.store(Some(3), at(4000));
    assert_eq!(settle.due(), at(4000) + CYCLE);
}

#[test]
fn without_a_store_a_quiet_screen_is_settled() {
    let start = Instant::now();
    let mut settle = Settle::new(start);
    settle.store(None, start);
    settle.frame("demo", start);
    assert_eq!(settle.due(), start + QUIET);
}

#[test]
fn the_active_time_heartbeat_is_not_a_change_of_the_store() -> support::Result {
    let sandbox = support::sandbox()?;
    let home = sandbox.path().join("state");
    let mut watch = Watch::new(&home);
    let instance = home.join("ktask-dev");
    assert_eq!(watch.content(), None);
    // Watching never creates a store.
    assert!(!instance.exists());
    std::fs::create_dir_all(&instance)?;
    let writer = Connection::open(instance.join("ktask.sqlite3"))?;
    writer.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE attempts(active_ms INTEGER, state TEXT);
         INSERT INTO attempts VALUES (0, 'running')",
    )?;
    let first = watch.content();
    assert!(first.is_some());
    writer.execute("UPDATE attempts SET active_ms = 1000", [])?;
    assert_eq!(watch.content(), first);
    writer.execute("UPDATE attempts SET state = 'paused'", [])?;
    assert_ne!(watch.content(), first);
    Ok(())
}

#[test]
#[should_panic(expected = "screen \"LOOKED AT\" was only looked at")]
fn a_screen_no_wait_settled_cannot_be_recorded() {
    let terminal = Terminal::open(70, 22).unwrap();
    let seen = support::eventually("a drawn screen", || {
        let screen = terminal.current_screen();
        Ok(screen.contains("Current").then_some(screen))
    })
    .unwrap();
    support::record("LOOKED AT", false, &seen);
}

#[test]
fn a_keyboard_label_after_mouse_input_is_refused() -> support::Result {
    let mut terminal = Terminal::open(100, 28)?;
    let opened = terminal.screen("opened", |s| s.contains("Current"))?;
    // Before any click, either path may name the frame.
    support::accepted("OPENED", Input::Keyboard, &opened)?;
    support::accepted("OPENED", Input::Mouse, &opened)?;
    terminal.click("2 Queue")?;
    let clicked = terminal.screen("queue", |s| s.contains("execution order"))?;
    support::accepted("CLICKED", Input::Mouse, &clicked)?;
    let refusal = support::accepted("CLICKED", Input::Keyboard, &clicked);
    assert!(
        refusal.as_ref().is_err_and(
            |why| why.starts_with("screen \"CLICKED\" is labelled keyboard after mouse input")
        ),
        "{refusal:?}"
    );
    terminal.send("\x03")?;
    terminal.finish()
}
