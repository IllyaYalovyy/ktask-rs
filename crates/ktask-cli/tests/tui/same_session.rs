//! M4-05b on the real binary, through the TUI: the queue screen's step line shows the session
//! an attempt ran in, the same way `ktask-rs status` does — proving the resolver's own
//! `retry --same-session` is not a CLI-only effect.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// A bash block whose implementation step, on attempt 1, reports a session and fails; the
/// resolve step retries with `--same-session`; attempt 2 reports the same session again and
/// reports done; the review and test steps, when reached, approve and accept.
fn body_that_fails_once_then_retries_with_the_same_session() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry --same-session\nelif [ \"$2\" = \"1\" ]; then\n  echo \"KTASK_SESSION: carried-over\"\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nelse\n  echo \"KTASK_SESSION: carried-over\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n".to_owned()
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
fn the_queue_screen_shows_the_resumed_attempts_session_beside_its_provider() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &body_that_fails_once_then_retries_with_the_same_session(),
    )?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the task done on its retried attempt", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;

    let lines = lines_inside_frame(&screen);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("implementation · echo")
                && line.contains("session:carried-over")),
        "the current attempt's implementation line should show the session it ran in: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("attempt 1") && line.contains("session:carried-over")),
        "the earlier attempt's own implementation line should show the same session: {lines:?}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
