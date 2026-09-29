//! `p` on the queue screen: it opens the list of registered projects — the same one
//! `ktask-rs project list` prints — the current one marked, and selecting one replaces the
//! queue screen with that project's queue, every action from then on applying to it.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 90;
const DOWN: &str = "\x1b[B";
const ENTER: &str = "\r";
const ESC: &str = "\x1b";
const SUBMIT: &str = "\x13";

/// A sandbox with two registered git repositories: `my-app`, with one task, registered first,
/// and `other-app`, with one task of its own, registered second.
struct Fixture {
    sandbox: Sandbox,
    my_app: PathBuf,
    other_app: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let my_app = git_repository(&sandbox, &work, "my-app")?;
        let other_app = git_repository(&sandbox, &work, "other-app")?;
        let fixture = Self {
            sandbox,
            my_app,
            other_app,
            _keep: keep,
        };
        // Registers `my-app` first and `other-app` second: `ktask-rs project list` and the
        // picker both list oldest registration first.
        fixture.add(&fixture.my_app, "alpha")?;
        fixture.add(&fixture.other_app, "bravo")?;
        Ok(fixture)
    }

    fn add(&self, repository: &std::path::Path, title: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            repository,
            &["add", "--title", title, "--criterion", "it works"],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// What `ktask-rs project list` prints, as text.
    fn cli_project_list(&self) -> Result<String> {
        let outcome = self.sandbox.run(&self.my_app, &["project", "list"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }

    /// What `ktask-rs list` prints for `repository`'s project, as text.
    fn cli_list(&self, repository: &std::path::Path) -> Result<String> {
        let outcome = self.sandbox.run(repository, &["list"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }

    /// Opens the queue screen on `my-app` and waits until it is drawn whole, with its task
    /// selected.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.my_app, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue with its task selected", |screen| {
            let contents = screen.contents();
            contents.ends_with('┘') && contents.contains(">1  #1  pending  agent  alpha")
        })?;
        Ok(terminal)
    }

    /// Opens the queue screen and the project picker over it.
    fn open_picker(&self) -> Result<Terminal> {
        let mut terminal = self.open()?;
        terminal.send("p")?;
        terminal.wait_for("the project picker", |screen| {
            screen.contents().contains("Projects")
        })?;
        Ok(terminal)
    }
}

#[test]
fn p_opens_the_list_project_list_prints_with_name_and_path_the_current_one_marked() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Projects");

    let listed = fixture.cli_project_list()?;
    let mut cli_rows = listed.lines();
    let (my_app_name, my_app_path) = cli_rows.next().and_then(|l| l.split_once('\t')).unwrap();
    let (other_app_name, other_app_path) =
        cli_rows.next().and_then(|l| l.split_once('\t')).unwrap();
    assert_eq!(my_app_name, "my-app");
    assert_eq!(other_app_name, "other-app");
    assert!(cli_rows.next().is_none(), "{listed}");

    // The picker lists them in the same order, `my-app` marked as the one on show.
    assert!(
        lines[3].starts_with('>')
            && lines[3].contains(my_app_name)
            && lines[3].contains(my_app_path)
            && lines[3].ends_with("(current)"),
        "{lines:?}"
    );
    assert!(
        lines[4].starts_with(' ')
            && lines[4].contains(other_app_name)
            && lines[4].contains(other_app_path)
            && !lines[4].contains("(current)"),
        "{lines:?}"
    );
    assert!(
        lines[ROWS as usize - 1].starts_with("└ j, k select · Enter switch · Esc cancel"),
        "{lines:?}"
    );

    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn esc_closes_the_picker_and_switches_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    terminal.send(ESC)?;

    terminal.wait_for("the queue back, without the picker", |screen| {
        let contents = screen.contents();
        !contents.contains("Projects") && contents.contains("my-app")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn selecting_a_project_replaces_the_queue_screen_and_every_action_then_applies_to_it() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    terminal.send(DOWN)?;
    terminal.send(ENTER)?;

    // The queue screen now shows `other-app`'s queue, not `my-app`'s.
    terminal.wait_for("other-app's queue, with its own task selected", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(1).is_some_and(|line| line == "other-app")
            && lines
                .get(4)
                .is_some_and(|line| line == ">1  #1  pending  agent  bravo")
    })?;

    // Adding a task from here applies to `other-app`, not `my-app`.
    terminal.send("n")?;
    terminal.wait_for("the form", |screen| screen.contents().contains("New task"))?;
    terminal.send("charlie")?;
    terminal.send("\t\t\t\tit works")?;
    terminal.send(SUBMIT)?;
    terminal.wait_for("charlie added to the queue on show", |screen| {
        screen.contents().contains("charlie")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    let other_app_list = fixture.cli_list(&fixture.other_app)?;
    assert!(other_app_list.contains("bravo"), "{other_app_list}");
    assert!(other_app_list.contains("charlie"), "{other_app_list}");
    let my_app_list = fixture.cli_list(&fixture.my_app)?;
    assert!(my_app_list.contains("alpha"), "{my_app_list}");
    assert!(!my_app_list.contains("charlie"), "{my_app_list}");
    Ok(())
}

#[test]
fn the_queue_key_map_lists_p() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.send("?")?;
    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        screen.contains("p        work on another registered project's queue"),
        "{screen}"
    );
    terminal.send(ESC)?;
    terminal.wait_for("the key map closed", |screen| {
        !screen.contents().contains("Keys")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
