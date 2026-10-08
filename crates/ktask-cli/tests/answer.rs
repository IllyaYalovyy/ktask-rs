//! `ktask-rs answer` on the real binary: answering the question a blocked task's attempt
//! asked, sending it back to pending, and letting the next attempt's own prompt carry both.

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

/// A bash block that asks `question` the first time it runs — leaving a marker file behind so
/// it never asks twice — reporting `needs-input`, then, once it finds that marker, reports
/// `outcome`; the review and test steps, when reached, approve and accept.
fn body_that_asks_then(question: &str, outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ -f asked.marker ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelse\n  touch asked.marker\n  ktask-rs report --token \"$1\" needs-input --reason \"{question}\"\nfi\n```\n"
    )
}

/// Like [`body_that_asks_then`], except once it finds `asked.marker` it first copies the whole
/// prompt it was given — read from the file path it receives as `$7` — to `prompt_copy`, so the
/// test can read back what the running system actually told the agent, not merely what a
/// prompt-building function predicts it would.
fn body_that_asks_then_copies_its_prompt(
    question: &str,
    outcome: &str,
    prompt_copy: &std::path::Path,
) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ -f asked.marker ]; then\n  cp \"$7\" \"{prompt_copy}\"\n  ktask-rs report --token \"$1\" {outcome}\nelse\n  touch asked.marker\n  ktask-rs report --token \"$1\" needs-input --reason \"{question}\"\nfi\n```\n",
        prompt_copy = prompt_copy.display(),
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
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
            work,
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

    fn journal(&self) -> PathBuf {
        self.sandbox.state_dir().join("my-app").join("journal.db")
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

    /// Asserts that `answer ID TEXT` exits 2 naming what is wrong, and that nothing changed.
    fn assert_refused(&self, id: &str, text: &str, naming: &[&str]) -> Result<()> {
        let before = (self.events()?, self.run(&["list", "--all"])?.stdout);
        let outcome = self.run(&["answer", id, text])?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        for name in naming {
            assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
        }
        let after = (self.events()?, self.run(&["list", "--all"])?.stdout);
        assert_eq!(after, before, "a refused answer changed something");
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
fn answer_exits_zero_sends_the_task_back_to_pending_and_shows_the_question_and_the_answer()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(fixture.task_status(1)?, "blocked");

    let answered = fixture.run(&["answer", "1", "the left one"])?;

    assert_eq!(answered.code, Some(0), "{}", answered.stderr);
    assert!(answered.stdout.contains("task 1"), "{}", answered.stdout);
    assert_eq!(answered.stderr, "");
    assert_eq!(fixture.task_status(1)?, "pending");

    // The question and the answer are both shown with the task.
    let parsed = fixture.status_json()?;
    let entries = &parsed["tasks"];
    assert_eq!(entries[0]["status"], "pending");
    assert_eq!(entries[0]["attempt"]["outcome"], "needs-input");
    let reason = entries[0]["attempt"]["reason"].as_str().unwrap();
    assert!(reason.contains("which path?"), "{reason}");
    assert!(reason.contains("the left one"), "{reason}");
    Ok(())
}

#[test]
fn a_second_run_after_answering_continues_and_the_prompt_carries_the_question_and_the_answer()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    let prompt_copy = fixture.work.join("attempt-2-prompt.txt");
    fixture.add_agent_task(
        "a",
        &body_that_asks_then_copies_its_prompt("which path?", "done", &prompt_copy),
    )?;
    fixture.run(&["run"])?;
    assert_eq!(fixture.task_status(1)?, "blocked");

    fixture.run(&["answer", "1", "the left one"])?;

    let second = fixture.run(&["run"])?;
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(fixture.task_status(1)?, "done");

    // Attempt 2's own script read its real prompt from the file path it was given as `$7` and
    // copied it here — this is what the running system actually told the agent, not a
    // prediction from `ktask_core::implementation_prompt` called directly.
    let prompt = std::fs::read_to_string(&prompt_copy)?;
    assert!(prompt.contains("which path?"), "{prompt}");
    assert!(prompt.contains("the left one"), "{prompt}");
    Ok(())
}

#[test]
fn answer_on_a_task_that_is_not_blocked_is_refused_naming_its_status() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    fixture.assert_refused("1", "an answer", &["task 1 is pending"])?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    fixture.run(&["answer", "1", "the left one"])?;
    let done = fixture.run(&["run"])?;
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    fixture.assert_refused("1", "another answer", &["task 1 is done"])?;
    Ok(())
}

#[test]
fn an_empty_or_blank_answer_is_refused_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    fixture.run(&["run"])?;
    assert_eq!(fixture.task_status(1)?, "blocked");
    fixture.assert_refused("1", "   ", &["the answer is empty"])?;
    Ok(())
}

#[test]
fn answering_an_unknown_task_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    fixture.assert_refused("9", "an answer", &["there is no task 9"])
}

#[test]
fn a_missing_id_or_text_exits_two_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body_that_asks_then("which path?", "done"))?;
    let missing_both = fixture.run(&["answer"])?;
    assert_eq!(missing_both.code, Some(2), "{}", missing_both.stderr);
    let missing_text = fixture.run(&["answer", "1"])?;
    assert_eq!(missing_text.code, Some(2), "{}", missing_text.stderr);
    assert_eq!(fixture.events()?, (1, "task_added".to_owned()));
    Ok(())
}

#[test]
fn answer_and_its_arguments_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["--help"])?;
    assert!(help.stdout.contains("answer"), "{}", help.stdout);
    let answer = fixture.run(&["answer", "--help"])?;
    assert_eq!(answer.code, Some(0));
    for word in ["ID", "TEXT", "--project"] {
        assert!(answer.stdout.contains(word), "{word}: {}", answer.stdout);
    }
    Ok(())
}
