//! `ktask-rs run` on the real binary: the pending tasks of a project, in queue order, one
//! attempt each with the `echo` provider.
//!
//! The `echo` provider runs the first fenced `bash` block of a task's prompt — which is the
//! task's own body, carried through unchanged — so a task's body doubles as the script a
//! real agent would have run: it calls back into `ktask-rs report`, the same binary under
//! test, one level down. For that nested call to find `ktask-rs` on `PATH`, every run here
//! puts the binary's own directory on the child's `PATH`.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use rusqlite::OptionalExtension;
use support::{Outcome, Result, Sandbox};

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// A bash block that reports `outcome` for whatever token it is given as `$1`.
fn reporting_body(outcome: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome}\n```\n")
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\n```\n")
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

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` with `args` inside the repository, its `PATH` carrying the
    /// directory of the `ktask-rs` under test, so a task's own bash block — standing in for
    /// what a real agent would run — can call it back in with `ktask-rs report`.
    fn run_the_queue(&self, args: &[&str]) -> Result<Outcome> {
        self.run_the_queue_in(&self.repository, args)
    }

    /// Like [`Fixture::run_the_queue`], in `dir` instead of the repository.
    fn run_the_queue_in(&self, dir: &Path, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run_with(dir, args, |command| {
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
        })
    }

    /// Adds a task with `title` and body `body`, one criterion, kind `agent`.
    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        let added = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            body,
        ])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(())
    }

    /// Adds a task with `title`, kind `human`.
    fn add_human_task(&self, title: &str) -> Result<()> {
        let added = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--kind",
            "human",
        ])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
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

    /// The `attempt_running` event for attempt `number` of `task`: its provider.
    fn attempt_running_provider(&self, task: u64, number: i64) -> Result<String> {
        let database = rusqlite::Connection::open(self.journal())?;
        let payload: String = database.query_row(
            "SELECT payload FROM events WHERE kind = 'attempt_running' AND task_id = ?1",
            [i64::try_from(task)?],
            |row| row.get(0),
        )?;
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        assert_eq!(payload.get("number"), Some(&serde_json::json!(number)));
        Ok(payload
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    /// The `attempt_ended` event for `task`: duration in ms, exit code, status and reason.
    fn attempt_ended(&self, task: u64) -> Result<(i64, Option<i64>, String, Option<String>)> {
        let database = rusqlite::Connection::open(self.journal())?;
        let payload: Option<String> = database
            .query_row(
                "SELECT payload FROM events WHERE kind = 'attempt_ended' AND task_id = ?1",
                [i64::try_from(task)?],
                |row| row.get(0),
            )
            .optional()?;
        let payload: serde_json::Value = serde_json::from_str(&payload.unwrap_or_default())?;
        Ok((
            payload
                .get("duration_ms")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
            payload.get("exit_code").and_then(serde_json::Value::as_i64),
            payload
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        ))
    }
}

#[test]
fn three_tasks_that_all_report_done_all_end_done_and_exit_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    for title in ["a", "b", "c"] {
        fixture.add_agent_task(title, &reporting_body("done"))?;
    }

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "done");
    assert_eq!(fixture.task_status(3)?, "done");
    for task in [1, 2, 3] {
        assert_eq!(fixture.attempt_running_provider(task, 1)?, "echo");
        let (duration_ms, exit_code, status, reason) = fixture.attempt_ended(task)?;
        assert!(duration_ms >= 0, "{duration_ms}");
        assert_eq!(exit_code, Some(0));
        assert_eq!(status, "done");
        assert_eq!(reason, None);
    }
    Ok(())
}

#[test]
fn a_failed_report_ends_that_task_failed_with_the_reason_stops_the_run_leaves_the_rest_pending_and_exits_one()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "failed");
    assert_eq!(fixture.task_status(3)?, "pending");
    let (_, _, status, reason) = fixture.attempt_ended(2)?;
    assert_eq!(status, "failed");
    assert_eq!(reason.as_deref(), Some("it broke"));
    assert!(outcome.stdout.contains("failed"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("it broke"), "{}", outcome.stdout);
    Ok(())
}

#[test]
fn a_too_large_report_ends_that_task_failed_too() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("too-large", "split me"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    let (_, _, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(status, "failed");
    assert_eq!(reason.as_deref(), Some("split me"));
    Ok(())
}

#[test]
fn a_needs_input_report_ends_that_task_blocked_with_the_reason_and_stops_the_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task(
        "b",
        &reporting_body_with_reason("needs-input", "which path?"),
    )?;
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "blocked");
    assert_eq!(fixture.task_status(3)?, "pending");
    let (_, _, status, reason) = fixture.attempt_ended(2)?;
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("which path?"));
    Ok(())
}

#[test]
fn no_report_at_all_ends_that_task_failed_unknown_with_what_was_observed_and_stops_the_run()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", "```bash\necho did nothing\n```\n")?;
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "failed-unknown");
    assert_eq!(fixture.task_status(3)?, "pending");
    let (_, exit_code, status, reason) = fixture.attempt_ended(2)?;
    assert_eq!(exit_code, Some(0));
    assert_eq!(status, "failed-unknown");
    assert!(
        reason.as_deref().unwrap().contains("reported nothing"),
        "{reason:?}"
    );
    Ok(())
}

#[test]
fn a_provider_killed_past_its_time_limit_ends_that_task_failed_unknown_and_stops_the_run()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", "```bash\nsleep 30\n```\n")?;

    let started = Instant::now();
    let outcome = fixture.run_the_queue(&["run", "--attempt-timeout", "1"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, None);
    assert_eq!(status, "failed-unknown");
    assert!(
        reason.as_deref().unwrap().contains("time limit"),
        "{reason:?}"
    );
    Ok(())
}

#[test]
fn a_human_task_stops_the_run_before_attempting_it_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_human_task("b")?;
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.task_status(2)?, "pending");
    assert_eq!(fixture.task_status(3)?, "pending");
    assert!(outcome.stdout.contains("human"), "{}", outcome.stdout);
    Ok(())
}

#[test]
fn an_empty_queue_exits_zero_saying_so_and_runs_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert!(outcome.stdout.contains("empty"), "{}", outcome.stdout);
    Ok(())
}

#[test]
fn a_queue_with_nothing_pending_exits_zero_saying_so() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let first = fixture.run_the_queue(&["run"])?;
    assert_eq!(first.code, Some(0), "{}", first.stderr);

    let second = fixture.run_the_queue(&["run"])?;

    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert!(
        second.stdout.contains("nothing is pending"),
        "{}",
        second.stdout
    );
    Ok(())
}

#[test]
fn list_and_list_json_show_the_new_statuses() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body_with_reason("needs-input", "why"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert!(listed.stdout.contains("\tdone\t"), "{}", listed.stdout);
    assert!(listed.stdout.contains("\tblocked\t"), "{}", listed.stdout);

    let json = fixture.run(&["list", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let tasks: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let statuses: Vec<&str> = tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["done", "blocked"]);
    Ok(())
}

#[test]
fn run_and_its_options_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("run"), "{}", top.stdout);
    let help = fixture.run(&["run", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    assert!(help.stdout.contains("--attempt-timeout"), "{}", help.stdout);
    Ok(())
}

#[test]
fn run_works_from_a_subdirectory_and_with_project_from_any_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;

    let outcome = fixture.run_the_queue_in(&elsewhere, &["run", "--project", "my-app"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}
