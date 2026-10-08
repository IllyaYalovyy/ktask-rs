//! The queue screen's status band: always present under the summary line, wrapping instead of
//! being cut when it does not fit one row, and read back from the journal alone — so a TUI
//! that never started the run itself still shows exactly what happened.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, so a task's own
/// bash block can call back into `ktask-rs report` and find this same binary.
fn with_nested_ktask_rs_on_path(command: &mut Command) {
    let mut paths = Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for
/// the implementation step.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\n```\n")
}

/// A bash block that reports `outcome`, with no reason, for whatever token it is given, for
/// the implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left a `run`, nothing of it survives the test itself.
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

    fn cli(&self, args: &[&str]) -> Result<String> {
        let outcome = self.sandbox.run(&self.repository, args)?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
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
        ])?;
        Ok(())
    }

    fn add_human_task(&self, title: &str) -> Result<()> {
        self.cli(&[
            "add",
            "--title",
            title,
            "--criterion",
            "approved",
            "--kind",
            "human",
        ])?;
        Ok(())
    }

    /// Runs `ktask-rs run` in the foreground, its `PATH` carrying the directory of the
    /// `ktask-rs` under test, so a task's own bash block can call it back with
    /// `ktask-rs report`. Never through the TUI: the run this starts has no screen watching
    /// it, exactly as a run started from a terminal elsewhere, or by a scheduled job, would.
    fn run_the_queue(&self) -> Result<String> {
        let outcome =
            self.sandbox
                .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)?;
        Ok(outcome.stdout)
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

/// The frame's lines with the border stripped, for direct indexing: `0` the top border,
/// `1` the project name, `2` the summary counts, `3` (and `4`, when the band wrapped) the
/// run band, then the question line and the task list.
fn frame_lines(screen: &str) -> Vec<String> {
    lines_inside_frame(screen)
}

#[test]
fn the_band_wraps_to_a_second_line_at_80_columns_without_losing_the_next_part() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let run = fixture.run_the_queue()?;
    assert!(run.contains("failed"), "{run}");

    let terminal = fixture.open()?;
    let screen = terminal.wait_for_text("run stopped")?;
    let lines = frame_lines(&screen);

    // The band's own text — "run stopped <time>: #1 failed (routed: ...) — it broke · next:
    // fix the cause, then t to retry #1" — does not fit one row at 80 columns, so it must
    // take a second one rather than being cut with no sign of it.
    assert!(lines[3].starts_with("run stopped"), "{lines:#?}");
    let joined = format!("{} {}", lines[3].trim_end(), lines[4].trim());
    assert!(
        joined.contains("next: fix the cause, then t to retry #1"),
        "the band's next action was cut instead of wrapping: {lines:#?}"
    );
    assert!(!lines[3].contains('…'), "{lines:#?}");
    assert!(!lines[4].contains('…'), "{lines:#?}");
    Ok(())
}

#[test]
fn a_tui_opened_after_the_run_stopped_elsewhere_shows_the_same_band_from_the_journal() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;

    // The run that stops here is never watched by any screen: this proves the band a TUI
    // shows afterwards comes from the journal, not from a run message only the screen that
    // started a run would ever hold.
    let run = fixture.run_the_queue()?;
    assert!(run.contains("failed"), "{run}");

    let terminal = fixture.open()?;
    let screen = terminal.wait_for_text("run stopped")?;
    let lines = frame_lines(&screen);
    let joined = format!("{} {}", lines[3].trim_end(), lines[4].trim());
    assert!(joined.starts_with("run stopped"), "{lines:#?}");
    assert!(joined.contains("#1 failed"), "{lines:#?}");
    assert!(joined.contains("it broke"), "{lines:#?}");
    assert!(
        joined.contains("next: fix the cause, then t to retry #1"),
        "{lines:#?}"
    );
    Ok(())
}

#[test]
fn a_blocked_task_shows_a_stopped_band_naming_the_question_and_the_answer_key() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("x", &reporting_body("done"))?;
    fixture.add_agent_task(
        "d",
        &reporting_body_with_reason("needs-input", "which path?"),
    )?;
    let run = fixture.run_the_queue()?;
    assert!(run.contains("blocked"), "{run}");

    let terminal = fixture.open()?;
    let screen = terminal.wait_for_text("run stopped")?;
    let lines = frame_lines(&screen);
    let joined = format!("{} {}", lines[3].trim_end(), lines[4].trim());
    assert!(joined.starts_with("run stopped"), "{lines:#?}");
    assert!(joined.contains("#2 blocked"), "{lines:#?}");
    assert!(joined.contains("which path?"), "{lines:#?}");
    assert!(
        joined.contains("next: answer the question, then A to answer #2"),
        "{lines:#?}"
    );
    Ok(())
}

#[test]
fn a_human_task_at_the_head_of_the_queue_shows_a_stopped_band_naming_the_acknowledge_key()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_human_task("approve the plan")?;

    let terminal = fixture.open()?;
    let screen = terminal.wait_for_text("run stopped")?;
    let lines = frame_lines(&screen);
    let joined = format!("{} {}", lines[3].trim_end(), lines[4].trim());
    assert!(joined.starts_with("run stopped"), "{lines:#?}");
    assert!(joined.contains("#1 is a human task"), "{lines:#?}");
    assert!(
        joined.contains("next: H to acknowledge #1, then r to run"),
        "{lines:#?}"
    );
    Ok(())
}
