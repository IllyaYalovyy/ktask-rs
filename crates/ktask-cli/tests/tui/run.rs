//! `r` on the queue screen: it starts executing the pending tasks exactly as `ktask-rs run`
//! does, the screen shows the run's progress the same way it shows one started elsewhere, the
//! refusals `ktask-rs run` itself prints are shown in the same words, and quitting the screen
//! does not stop a run it started.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 110;

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review step, when reached, approves.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for the
/// implementation step; the review step, when reached, approves.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A bash block that waits for the file at `go` to exist, then reports `done` (or, for the
/// review step, `approved`): an attempt that stays running until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            work,
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

    /// Runs `ktask-rs run` in the foreground, outside the TUI, and waits for it.
    fn run_the_queue(&self) -> Result<()> {
        let outcome = self.sandbox.run(&self.repository, &["run"])?;
        assert!(matches!(outcome.code, Some(0 | 1)), "{outcome:?}");
        Ok(())
    }

    /// Spawns `ktask-rs run` outside the TUI and returns at once, so the caller can hold its
    /// lock while watching the TUI through a run it did not start.
    fn spawn_run_outside_the_tui(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command.spawn()?)
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

/// The header's message row — where a refusal or a removal question is shown.
fn message_line(screen: &str) -> String {
    lines_inside_frame(screen)
        .get(3)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn r_starts_the_run_and_the_screen_shows_its_progress_as_it_would_for_a_run_started_elsewhere()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for("the task running with its attempt line", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
            && lines
                .get(5)
                .is_some_and(|line| line.contains("implementation"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  running  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("running"),
        "{}",
        lines[5]
    );

    std::fs::write(&go, "")?;
    let screen = terminal.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">  1  #1  done"))
    })?;
    assert!(
        screen.contains("pending 0") && screen.contains("running 0") && screen.contains("done 1"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_on_a_queue_with_nothing_pending_refuses_in_the_same_words_ktask_rs_run_would() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("nothing is pending")?;
    assert_eq!(message_line(&screen), "nothing is pending");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_past_an_earlier_task_that_did_not_finish_refuses_in_the_same_words_ktask_rs_run_would()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("run did not start")?;
    assert_eq!(
        message_line(&screen),
        "task 1: failed: it broke; run did not start"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_while_a_run_started_elsewhere_holds_the_queue_refuses_naming_its_process() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    let mut terminal = fixture.open()?;
    let mut outside = fixture.spawn_run_outside_the_tui()?;
    let outside_pid = outside.id();
    terminal.wait_for("the task running from the outside run", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.contains("running"))
    })?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("already in progress")?;
    assert_eq!(
        message_line(&screen),
        format!("ktask-rs: a run is already in progress: process {outside_pid}")
    );

    std::fs::write(&go, "")?;
    let status = outside.wait()?;
    assert!(status.success(), "{status:?}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn quitting_the_screen_does_not_stop_the_run_it_started_and_opening_it_again_shows_it_in_progress()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    let mut first = fixture.open()?;
    first.send("r")?;
    first.wait_for("the task running", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.contains("running"))
    })?;

    first.send("q")?;
    assert_eq!(first.wait_for_exit()?, 0);
    drop(first);

    let mut second = fixture.open()?;
    let screen = second.wait_for("the run still in progress, not interrupted", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
            && lines
                .get(5)
                .is_some_and(|line| line.contains("implementation"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  running  agent  a");
    assert!(!screen.contains("interrupted"), "{screen}");

    std::fs::write(&go, "")?;
    second.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">  1  #1  done"))
    })?;

    second.send("q")?;
    assert_eq!(second.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_r() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(screen.contains("ktask-rs run"), "{screen}");
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('r')),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
