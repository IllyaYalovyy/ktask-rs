//! `t` on the queue screen: retries the selected task through the same use case `ktask-rs
//! retry` runs — no confirmation, since retrying is not destructive — and refuses, in the
//! same words, when the task's status is not `failed`, `failed-unknown` or `blocked`.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block whose implementation step fails the first time it runs — leaving a marker
/// file behind in the working tree — and succeeds the next time it finds that marker already
/// there: a retried attempt building on what the failed one left behind.
fn body_that_fails_once_then_succeeds() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ -f retried.marker ]; then\n  ktask-rs report --token \"$1\" done\nelse\n  touch retried.marker\n  ktask-rs report --token \"$1\" failed --reason \"first try\"\nfi\n```\n".to_owned()
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left its `run`, nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    fn new() -> Result<Self> {
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
    fn cli(&self, args: &[&str]) -> Result<()> {
        let outcome = self.sandbox.run(&self.repository, args)?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        self.cli(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            body,
        ])
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempts to
    /// commit are not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        for args in [
            ["config", "user.email", "test@example.com"],
            ["config", "user.name", "Test User"],
        ] {
            let mut command = std::process::Command::new("git");
            command.args(args);
            let output = self
                .sandbox
                .isolate(&mut command, &self.repository)
                .output()?;
            assert!(output.status.success(), "git {args:?}");
        }
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

/// The header's question line — where a refusal is shown at once, without asking.
fn header_line(screen: &str) -> String {
    lines_inside_frame(screen)
        .get(3)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn t_retries_a_failed_task_at_once_with_no_question_and_a_second_run_shows_the_earlier_attempt_as_history()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_succeeds())?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown failed", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  failed")
    })?;

    terminal.send("t")?;

    // No question is asked — retrying is not destructive — and the task is pending again at
    // once, its earlier, failed attempt's own step line still shown.
    let screen = terminal.wait_for("the task pending again", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  pending")
    })?;
    assert!(!screen.contains("y to"), "{screen}");
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.contains("implementation") && line.contains("failed")),
        "{screen}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the task done on its second attempt", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;
    let lines = lines_inside_frame(&screen);
    let implementation_lines: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("implementation"))
        .collect();
    assert_eq!(implementation_lines.len(), 2, "{lines:?}");
    assert!(
        implementation_lines[0].contains("attempt 1:")
            && implementation_lines[0].contains("failed"),
        "the earlier attempt shows above the current one, named with its own number: {lines:?}"
    );
    assert!(
        implementation_lines[1].contains("attempt 2:"),
        "the current attempt's own line carries its own number too: {lines:?}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn t_on_a_pending_task_refuses_without_asking_in_the_same_words_as_ktask_rs_retry() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_succeeds())?;
    let mut terminal = fixture.open()?;
    let before = terminal.screen();

    terminal.send("t")?;

    let screen = terminal.wait_for("the refusal", |screen| {
        screen.contents().contains("task 1 is pending")
    })?;
    assert!(
        header_line(&screen).contains("task 1 is pending"),
        "{}",
        header_line(&screen)
    );
    assert_eq!(
        selected_row(&screen),
        selected_row(&before),
        "the refusal changed nothing about the task list"
    );

    // The same words the CLI gives for retrying the same task.
    let cli_refusal = fixture.sandbox.run(&fixture.repository, &["retry", "1"])?;
    assert_eq!(cli_refusal.code, Some(2));
    assert!(
        cli_refusal.stderr.contains(&header_line(&screen)),
        "{}",
        cli_refusal.stderr
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn t_on_a_done_task_refuses_the_same_way() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown done", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;

    terminal.send("t")?;

    let screen = terminal.wait_for("the refusal", |screen| {
        screen.contents().contains("task 1 is done")
    })?;
    assert!(
        header_line(&screen).contains("task 1 is done"),
        "{}",
        header_line(&screen)
    );
    assert!(!screen.contains("y to"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn with_nothing_selected_t_retries_and_refuses_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the empty queue message", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;

    terminal.send("t")?;
    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(!screen.contains("is pending"), "{screen}");
    terminal.send(super::navigate::ESC)?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_t() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_succeeds())?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('t') && line.contains("retry")),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
