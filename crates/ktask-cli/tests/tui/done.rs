//! `D` on the queue screen: opens the done form for the selected task, through the same use
//! case `ktask-rs done` runs, and refuses, in the same words, when the task's status is not
//! `failed`, `failed-unknown` or `blocked`.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;
const SUBMIT: &str = "\x13";
const ESC: &str = "\x1b";

/// A bash block that reports `outcome` with `--reason` for whatever token it is given as `$1`,
/// for the implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
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
fn capital_d_opens_the_done_form_and_ctrl_s_marks_it_done_showing_the_reason_and_when() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown failed", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  failed")
    })?;

    terminal.send("D")?;

    let screen = terminal.wait_for("the done form", |screen| {
        screen.contents().contains("Mark task #1 done")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Mark task #1 done");
    assert!(
        screen
            .lines()
            .next_back()
            .is_some_and(|row| row.starts_with("└ Ctrl-S mark done · Esc cancel")),
        "{screen}"
    );

    terminal.send("fixed by hand")?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for("the task done, the reason and when shown", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;
    assert!(!screen.contains("Mark task #1 done"), "{screen}");
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.contains("marked done by the user") && line.contains("fixed by hand")),
        "{screen}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    // The next run continues past the task: there is nothing left for it to attempt.
    let run = fixture.sandbox.run(&fixture.repository, &["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stdout.contains("nothing is pending"), "{}", run.stdout);
    Ok(())
}

#[test]
fn esc_closes_the_done_form_and_marks_nothing_done() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the task shown failed", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  failed")
    })?;
    terminal.send("D")?;
    terminal.wait_for_text("Mark task #1 done")?;
    terminal.send("ignored")?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the queue screen back, still failed", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  failed")
    })?;
    assert!(!screen.contains("Mark task #1 done"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn capital_d_on_a_pending_task_refuses_without_asking_in_the_same_words_as_ktask_rs_done()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let mut terminal = fixture.open()?;
    let before = terminal.screen();

    terminal.send("D")?;

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

    // The same words the CLI gives for marking the same task done.
    let cli_refusal = fixture.sandbox.run(
        &fixture.repository,
        &["done", "1", "--reason", "an attempt"],
    )?;
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
fn with_nothing_selected_capital_d_opens_nothing_and_refuses_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the empty queue message", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;

    terminal.send("D")?;
    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(!screen.contains("is pending"), "{screen}");
    assert!(!screen.contains("Mark task"), "{screen}");
    terminal.send(super::navigate::ESC)?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_capital_d() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('D') && line.contains("done")),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
