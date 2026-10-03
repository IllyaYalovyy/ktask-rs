//! `ktask-rs done` on the real binary: marking any task done by hand, recording the reason and
//! when, and letting the next run continue past it.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use serde_json::Value;
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A bash block that reports `outcome` with `--reason` for whatever token it is given as `$1`,
/// for the implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

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

    /// Asserts that `done ID --reason TEXT` exits 2 naming what is wrong, and that nothing
    /// changed.
    fn assert_refused(&self, id: &str, reason: &[&str], naming: &[&str]) -> Result<()> {
        let before = (self.events()?, self.run(&["list", "--all"])?.stdout);
        let mut args = vec!["done", id];
        args.extend_from_slice(reason);
        let outcome = self.run(&args)?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        for name in naming {
            assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
        }
        let after = (self.events()?, self.run(&["list", "--all"])?.stdout);
        assert_eq!(after, before, "a refused done changed something");
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
fn done_exits_zero_marks_the_task_done_and_status_shows_the_reason_and_when() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");

    let done = fixture.run(&["done", "1", "--reason", "fixed by hand"])?;

    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.stdout, "task 1 is done\n");
    assert_eq!(done.stderr, "");
    assert_eq!(fixture.task_status(1)?, "done");

    // `status --json` shows the task done, with the reason and when, and the attempt's own
    // ending is still there too — the manual marking does not erase it.
    let entries = fixture.status_json()?;
    assert_eq!(entries[0]["status"], "done");
    assert_eq!(entries[0]["done_by_user"]["reason"], "fixed by hand");
    assert!(entries[0]["done_by_user"]["at"].is_string(), "{entries}");
    assert_eq!(entries[0]["attempt"]["outcome"], "failed");
    assert_eq!(entries[0]["attempt"]["reason"], "it broke");

    // `status`'s plain text form names it too.
    let text = fixture.run(&["status"])?;
    assert_eq!(text.code, Some(0), "{}", text.stderr);
    assert!(
        text.stdout
            .contains("marked done by the user: fixed by hand"),
        "{}",
        text.stdout
    );
    Ok(())
}

#[test]
fn a_second_run_continues_past_the_task_marked_done() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run(&["run"])?;
    fixture.run(&["done", "1", "--reason", "fixed by hand"])?;

    let second = fixture.run(&["run"])?;

    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert!(
        second.stdout.contains("nothing is pending"),
        "{}",
        second.stdout
    );
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn done_on_a_blocked_or_a_failed_unknown_task_works_too() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(fixture.task_status(1)?, "blocked");

    let done = fixture.run(&["done", "1", "--reason", "handled outside the tool"])?;

    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn done_on_a_pending_task_succeeds_before_it_is_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("done", "n/a"))?;
    let done = fixture.run(&["done", "1", "--reason", "finished before its run"])?;

    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.stdout, "task 1 is done\n");
    assert_eq!(fixture.task_status(1)?, "done");

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stdout.contains("nothing is pending"), "{}", run.stdout);
    Ok(())
}

#[test]
fn done_without_reason_exits_two_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run(&["run"])?;
    assert_eq!(fixture.task_status(1)?, "failed");

    let before = (fixture.events()?, fixture.run(&["list", "--all"])?.stdout);
    let outcome = fixture.run(&["done", "1"])?;
    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    let after = (fixture.events()?, fixture.run(&["list", "--all"])?.stdout);
    assert_eq!(after, before, "a missing --reason changed something");
    Ok(())
}

#[test]
fn done_with_an_empty_or_blank_reason_is_refused_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.run(&["run"])?;
    fixture.assert_refused("1", &["--reason", "   "], &["the reason is empty"])?;
    Ok(())
}

#[test]
fn marking_an_unknown_task_done_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.assert_refused("9", &["--reason", "x"], &["there is no task 9"])
}

#[test]
fn a_missing_or_malformed_id_exits_two_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    let missing = fixture.run(&["done", "--reason", "x"])?;
    assert_eq!(missing.code, Some(2), "{}", missing.stderr);
    for id in ["abc", "-1", "1.5", ""] {
        let outcome = fixture.run(&["done", id, "--reason", "x"])?;
        assert_eq!(outcome.code, Some(2), "{id}: {}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
    }
    assert_eq!(fixture.events()?, (1, "task_added".to_owned()));
    Ok(())
}

#[test]
fn done_and_its_arguments_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["--help"])?;
    assert!(help.stdout.contains("done"), "{}", help.stdout);
    let done = fixture.run(&["done", "--help"])?;
    assert_eq!(done.code, Some(0));
    for word in ["ID", "--reason", "--project"] {
        assert!(done.stdout.contains(word), "{word}: {}", done.stdout);
    }
    Ok(())
}
