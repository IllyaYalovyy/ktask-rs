//! `r` on the queue screen: it starts executing the pending tasks exactly as `ktask-rs run`
//! does, the screen shows the run's progress the same way it shows one started elsewhere, a
//! refusal to start — an earlier task left unfinished, another run already in progress, or
//! nothing left pending — is shown beside the task list rather than in place of it, printing
//! the same words `ktask-rs run` itself would, and quitting the screen does not stop a run it
//! started.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 110;

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A bash block that waits for the file at `go` to exist, then reports `done` (or, for the
/// review step, `approved`; for the test step, `accepted`): an attempt that stays running
/// until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
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

/// The list area's row at `index`, counting from its top — where the last run's results are
/// shown, one per line, when it covers the task list, or the task list's own rows otherwise.
fn result_line(screen: &str, index: usize) -> String {
    lines_inside_frame(screen)
        .get(4 + index)
        .cloned()
        .unwrap_or_default()
}

/// The header's question line — where a run's refusal to start is shown, beside the task list
/// rather than in place of it.
fn header_line(screen: &str) -> String {
    lines_inside_frame(screen)
        .get(3)
        .cloned()
        .unwrap_or_default()
}

/// The task list's row that carries the selection marker, wherever a task's own attempt steps
/// have pushed it to.
fn selected_row(screen: &str) -> String {
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.starts_with('>'))
        .unwrap_or_default()
}

/// The task list's row for the task titled `title` — wherever another task's own attempt
/// steps have pushed it to — found by its title, the last column of its row and unique among
/// the fixtures below.
fn row_titled(screen: &str, title: &str) -> String {
    let suffix = format!("  {title}");
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.ends_with(&suffix))
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
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("running"),
        "{}",
        lines[5]
    );

    std::fs::write(&go, "")?;
    let screen = terminal.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
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
fn r_on_a_queue_with_nothing_pending_shows_the_refusal_beside_the_task_list() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("nothing is pending")?;
    assert_eq!(header_line(&screen), "nothing is pending");
    // The task list is still there, under the refusal, with the selection on the first task,
    // where it was — not replaced by the refusal the way a run's own report would.
    let selected = selected_row(&screen);
    assert!(selected.ends_with("  a"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    // `j`, `k`, `g` and `G` move the selection while the refusal stays shown.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("G")?;
    let screen = terminal.wait_for("G kept the selection on the last task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("g")?;
    let screen = terminal.wait_for("g moved the selection to the first task", |screen| {
        selected_row(&screen.contents()).ends_with("  a")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("k")?;
    let screen = terminal.wait_for("k held the selection on the first task", |screen| {
        selected_row(&screen.contents()).ends_with("  a")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    // Any other key dismisses the refusal.
    terminal.send("a")?;
    let screen = terminal.wait_for(
        "the refusal dismissed by a key that is not j, k, g or G",
        |screen| header_line(&screen.contents()).is_empty(),
    )?;
    assert!(!screen.contains("nothing is pending"), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_past_an_earlier_task_that_did_not_finish_shows_the_refusal_beside_the_task_list() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("run did not start")?;
    assert_eq!(
        header_line(&screen),
        "task 1: failed: it broke; run did not start"
    );
    assert!(selected_row(&screen).ends_with("  a"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(
        header_line(&screen),
        "task 1: failed: it broke; run did not start"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_run_that_attempts_several_tasks_shows_one_result_per_line_scrollable_when_more_than_fit()
-> Result<()> {
    let fixture = Fixture::new()?;
    for letter in ["a", "b", "c", "d", "e", "f"] {
        fixture.add_agent_task(letter, &reporting_body("done"))?;
    }
    // Small enough that the list area (5 rows: 8 inner rows minus the 3-row header) holds
    // fewer than the six result lines a run through every task prints.
    let terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], 10, 80)?;
    terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    let mut terminal = terminal;

    terminal.send("r")?;

    // The header's counts can already say `done 6` — they follow the journal live, task by
    // task — while the run's own results, printed only once the whole `ktask-rs run`
    // subprocess exits, are still on their way; waiting for the first result line is what
    // actually proves they arrived.
    let screen = terminal.wait_for("the run's results to show", |screen| {
        result_line(&screen.contents(), 0) == "task 1: done"
    })?;
    // Each result is its own line, in the words `ktask-rs run` itself prints — not joined —
    // and only as many as fit in the list area are shown.
    assert_eq!(result_line(&screen, 0), "task 1: done");
    assert_eq!(result_line(&screen, 1), "task 2: done");
    assert_eq!(result_line(&screen, 2), "task 3: done");
    assert_eq!(result_line(&screen, 3), "task 4: done");
    assert_eq!(result_line(&screen, 4), "task 5: done");
    assert!(!screen.contains("task 6: done"), "{screen}");

    terminal.send("G")?;

    let screen = terminal.wait_for("the results scrolled to the last line", |screen| {
        result_line(&screen.contents(), 4) == "task 6: done"
    })?;
    assert_eq!(result_line(&screen, 0), "task 2: done");
    assert_eq!(result_line(&screen, 1), "task 3: done");
    assert_eq!(result_line(&screen, 2), "task 4: done");
    assert_eq!(result_line(&screen, 3), "task 5: done");
    assert_eq!(result_line(&screen, 4), "task 6: done");

    terminal.send("g")?;
    let screen = terminal.wait_for("the results scrolled back to the first line", |screen| {
        result_line(&screen.contents(), 0) == "task 1: done"
    })?;
    assert_eq!(result_line(&screen, 4), "task 5: done");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_while_a_run_started_elsewhere_holds_the_queue_shows_the_refusal_beside_the_task_list()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
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
    let expected = format!("ktask-rs: a run is already in progress: process {outside_pid}");
    assert_eq!(header_line(&screen), expected);
    // The task list is still there, both tasks on it, under the refusal, with the selection
    // on the first task, where it was.
    let selected = selected_row(&screen);
    assert!(selected.ends_with("  a"), "{screen}");
    assert!(selected.contains("running"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    // `j` moves the selection while the refusal stays shown.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), expected);

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
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(!screen.contains("interrupted"), "{screen}");

    std::fs::write(&go, "")?;
    second.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
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
