//! The TUI always gives the terminal back: Ctrl-C from every screen, with a discard question
//! guarding a form that holds something typed; SIGTERM and SIGHUP; the pseudo-terminal itself
//! going away; and a panic. Every case is proven through the real binary, in a pseudo-terminal,
//! asserting the process has ended and the terminal is back in cooked mode.

use nix::sys::signal::Signal;

use super::add::open_form;
use super::navigate::{ESC, Fixture, ROWS, quit};
use super::pty::{HungUp, Terminal, lines_inside_frame};
use super::support::{Result, Sandbox};

const CTRL_C: &str = "\x03";

/// Sends Ctrl-C and waits for the process to exit with code 0 and the terminal restored.
fn ctrl_c_quits(mut terminal: Terminal) -> Result<()> {
    terminal.send(CTRL_C)?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    assert!(terminal.is_cooked()?, "line editing and echo are back on");
    Ok(())
}

#[test]
fn ctrl_c_quits_the_queue_screen_the_same_as_q() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = fixture.open(ROWS)?;
    ctrl_c_quits(terminal)
}

#[test]
fn ctrl_c_quits_the_key_map() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("?")?;
    terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    ctrl_c_quits(terminal)
}

#[test]
fn ctrl_c_quits_a_removal_question_and_removes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("d")?;
    terminal.wait_for("the removal question", |screen| {
        screen.contents().contains("Remove #")
    })?;
    ctrl_c_quits(terminal)
}

#[test]
fn ctrl_c_quits_an_empty_add_form_without_asking_to_discard() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = open_form(&fixture)?;
    ctrl_c_quits(terminal)
}

#[test]
fn ctrl_c_in_a_form_with_content_asks_to_discard_and_y_quits() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("Half typed")?;

    terminal.send(CTRL_C)?;

    let screen = terminal.wait_for("the discard question", |screen| {
        screen.contents().contains("Discard this task?")
    })?;
    assert!(
        screen.contains("y to discard · n or Esc to keep writing"),
        "{screen}"
    );
    assert!(screen.contains("Half typed"), "{screen}");

    terminal.send("y")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    assert!(terminal.is_cooked()?, "line editing and echo are back on");
    Ok(())
}

#[test]
fn a_second_ctrl_c_quits_from_the_discard_question() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("Half typed")?;
    terminal.send(CTRL_C)?;
    terminal.wait_for("the discard question", |screen| {
        screen.contents().contains("Discard this task?")
    })?;

    ctrl_c_quits(terminal)
}

#[test]
fn question_mark_shows_the_discard_questions_own_keys_and_esc_closes_it_back_to_the_question()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = open_form(&fixture)?;
    terminal.send("Half typed")?;
    terminal.send(CTRL_C)?;
    let asked = terminal.wait_for("the discard question", |screen| {
        screen.contents().contains("Discard this task?")
    })?;

    terminal.send("?")?;
    let screen = terminal.wait_for("the discard question's key map", |screen| {
        screen.contents().contains("Keys")
    })?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines.contains(&"y       discard it".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"n, Esc  keep writing".to_owned()),
        "{lines:?}"
    );
    assert!(!screen.contains("Half typed"), "{screen}");

    // y does nothing while the key map is up: nothing is discarded.
    terminal.send("y")?;

    terminal.send(ESC)?;
    let screen = terminal.wait_for("the question back", |screen| {
        screen.contents().contains("Discard this task?")
    })?;
    assert_eq!(screen, asked);

    terminal.send("n")?;
    terminal.wait_for("the form back with its content", |screen| {
        !screen.contents().contains("Discard this task?")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("New task")
    })?;
    quit(terminal)
}

#[test]
fn n_and_esc_answer_no_to_discard_and_the_form_keeps_its_content() -> Result<()> {
    let fixture = Fixture::new()?;
    for answer in ["n", ESC] {
        let mut terminal = open_form(&fixture)?;
        terminal.send("Half typed")?;
        terminal.send(CTRL_C)?;
        terminal.wait_for("the discard question", |screen| {
            screen.contents().contains("Discard this task?")
        })?;

        terminal.send(answer)?;

        let screen = terminal.wait_for("the form back with its content", |screen| {
            !screen.contents().contains("Discard this task?")
        })?;
        assert!(screen.contains("Half typed"), "{screen}");
        assert!(screen.contains("Ctrl-S add"), "{screen}");
        terminal.send(ESC)?;
        terminal.wait_for("the queue back", |screen| {
            !screen.contents().contains("New task")
        })?;
        quit(terminal)?;
    }
    Ok(())
}

/// Sends `signal` to the child directly, as the operating system would, and expects the same
/// clean exit and terminal restoration as quitting with `q`.
fn signal_quits(terminal: &Terminal, signal: Signal) -> Result<()> {
    terminal.send_signal(signal)?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    assert!(terminal.is_cooked()?, "line editing and echo are back on");
    Ok(())
}

#[test]
fn sigterm_makes_the_tui_exit_with_the_terminal_restored() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = fixture.open(ROWS)?;
    signal_quits(&terminal, Signal::SIGTERM)
}

#[test]
fn sighup_makes_the_tui_exit_with_the_terminal_restored() -> Result<()> {
    let fixture = Fixture::new()?;
    let terminal = fixture.open(ROWS)?;
    signal_quits(&terminal, Signal::SIGHUP)
}

#[test]
fn when_its_terminal_closes_the_tui_exits_at_once_and_burns_no_cpu() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let cwd = sandbox.home();
    let mut hung_up = HungUp::launch(env!("CARGO_BIN_EXE_ktask-rs"), &sandbox, &cwd, &["tui"])?;

    let ticks_before = hung_up.cpu_ticks().unwrap_or(0);
    let status = hung_up.wait_for_exit()?;
    // Once the child has exited and this process has reaped it, `/proc/<pid>` is gone: that
    // is itself proof nothing is left spinning, so a ticks reading missing at this point
    // counts as no further CPU used, not as a failure to measure.
    let used = hung_up
        .cpu_ticks()
        .unwrap_or(ticks_before)
        .saturating_sub(ticks_before);

    assert!(!status.success(), "{status:?}");
    assert!(
        used <= 2,
        "the process used {used} ticks of CPU after its terminal closed"
    );
    Ok(())
}

/// Builds `examples/panic_test.rs` and returns its path. `cargo test` builds examples only
/// when no single test target is selected, so the test builds it itself and reads the path
/// from cargo rather than guessing the target directory.
fn panic_test_example() -> Result<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "--example",
            "panic_test",
            "--message-format=json",
            "-p",
            "ktask-cli",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| message.get("executable")?.as_str().map(str::to_owned))
        .ok_or_else(|| "cargo built no panic_test example".into())
}

#[test]
fn a_panic_restores_the_terminal_before_the_message_is_printed() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let cwd = sandbox.home();
    let mut terminal =
        Terminal::launch_binary(&panic_test_example()?, &sandbox, &cwd, &[], ROWS, 80)?;
    terminal.wait_for("the first frame", |screen| {
        screen.contents().contains("panic-test") && screen.contents().ends_with('┘')
    })?;

    // Toggling cancelled tasks forces a second load of the queue, which is where the test
    // binary deliberately panics.
    terminal.send("a")?;

    let code = terminal.wait_for_exit()?;
    assert_ne!(code, 0, "a panic should not exit like a clean quit");
    assert!(
        terminal.is_cooked()?,
        "the terminal was not restored before the panic propagated"
    );
    let screen = terminal.screen();
    assert!(screen.contains("panicked at"), "{screen}");
    assert!(!screen.contains("panic-test"), "{screen}");
    Ok(())
}
