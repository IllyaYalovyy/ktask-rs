//! Opening `ktask-rs tui` in a directory whose folder name is already registered for another
//! path: the queue cannot open on it, so the screen shows the refusal `ktask-rs` prints for the
//! same conflict and asks for a name to register the directory under instead — a free name
//! registers it and opens its queue; a taken name is refused and asks again — the same outcome
//! `ktask-rs project register --name` gives for the same name.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;
const SUBMIT: &str = "\x13";
const ESC: &str = "\x1b";

/// Two registered git repositories called `app`, in different parents: `one`, registered first
/// under its own folder name, and `two`, whose own folder name is therefore already taken.
struct Fixture {
    sandbox: Sandbox,
    one: PathBuf,
    two: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let one = git_repository(&sandbox, &work.join("one"), "app")?;
        let two = git_repository(&sandbox, &work.join("two"), "app")?;
        let shown = sandbox.run(&one, &["project", "show"])?;
        assert_eq!(shown.code, Some(0), "{}", shown.stderr);
        Ok(Self {
            sandbox,
            one,
            two,
            _keep: keep,
        })
    }

    /// Opens the terminal interface in `two` and waits until the registration screen is drawn
    /// whole, on an empty name.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.two, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the registration screen", |screen| {
            let contents = screen.contents();
            contents.contains("Register it under this name instead:") && contents.ends_with('┘')
        })?;
        Ok(terminal)
    }

    /// What `ktask-rs project list` prints.
    fn listed(&self) -> Result<String> {
        let outcome = self.sandbox.run(&self.one, &["project", "list"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }
}

#[test]
fn opening_on_a_taken_folder_name_shows_the_refusal_with_both_paths_and_asks_for_a_name()
-> Result<()> {
    let fixture = Fixture::new()?;

    let mut terminal = fixture.open()?;

    let screen = terminal.screen();
    super::title::assert_dev_title(&screen, "");
    assert!(
        screen.contains("This directory cannot be opened"),
        "{screen}"
    );
    assert!(
        screen.contains(&fixture.one.display().to_string()),
        "{screen}"
    );
    assert!(
        screen.contains(&fixture.two.display().to_string()),
        "{screen}"
    );
    assert!(screen.contains("already registered for"), "{screen}");
    assert!(
        screen.contains("Register it under this name instead:"),
        "{screen}"
    );
    let lines = lines_inside_frame(&screen);
    assert!(lines.iter().any(|line| line == ">"), "{lines:?}");
    assert!(
        screen.contains("Ctrl-S register") && screen.contains("Esc quit"),
        "{screen}"
    );
    assert!(!screen.contains("The queue is empty."), "{screen}");

    terminal.send(ESC)?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    // Nothing was registered: the directory is still unresolved and the registry unchanged.
    assert_eq!(
        fixture.listed()?,
        format!("app\t{}\n", fixture.one.display())
    );
    Ok(())
}

#[test]
fn a_free_name_registers_the_directory_and_opens_its_queue() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    terminal.send("app-two")?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for("app-two's fresh queue", |screen| {
        let contents = screen.contents();
        contents.contains("The queue is empty.") && contents.ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "app-two");
    assert!(!screen.contains("cannot be opened"), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    // The same outcome `ktask-rs project register --name app-two` gives for this name.
    assert_eq!(
        fixture.listed()?,
        format!(
            "app\t{}\napp-two\t{}\n",
            fixture.one.display(),
            fixture.two.display()
        )
    );
    let shown = fixture.sandbox.run(&fixture.two, &["project", "show"])?;
    assert_eq!(
        shown.stdout,
        format!(
            "app-two\t{}\tdev\t{}\n",
            fixture.two.display(),
            fixture.sandbox.state_dir().display()
        )
    );
    assert_eq!(shown.stderr, "");
    assert_eq!(shown.code, Some(0));
    Ok(())
}

#[test]
fn a_taken_name_is_refused_and_asks_again_until_a_free_one_registers() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    // "app" is already registered for `one`: submitting it registers nothing and asks again,
    // the same refusal `ktask-rs project register --name app` gives for `two`.
    terminal.send("app")?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for("the taken-name refusal", |screen| {
        let contents = screen.contents();
        contents.contains("project name \"app\" is already registered for")
            && contents.ends_with('┘')
    })?;
    assert!(
        screen.contains(&fixture.one.display().to_string()),
        "{screen}"
    );
    assert!(!screen.contains("The queue is empty."), "{screen}");
    // Nothing was registered by the refused attempt, and what was typed is kept so a typo can
    // be fixed rather than retyped from scratch.
    assert_eq!(
        fixture.listed()?,
        format!("app\t{}\n", fixture.one.display())
    );
    let lines = lines_inside_frame(&screen);
    assert!(lines.iter().any(|line| line == "> app"), "{lines:?}");

    // Clearing the typed name and writing a free one instead still works.
    terminal.send("\x7f\x7f\x7f")?;
    terminal.send("app-two")?;
    terminal.send(SUBMIT)?;
    terminal.wait_for("app-two's fresh queue", |screen| {
        let contents = screen.contents();
        contents.contains("The queue is empty.") && contents.ends_with('┘')
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    assert_eq!(
        fixture.listed()?,
        format!(
            "app\t{}\napp-two\t{}\n",
            fixture.one.display(),
            fixture.two.display()
        )
    );
    Ok(())
}
