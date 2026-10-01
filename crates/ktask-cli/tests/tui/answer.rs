//! `A` on the queue screen: answers the question a blocked task's attempt asked, through the
//! same use case `ktask-rs answer` runs, and refuses, in the same words, when the task's
//! status is not `blocked`.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;
const SUBMIT: &str = "\x13";
const ESC: &str = "\x1b";

/// A bash block that asks `question` the first time it runs — leaving a marker file behind so
/// it never asks twice — reporting `needs-input`, then, once it finds that marker, reports
/// `outcome`; the review and test steps, when reached, approve and accept.
fn body_that_asks_then(question: &str, outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ -f asked.marker ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelse\n  touch asked.marker\n  ktask-rs report --token \"$1\" needs-input --reason \"{question}\"\nfi\n```\n"
    )
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
fn capital_a_opens_the_answer_form_with_the_question_and_ctrl_s_answers_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown blocked", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  blocked")
    })?;

    terminal.send("A")?;

    let screen = terminal.wait_for("the answer form", |screen| {
        screen.contents().contains("Answer task #1")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Answer task #1");
    assert_eq!(lines[3], "which path?");
    assert!(
        screen
            .lines()
            .next_back()
            .is_some_and(|row| row.starts_with("└ Ctrl-S answer · Esc cancel")),
        "{screen}"
    );

    terminal.send("the left one")?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for(
        "the task pending again, the question and answer shown",
        |screen| selected_row(&screen.contents()).starts_with(">1  #1  pending"),
    )?;
    assert!(!screen.contains("Answer task #1"), "{screen}");
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.contains("which path?") && line.contains("the left one")),
        "{screen}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    // The next run picks the answered task back up, and succeeds on its second attempt.
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown done", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn esc_closes_the_answer_form_and_answers_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown blocked", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  blocked")
    })?;
    terminal.send("A")?;
    terminal.wait_for_text("Answer task #1")?;
    terminal.send("ignored")?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the queue screen back, still blocked", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  blocked")
    })?;
    assert!(!screen.contains("Answer task #1"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn capital_a_on_a_pending_task_refuses_without_asking_in_the_same_words_as_ktask_rs_answer()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    let mut terminal = fixture.open()?;
    let before = terminal.screen();

    terminal.send("A")?;

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

    // The same words the CLI gives for answering the same task.
    let cli_refusal = fixture
        .sandbox
        .run(&fixture.repository, &["answer", "1", "an answer"])?;
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
fn with_nothing_selected_capital_a_opens_nothing_and_refuses_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the empty queue message", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;

    terminal.send("A")?;
    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(!screen.contains("is pending"), "{screen}");
    assert!(!screen.contains("Answer task"), "{screen}");
    terminal.send(super::navigate::ESC)?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_capital_a() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('A') && line.contains("answer")),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
