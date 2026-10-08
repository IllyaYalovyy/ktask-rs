//! M4-08 on the real binary, through the TUI: once the resolver's `supersede` decision ends a
//! task `superseded` and places the new tasks where it was, the queue screen hides the
//! superseded task like a cancelled one, counts it in the summary, shows it — with its reason
//! naming the new tasks on the resolve step's own line — once the `a` toggle asks for it, and
//! shows the new tasks themselves at once, with no toggle needed.

use std::path::{Path, PathBuf};

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// A bash block whose implementation step fails the first time (attempt 1); the resolve step
/// supersedes with the tasks of `tasks_file`.
fn body_that_fails_once_then_supersedes(tasks_file: &Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" supersede --tasks \"{}\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n",
        tasks_file.display()
    )
}

/// A bash block for a task that simply succeeds: the implementation step reports `done` at
/// once, and the review and test steps, if ever reached, approve and accept.
fn succeeding_body() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        .to_owned()
}

/// A sandbox with a git repository called `my-app`, `max-attempts` set to 2 so the resolver
/// gets exactly one chance to supersede.
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
        sandbox.run(&repository, &["settings", "set", "max-attempts", "2"])?;
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
fn a_superseded_task_is_hidden_and_counted_while_the_new_ones_show_at_once_and_a_reveals_it()
-> Result<()> {
    let fixture = Fixture::new()?;
    let tasks_file = fixture.work.join("tasks.json");
    std::fs::write(
        &tasks_file,
        serde_json::json!([
            {"title": "part one", "criteria": ["it works"], "body": succeeding_body()},
            {"title": "part two", "criteria": ["it works"], "body": succeeding_body()}
        ])
        .to_string(),
    )?;
    fixture.add_agent_task(
        "too large",
        &body_that_fails_once_then_supersedes(&tasks_file),
    )?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    // Hidden by default: the summary counts it, but the list shows the two new tasks that
    // replaced it, in its place, with no toggle needed.
    let screen = terminal.wait_for(
        "the superseded task hidden but counted, the new ones shown",
        |screen| {
            let contents = screen.contents();
            contents.contains("superseded 1")
                && contents.contains("part one")
                && contents.contains("part two")
        },
    )?;
    assert!(!screen.contains("superseded  agent  too large"), "{screen}");
    let lines = lines_inside_frame(&screen);
    let positions: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("part one") || line.contains("part two"))
        .cloned()
        .collect();
    assert!(
        positions
            .iter()
            .any(|line| line.contains("1  #2  done  agent  part one")),
        "{positions:?}"
    );
    assert!(
        positions
            .iter()
            .any(|line| line.contains("2  #3  done  agent  part two")),
        "{positions:?}"
    );

    terminal.send("a")?;
    let screen = terminal.wait_for("the superseded task shown in its place", |screen| {
        screen.contents().contains("superseded  agent  too large")
    })?;
    let lines = lines_inside_frame(&screen);
    let task_row = lines
        .iter()
        .find(|line| line.contains("superseded  agent  too large"))
        .unwrap_or_else(|| panic!("no superseded row in {lines:?}"));
    assert!(
        task_row.ends_with("1  #1  superseded  agent  too large · usage none"),
        "{task_row:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("resolve · echo")
                && line.contains("superseded by 2 tasks: 2, 3")),
        "{lines:?}"
    );

    terminal.send("a")?;
    terminal.wait_for("the superseded task hidden again", |screen| {
        !screen.contents().contains("superseded  agent  too large")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
