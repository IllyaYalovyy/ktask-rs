//! M4-07 on the real binary, through the TUI: once the resolver's `skip` decision ends a task
//! `skipped`, the queue screen hides it like a cancelled one, counts it in the summary, and
//! shows it — with its reason on the resolve step's own line — once the `a` toggle asks for it.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// A bash block whose implementation step fails the first time (attempt 1); the resolve step
/// skips with `reason`.
fn body_that_fails_once_then_skips(reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" skip --reason \"{reason}\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n"
    )
}

/// A sandbox with a git repository called `my-app`, `max-attempts` set to 2 so the resolver
/// gets exactly one chance to skip.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "2"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            &self.repository,
            &[
                "add",
                "--title",
                title,
                "--criterion",
                "it works",
                "--body",
                body,
            ],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Runs `ktask-rs run` outside the TUI, accepting either a completed or a stopped run.
    fn run_the_queue(&self) -> Result<()> {
        let outcome = self.sandbox.run(&self.repository, &["run"])?;
        assert!(matches!(outcome.code, Some(0 | 1)), "{outcome:?}");
        Ok(())
    }

    /// Opens the queue screen and waits until it is drawn whole.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }
}

#[test]
fn a_skipped_task_is_hidden_counted_and_shown_with_its_reason_once_toggled_on() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_skips("no longer relevant"))?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    // Hidden by default: the summary still counts it, but the only task is skipped, so the
    // list reads empty, the same as it would for a cancelled task.
    let screen = terminal.wait_for("the skipped task hidden but counted", |screen| {
        let contents = screen.contents();
        contents.contains("skipped 1") && contents.contains("The queue is empty.")
    })?;
    assert!(!screen.contains("skipped  agent  a"), "{screen}");

    terminal.send("a")?;
    let screen = terminal.wait_for("the skipped task shown in its place", |screen| {
        screen.contents().contains("skipped  agent  a")
    })?;
    let lines = lines_inside_frame(&screen);
    let task_row = lines
        .iter()
        .find(|line| line.contains("skipped  agent  a"))
        .unwrap_or_else(|| panic!("no skipped row in {lines:?}"));
    assert!(
        task_row.ends_with("1  #1  skipped  agent  a"),
        "{task_row:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("resolve · echo") && line.contains("no longer relevant")),
        "{lines:?}"
    );

    terminal.send("a")?;
    terminal.wait_for("the skipped task hidden again", |screen| {
        !screen.contents().contains("skipped  agent  a")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
