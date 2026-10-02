//! M4-05 on the real binary, through the TUI: the queue screen's step lines show the model a
//! retried attempt ran with, the same way `ktask-rs status` does — proving the resolver's own
//! `--model` is not a CLI-only effect.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// A bash block whose implementation step fails the first time (attempt 1); the resolve step
/// retries with `--model other`; the review and test steps, when reached, approve and accept.
fn body_that_fails_once_then_retries_with_a_model() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry --model other\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n".to_owned()
}

/// A sandbox with a git repository called `my-app`, `max-attempts` set to 2 so the resolver
/// gets exactly one chance to retry.
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

/// The task list's row that carries the selection marker.
fn selected_row(screen: &str) -> String {
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.starts_with('>'))
        .unwrap_or_default()
}

#[test]
fn the_queue_screen_shows_the_retried_attempts_model_beside_its_provider() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_retries_with_a_model())?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the task done on its retried attempt", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;

    let lines = lines_inside_frame(&screen);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("attempt 2: implementation · echo (other)")),
        "the current attempt's own line is named with its own number too, and shows the \
         model it ran with: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("attempt 1: resolve · echo") && line.contains("retry")),
        "the resolution line between the two attempts says which attempt it resolved: {lines:?}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
