//! `ktask-rs retry` on the real binary: sending a task back to pending, keeping its earlier
//! attempts, and letting `ktask-rs run` continue from it.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::PathBuf;

use ktask_adapters::{GitCli, SqliteJournal};
use ktask_core::{AttemptToken, RunContext, Task, TaskId, TaskKind, TaskStatus};
use repo::{git_repository, scratch};
use serde_json::Value;
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept, so a
/// task meant to succeed does so end to end.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// Like [`reporting_body`], reporting `outcome` with `--reason`.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A bash block whose implementation step fails the first time it runs — leaving a marker
/// file behind in the working tree, which nothing between attempts removes — and succeeds the
/// next time it finds that marker already there: a retried attempt building on what the
/// failed one left behind, the way a real agent picking the work back up would.
fn body_that_fails_once_then_succeeds() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ -f retried.marker ]; then\n  ktask-rs report --token \"$1\" done\nelse\n  touch retried.marker\n  ktask-rs report --token \"$1\" failed --reason \"first try\"\nfi\n```\n".to_owned()
}

/// A bash block whose implementation step commits a real change to the working tree, then
/// reports `outcome` with `--reason`: for a task whose later, retried attempt should see a
/// non-empty diff of what it changed so far.
fn body_that_commits_then_reports(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  echo hello > changed.txt\n  git add changed.txt\n  git -c user.name=t -c user.email=t@t commit -q -m wip\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
}

/// However a test above left its `run`, nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
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

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `git` with `args` inside the repository, asserting it succeeded.
    fn git(&self, args: &[&str]) -> Result<()> {
        let mut command = std::process::Command::new("git");
        command.args(args);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempts to
    /// commit are not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        self.git(&["config", "user.email", "test@example.com"])?;
        self.git(&["config", "user.name", "Test User"])?;
        Ok(())
    }

    /// Commits a first file under the identity already configured, so the project's first real
    /// attempt has a commit to call its own start — the way any real project already has
    /// history before `ktask-rs` is ever pointed at it.
    fn initial_commit(&self) -> Result<()> {
        self.configure_git_identity()?;
        std::fs::write(self.repository.join("README"), "first\n")?;
        self.git(&["add", "."])?;
        self.git(&["commit", "--quiet", "-m", "first"])?;
        Ok(())
    }

    /// Adds a task with `title` and body `body`, one criterion, kind `agent`.
    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        let outcome = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            body,
        ])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    fn journal(&self) -> PathBuf {
        self.sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("journal.db")
    }

    fn task_status(&self, task: u64) -> Result<String> {
        let database = rusqlite::Connection::open(self.journal())?;
        Ok(database.query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [i64::try_from(task)?],
            |row| row.get(0),
        )?)
    }

    /// The number of events in the journal and the kind of the last one.
    fn events(&self) -> Result<(i64, String)> {
        let database = rusqlite::Connection::open(self.journal())?;
        Ok(database.query_row(
            "SELECT COUNT(*), (SELECT kind FROM events ORDER BY seq DESC LIMIT 1) FROM events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Asserts that `retry ID` exits 2 naming what is wrong, and that nothing changed.
    fn assert_refused(&self, id: &str, naming: &[&str]) -> Result<()> {
        let before = (self.events()?, self.run(&["list", "--all"])?.stdout);
        let outcome = self.run(&["retry", id])?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        for name in naming {
            assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
        }
        let after = (self.events()?, self.run(&["list", "--all"])?.stdout);
        assert_eq!(after, before, "a refused retry changed something");
        Ok(())
    }

    /// `status --json`, parsed.
    fn status_json(&self) -> Result<Value> {
        let outcome = self.run(&["status", "--json"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(serde_json::from_str(&outcome.stdout)?)
    }
}

#[test]
fn retry_exits_zero_sends_the_task_back_to_pending_and_keeps_its_attempt_visible() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");

    let retried = fixture.run(&["retry", "1"])?;

    assert_eq!(retried.code, Some(0), "{}", retried.stderr);
    assert_eq!(retried.stdout, "task 1 is pending again\n");
    assert_eq!(retried.stderr, "");
    assert_eq!(fixture.task_status(1)?, "pending");

    // The earlier, failed attempt is still shown — nothing about it was forgotten.
    let entries = fixture.status_json()?;
    assert_eq!(entries[0]["status"], "pending");
    assert_eq!(entries[0]["attempt"]["number"], 1);
    assert_eq!(entries[0]["attempt"]["outcome"], "failed");
    assert_eq!(entries[0]["attempt"]["reason"], "it broke");
    Ok(())
}

#[test]
fn a_second_run_after_retry_adds_attempt_two_under_the_first_in_status_and_the_task_builds_on_it()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_succeeds())?;
    let first = fixture.run(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");

    fixture.run(&["retry", "1"])?;
    let second = fixture.run(&["run"])?;

    // The marker the first attempt left in the working tree is still there — the second
    // attempt's own script found it and reported `done` because of it.
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(fixture.task_status(1)?, "done");

    let entries = fixture.status_json()?;
    assert_eq!(entries[0]["status"], "done");
    assert_eq!(entries[0]["attempt"]["number"], 2);
    let history = entries[0]["history"].as_array().expect("a history array");
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["number"], 1);
    assert_eq!(history[0]["outcome"], "failed");
    assert_eq!(history[0]["reason"], "first try");

    // The same history shows in the plain-text form too, the earlier attempt's steps named
    // with its own number ahead of them, the current attempt's own steps under it.
    let text = fixture.run(&["status"])?;
    assert_eq!(text.code, Some(0), "{}", text.stderr);
    assert!(
        text.stdout.contains("attempt 1: implementation"),
        "{}",
        text.stdout
    );
    let implementation_lines: Vec<_> = text
        .stdout
        .lines()
        .filter(|line| line.contains("implementation"))
        .collect();
    assert_eq!(implementation_lines.len(), 2, "{implementation_lines:?}");
    assert!(
        !implementation_lines[1].contains("attempt"),
        "the current attempt's own line carries no attempt-number prefix: {implementation_lines:?}"
    );
    Ok(())
}

#[test]
fn run_continues_from_the_retried_task_not_from_the_start_of_the_queue() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_fails_once_then_succeeds())?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let first = fixture.run(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    assert_eq!(fixture.task_status(2)?, "pending");
    assert_eq!(fixture.task_status(3)?, "pending");

    fixture.run(&["retry", "1"])?;
    let second = fixture.run(&["run"])?;

    assert_eq!(second.code, Some(0), "{}", second.stderr);
    // Task 1 was attempted again — not skipped — and tasks 2 and 3 ran after it, in order.
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "done");
    assert_eq!(fixture.task_status(3)?, "done");
    assert!(second.stdout.contains("task 1"), "{}", second.stdout);
    Ok(())
}

#[test]
fn retry_on_a_pending_a_running_or_a_done_task_is_refused_naming_its_status() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.assert_refused("1", &["task 1 is pending"])?;

    let done = fixture.run(&["run"])?;
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    fixture.assert_refused("1", &["task 1 is done"])?;
    Ok(())
}

#[test]
fn retrying_an_unknown_task_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.assert_refused("9", &["there is no task 9"])
}

#[test]
fn a_missing_or_malformed_id_exits_two_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let missing = fixture.run(&["retry"])?;
    assert_eq!(missing.code, Some(2), "{}", missing.stderr);
    assert!(missing.stderr.contains("<ID>"), "{}", missing.stderr);
    for id in ["abc", "-1", "1.5", ""] {
        let outcome = fixture.run(&["retry", id])?;
        assert_eq!(outcome.code, Some(2), "{id}: {}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
    }
    assert_eq!(fixture.events()?, (1, "task_added".to_owned()));
    Ok(())
}

#[test]
fn retry_and_id_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["--help"])?;
    assert!(help.stdout.contains("retry"), "{}", help.stdout);
    let retry = fixture.run(&["retry", "--help"])?;
    assert_eq!(retry.code, Some(0));
    for word in ["ID", "--project"] {
        assert!(retry.stdout.contains(word), "{word}: {}", retry.stdout);
    }
    Ok(())
}

/// A minimal task, only as far as [`ktask_core::implementation_prompt`] cares: its title and
/// criteria.
fn task_named(id: u64, title: &str) -> Task {
    Task {
        id: TaskId(id),
        position: 1,
        title: title.to_owned(),
        body: String::new(),
        criteria: vec!["it works".to_owned()],
        kind: TaskKind::Agent,
        links: vec![],
        status: TaskStatus::Pending,
        created_at: std::time::SystemTime::now(),
    }
}

#[test]
fn a_retried_tasks_second_attempt_prompt_names_the_firsts_outcome_reason_and_the_diff() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.initial_commit()?;
    fixture.add_agent_task("a", &body_that_commits_then_reports("failed", "it broke"))?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    fixture.run(&["retry", "1"])?;

    // Builds, ahead of running attempt 2, the exact prompt `ktask-rs run` will build for it —
    // the same technique `the_report_command_the_prompt_gives_the_agent_is_the_full_path_...`
    // in `tests/run.rs` uses, since the `echo` provider only ever runs a task's own fenced
    // `bash` block and never sees the rest of its prompt itself.
    let journal = SqliteJournal::open(&fixture.journal())?;
    let token = AttemptToken::new("my-app", TaskId(1), 2);
    let context = RunContext {
        project_name: "my-app",
        project_dir: &fixture.repository,
        binary_path: std::path::Path::new(env!("CARGO_BIN_EXE_ktask-rs")),
        attempt_timeout: std::time::Duration::from_secs(60),
        health_check_command: None,
        tracked_branch: None,
        disabled_steps: &[],
        max_attempts: 3,
        resolver_model: "",
    };
    let prompt =
        ktask_core::implementation_prompt(&journal, &GitCli, context, &task_named(1, "a"), &token)?;

    assert!(prompt.contains("## Earlier attempts"), "{prompt}");
    assert!(prompt.contains("attempt 1: failed — it broke"), "{prompt}");
    assert!(
        prompt.contains("## What the task has changed so far"),
        "{prompt}"
    );
    assert!(prompt.contains("+hello"), "{prompt}");
    Ok(())
}
