//! `ktask-rs run` on the real binary: the pending tasks of a project, in queue order, one
//! attempt each with the `echo` provider.
//!
//! The `echo` provider runs the first fenced `bash` block of a task's prompt — which is the
//! task's own body, carried through unchanged — so a task's body doubles as the script a
//! real agent would have run: it calls back into `ktask-rs report`, the same binary under
//! test, one level down. The binary under test puts its own directory first on that child's
//! `PATH` itself, so the nested call reaches it whether or not `ktask-rs` is on the caller's
//! `PATH` at all, and even when a different `ktask-rs` sits earlier on it — neither test
//! fixture here does anything special with `PATH` to make that call succeed.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use ktask_core::{AttemptToken, Task, TaskId, TaskKind, TaskStatus};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use repo::{git_repository, scratch};
use rusqlite::OptionalExtension;
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A directory holding a fake `ktask-rs` that does nothing but exit `99` — standing in for
/// some other tool of that name found earlier on a caller's `PATH`. Placed ahead of the real
/// one on `PATH`, it proves whether the real one is still the one reached: if it ran instead,
/// the attempt it was asked to report would be left unreported.
fn decoy_ktask_rs_dir() -> Result<TempDir> {
    let dir = TempDir::new()?;
    let path = dir.path().join("ktask-rs");
    std::fs::write(&path, "#!/bin/sh\nexit 99\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

/// Puts `dir` on `command`'s `PATH`, ahead of whatever is already there.
fn with_dir_first_on_path(command: &mut std::process::Command, dir: &Path) {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// Waits, for up to a few seconds, until `condition` holds, checking every 20ms; fails naming
/// `what` when it never does.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
    Ok(())
}

/// Like [`wait_until`], for a `probe` that also produces the value being waited for.
fn wait_until_some<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = probe() {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

/// Whether process `pid` is still running. A killed process's entry under `/proc` can briefly
/// outlive the signal that ended it, as a zombie waiting for its new parent to reap it once it
/// is orphaned, so that alone does not count as still running.
fn is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .split(')')
        .next_back()
        .and_then(|rest| rest.split_whitespace().next());
    state != Some("Z")
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: TempDir,
}

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step — the same block, run again for the review step (`$3`), approves it,
/// and again for the test step, accepts it, so a task meant to succeed end to end still does.
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

    /// Runs `ktask-rs run` with `args` inside the repository. A task's own bash block —
    /// standing in for what a real agent would run — can call back in with
    /// `ktask-rs report`: the binary under test puts its own directory on that child's `PATH`
    /// itself, so this needs no help finding it.
    fn run_the_queue(&self, args: &[&str]) -> Result<Outcome> {
        self.run_the_queue_in(&self.repository, args)
    }

    /// Like [`Fixture::run_the_queue`], in `dir` instead of the repository.
    fn run_the_queue_in(&self, dir: &Path, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(dir, args)
    }

    /// Like [`Fixture::run_the_queue`], without waiting for it: the caller drives or kills
    /// the child itself.
    fn spawn_the_queue(&self, args: &[&str]) -> Result<Child> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        Ok(command.spawn()?)
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
fn run_uses_the_projects_attempt_timeout_setting_when_the_command_line_gives_none() -> Result<()> {
    let fixture = Fixture::new()?;
    let set = fixture.run(&["settings", "set", "attempt-timeout", "1"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    fixture.add_agent_task("a", "```bash\nsleep 30\n```\n")?;

    let started = Instant::now();
    let outcome = fixture.run_the_queue(&["run"])?;

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
fn the_command_lines_attempt_timeout_still_overrides_the_projects_setting() -> Result<()> {
    let fixture = Fixture::new()?;
    let set = fixture.run(&["settings", "set", "attempt-timeout", "1"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    fixture.add_agent_task(
        "a",
        "```bash\nsleep 2\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue(&["run", "--attempt-timeout", "30"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
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
fn run_refuses_to_skip_past_an_earlier_failed_task_naming_it_and_its_reason() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let first = fixture.run_the_queue(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    assert_eq!(fixture.task_status(2)?, "pending");

    let second = fixture.run_the_queue(&["run"])?;

    assert_eq!(second.code, Some(1), "{}", second.stderr);
    assert!(
        second.stdout.contains("task 1: failed: it broke"),
        "{}",
        second.stdout
    );
    // Nothing was started: task 2 is untouched, still with no attempt of its own, and task 1
    // has no second attempt.
    assert_eq!(fixture.task_status(1)?, "failed");
    assert_eq!(fixture.task_status(2)?, "pending");
    Ok(())
}

#[test]
fn run_refuses_to_skip_past_an_earlier_blocked_task() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &reporting_body_with_reason("needs-input", "which path?"),
    )?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let first = fixture.run_the_queue(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status(1)?, "blocked");

    let second = fixture.run_the_queue(&["run"])?;

    assert_eq!(second.code, Some(1), "{}", second.stderr);
    assert!(
        second.stdout.contains("task 1: blocked: which path?"),
        "{}",
        second.stdout
    );
    assert_eq!(fixture.task_status(2)?, "pending");
    Ok(())
}

#[test]
fn run_refuses_to_skip_past_an_earlier_failed_unknown_task() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", "```bash\necho did nothing\n```\n")?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let first = fixture.run_the_queue(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");

    let second = fixture.run_the_queue(&["run"])?;

    assert_eq!(second.code, Some(1), "{}", second.stderr);
    assert!(
        second.stdout.contains("task 1: failed-unknown:"),
        "{}",
        second.stdout
    );
    assert_eq!(fixture.task_status(2)?, "pending");
    Ok(())
}

#[test]
fn removing_the_failed_task_lets_the_next_run_continue_with_the_task_after_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let first = fixture.run_the_queue(&["run"])?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    let blocked = fixture.run_the_queue(&["run"])?;
    assert_eq!(blocked.code, Some(1), "{}", blocked.stderr);
    assert_eq!(fixture.task_status(2)?, "pending");

    let removed = fixture.run(&["remove", "1"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);

    let third = fixture.run_the_queue(&["run"])?;

    assert_eq!(third.code, Some(0), "{}", third.stderr);
    assert_eq!(fixture.task_status(2)?, "done");
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

#[test]
fn project_named_before_run_works_the_same_as_after() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;

    let outcome = fixture.run_the_queue_in(&elsewhere, &["--project", "my-app", "run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn project_named_twice_with_different_values_on_run_exits_two_and_runs_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;
    assert_eq!(
        fixture.sandbox.run(&elsewhere, &["project", "show"])?.code,
        Some(0)
    );

    let outcome = fixture.run_the_queue_in(
        &elsewhere,
        &["--project", "my-app", "run", "--project", "elsewhere"],
    )?;

    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("\"my-app\""), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("\"elsewhere\""),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    assert_eq!(fixture.task_status(1)?, "pending");
    Ok(())
}

#[test]
fn a_second_run_while_one_is_in_progress_exits_two_naming_the_running_process() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            go.display()
        ),
    )?;

    let mut first = fixture.spawn_the_queue(&["run"])?;
    let first_pid = first.id();
    wait_until("the first run's attempt to start", || {
        fixture.attempt_running_provider(1, 1).is_ok()
    })?;

    let second = fixture.run_the_queue(&["run"])?;
    assert_eq!(second.code, Some(2), "{}", second.stderr);
    assert!(
        second.stderr.contains(&first_pid.to_string()),
        "{}",
        second.stderr
    );
    // The lock refusal changed nothing: the first run's attempt is still the only one.
    assert_eq!(fixture.task_status(1)?, "running");

    std::fs::write(&go, "")?;
    let status = first.wait()?;
    assert!(status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

/// `SIGTERM` and `SIGINT` to `run` end the provider (and everything it started), record the
/// attempt `failed-unknown` with the reason "the run was interrupted" and its real duration,
/// print that line, and exit `1` — all before `run` itself exits, with no need for a second
/// `run` to notice anything.
fn signal_to_run_ends_it_at_once_with_the_interrupted_reason_and_real_duration(
    signal: Signal,
) -> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.work.join("provider.pid");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\necho $$ > \"{}\"\nsleep 30\n```\n",
            pid_file.display()
        ),
    )?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let first = fixture.spawn_the_queue(&["run"])?;
    let first_pid = i32::try_from(first.id())?;

    let provider_pid: u32 = wait_until_some("the provider to record its process id", || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    })?;
    assert!(
        is_running(provider_pid),
        "provider {provider_pid} is not running"
    );
    // Gives the attempt some real wall-clock time to run, so the duration `run` records for
    // it below is provably more than an instant, not just never having been reset. Nothing
    // in the journal changes while it is gated on the signal below, so there is no condition
    // to wait for here beyond real time itself passing.
    std::thread::park_timeout(Duration::from_millis(300));

    signal::kill(Pid::from_raw(first_pid), signal)?;
    let output = first.wait_with_output()?;

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("task 1: failed-unknown: the run was interrupted"),
        "{stdout}"
    );

    // The provider — and the `sleep 30` inside the same bash block it ran — do not outlive
    // the run that started them, even though nothing past this point runs another `run`.
    wait_until(
        "the provider to end once the run that started it ends",
        || !is_running(provider_pid),
    )?;

    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (duration_ms, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, None);
    assert_eq!(status, "failed-unknown");
    assert_eq!(reason.as_deref(), Some("the run was interrupted"));
    // Well under the 300ms slept above would mean the duration was never really measured;
    // a generous floor avoids the test being flaky under a loaded machine.
    assert!(duration_ms >= 200, "{duration_ms}");
    Ok(())
}

#[test]
fn sigterm_to_run_ends_it_at_once_with_the_interrupted_reason_and_real_duration() -> Result<()> {
    signal_to_run_ends_it_at_once_with_the_interrupted_reason_and_real_duration(Signal::SIGTERM)
}

#[test]
fn sigint_to_run_ends_it_at_once_with_the_interrupted_reason_and_real_duration() -> Result<()> {
    signal_to_run_ends_it_at_once_with_the_interrupted_reason_and_real_duration(Signal::SIGINT)
}

#[test]
fn a_run_killed_outright_does_not_leave_its_provider_running_and_status_shows_it_interrupted_at_once()
-> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.work.join("provider.pid");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\necho $$ > \"{}\"\nsleep 30\n```\n",
            pid_file.display()
        ),
    )?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let mut first = fixture.spawn_the_queue(&["run"])?;
    let first_pid = i32::try_from(first.id())?;

    let provider_pid: u32 = wait_until_some("the provider to record its process id", || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    })?;
    assert!(
        is_running(provider_pid),
        "provider {provider_pid} is not running"
    );

    // `SIGKILL` cannot be caught: `run` gets no chance to run any code of its own — unlike
    // `SIGTERM`/`SIGINT` — so the provider ending anyway proves the kernel-level tie
    // (`PR_SET_PDEATHSIG`), not the ordinary signal handling those two get.
    signal::kill(Pid::from_raw(first_pid), Signal::SIGKILL)?;
    let status = first.wait()?;
    assert!(!status.success(), "{status:?}");

    wait_until(
        "the provider to end once the run that started it is killed outright",
        || !is_running(provider_pid),
    )?;

    // Nothing has reconciled the journal yet — the killed run had no chance to — but `status`
    // must not show the task `running` regardless: no run is alive to finish it.
    let status_output = fixture.run(&["status"])?;
    assert_eq!(status_output.code, Some(0), "{}", status_output.stderr);
    assert!(
        status_output.stdout.contains("#1\tinterrupted\ta"),
        "{}",
        status_output.stdout
    );
    assert!(
        !status_output.stdout.contains("\trunning\t") && !status_output.stdout.contains("running"),
        "{}",
        status_output.stdout
    );
    // The journal itself is untouched: nothing ran to reconcile it.
    assert_eq!(fixture.task_status(1)?, "running");

    // The next real `run` reconciles it exactly as it always has.
    let second = fixture.run_the_queue(&["run"])?;
    assert_eq!(second.code, Some(1), "{}", second.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, None);
    assert_eq!(status, "failed-unknown");
    assert_eq!(reason.as_deref(), Some("the run was interrupted"));
    Ok(())
}

#[test]
fn a_run_killed_outright_leaves_nothing_its_script_started_alive_foreground_or_background()
-> Result<()> {
    let fixture = Fixture::new()?;
    let script_pid_file = fixture.work.join("script.pid");
    let bg_pid_file = fixture.work.join("bg.pid");
    // `PR_SET_PDEATHSIG` alone only ties the provider's own process (the top-level `bash`
    // `EXEC_TIED_TO_PARENT_MARKER` execs into, recorded here as `$$`) to this process's
    // death. Both pids checked below are processes *that script itself starts* — a plain
    // foreground `sleep`, which `bash` forks and waits on rather than `exec`ing into since it
    // is not alone in the script, and a `sleep` explicitly backgrounded with `&` — and
    // neither is the pid `PR_SET_PDEATHSIG` is ever set on.
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\necho $$ > \"{}\"\nsleep 30 &\necho $! > \"{}\"\nsleep 30\n```\n",
            script_pid_file.display(),
            bg_pid_file.display()
        ),
    )?;

    let mut first = fixture.spawn_the_queue(&["run"])?;
    let first_pid = i32::try_from(first.id())?;

    let bg_pid: u32 = wait_until_some("the background sleep to record its pid", || {
        std::fs::read_to_string(&bg_pid_file)
            .ok()?
            .trim()
            .parse()
            .ok()
    })?;
    let script_pid: u32 = std::fs::read_to_string(&script_pid_file)?.trim().parse()?;
    // The foreground `sleep 30` is the script's other child, forked once it moves past the
    // line above — not `exec`ed into in its own process, since it is not the script's last
    // command.
    let fg_pid = wait_until_some(
        "the foreground sleep to be forked as the script's other child",
        || {
            std::fs::read_to_string(format!("/proc/{script_pid}/task/{script_pid}/children"))
                .ok()?
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .find(|&pid| pid != bg_pid)
        },
    )?;
    assert!(
        is_running(bg_pid),
        "background {bg_pid} not running before kill"
    );
    assert!(
        is_running(fg_pid),
        "foreground {fg_pid} not running before kill"
    );

    // `SIGKILL` cannot be caught: `run` gets no chance to run any code of its own — the
    // whole point of the scenario.
    signal::kill(Pid::from_raw(first_pid), Signal::SIGKILL)?;
    let status = first.wait()?;
    assert!(!status.success(), "{status:?}");

    wait_until(
        "the background sleep to end once run is killed outright",
        || !is_running(bg_pid),
    )?;
    wait_until(
        "the foreground sleep to end once run is killed outright",
        || !is_running(fg_pid),
    )?;
    Ok(())
}

#[test]
fn a_different_ktask_rs_earlier_on_the_callers_path_does_not_stop_the_real_one_being_reached()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let decoy = decoy_ktask_rs_dir()?;

    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            with_dir_first_on_path(command, decoy.path());
        })?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn the_scripts_third_argument_is_the_steps_name() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "step=$3"))?;

    let run = fixture.run_the_queue(&["run"])?;

    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let (_, _, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(status, "failed");
    assert_eq!(reason.as_deref(), Some("step=implementation"));
    Ok(())
}

#[test]
fn the_implementation_review_test_and_commit_steps_each_record_exactly_one_start_and_one_end_event()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let run = fixture.run_the_queue(&["run"])?;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let database = rusqlite::Connection::open(fixture.journal())?;
    let count = |kind: &str| -> rusqlite::Result<i64> {
        database.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = ?1 AND task_id = 1",
            [kind],
            |row| row.get(0),
        )
    };
    assert_eq!(count("step_started")?, 4);
    assert_eq!(count("step_ended")?, 4);
    Ok(())
}

/// A prediction: the first task added to a fresh project, run once, gets this token — good
/// enough to build the exact prompt `ktask-rs run` will build for it, ahead of adding it.
fn first_attempt_token() -> AttemptToken {
    AttemptToken::new("my-app", TaskId(1), 1)
}

/// A minimal task, only as far as `ktask_core::build_prompt` cares: its title and criteria,
/// nothing about its body, which the caller supplies separately.
fn task_named(title: &str) -> Task {
    Task {
        id: TaskId(1),
        position: 1,
        title: title.to_owned(),
        body: String::new(),
        criteria: vec!["it works".to_owned()],
        kind: TaskKind::Agent,
        links: vec![],
        status: TaskStatus::Running,
        created_at: std::time::SystemTime::now(),
    }
}

#[test]
fn the_report_command_the_prompt_gives_the_agent_is_the_full_path_and_works_with_no_path_at_all()
-> Result<()> {
    // Builds, ahead of adding the task, the exact prompt `ktask-rs run` will build for its
    // one attempt, and pulls the `done` command out of it — the line a real agent, not
    // `echo`, would read and run verbatim.
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_ktask-rs"));
    let token = first_attempt_token();
    let prompt = ktask_core::build_prompt(&task_named("a"), &token, &binary_path);
    let done_line = prompt
        .lines()
        .find(|line| line.trim_start().ends_with(" done"))
        .expect("the prompt names a done command")
        .trim();
    assert!(
        done_line.starts_with(binary_path.to_str().unwrap()),
        "{done_line}"
    );
    // Same proof, for the review step's own prompt and its `approved` command.
    let review_prompt = ktask_core::build_review_prompt(&task_named("a"), &token, &binary_path, "");
    let approved_line = review_prompt
        .lines()
        .find(|line| line.trim_start().ends_with(" approved"))
        .expect("the review prompt names an approved command")
        .trim();
    assert!(
        approved_line.starts_with(binary_path.to_str().unwrap()),
        "{approved_line}"
    );
    // Same proof again, for the test step's own prompt and its `accepted` command.
    let test_prompt = ktask_core::build_test_prompt(&task_named("a"), &token, &binary_path, "");
    let accepted_line = test_prompt
        .lines()
        .find(|line| line.trim_start().ends_with(" accepted"))
        .expect("the test prompt names an accepted command")
        .trim();
    assert!(
        accepted_line.starts_with(binary_path.to_str().unwrap()),
        "{accepted_line}"
    );

    // The task's body runs one exact line or another, depending on the step, with its own
    // `PATH` blanked out first — proving each line is a complete, self-sufficient command that
    // in no way depends on `ktask-rs` being found on `PATH`, unlike the bare
    // `ktask-rs report ...` other tests here use.
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nPATH=\nif [ \"$3\" = \"review\" ]; then\n  {approved_line}\nelif [ \"$3\" = \"testing\" ]; then\n  {accepted_line}\nelse\n  {done_line}\nfi\n```\n"
        ),
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

/// A bash block that reports `done` for the implementation step, `outcome` (with `reason`,
/// when it is not empty) for the review step, and `accepted` for the test step, when reached.
fn review_body(outcome: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    } else {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    }
}

#[test]
fn an_approving_review_carries_the_task_on_as_done_and_the_review_line_shows_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &review_body("approved", ""))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    assert_eq!(
        status.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn a_reviewer_that_requests_changes_ends_the_task_failed_with_the_findings_as_the_reason_and_stops_the_run()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &review_body("changes-requested", "fix the thing"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (_, _, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(status, "failed");
    assert_eq!(reason.as_deref(), Some("fix the thing"));
    assert!(outcome.stdout.contains("failed"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("fix the thing"),
        "{}",
        outcome.stdout
    );
    // Both interfaces show the review line and the findings.
    let status_lines = fixture.run(&["status"])?;
    assert_eq!(
        status_lines.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tfailed\ta",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tchanges-requested\tfix the thing",
        ]
    );
    Ok(())
}

#[test]
fn a_reviewer_that_reports_nothing_ends_the_task_failed_unknown_and_stops_the_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  echo did nothing\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, Some(0));
    assert_eq!(status, "failed-unknown");
    assert!(
        reason.as_deref().unwrap().contains("reported nothing"),
        "{reason:?}"
    );
    Ok(())
}

#[test]
fn a_reviewer_past_its_time_limit_is_killed_and_ends_the_task_failed_unknown() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  sleep 30\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

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
fn ktask_rs_report_refuses_an_outcome_outside_the_running_step_naming_the_ones_that_do()
-> Result<()> {
    let fixture = Fixture::new()?;
    let stderr_file = fixture.work.join("review-stderr");
    let exit_file = fixture.work.join("review-exit");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" done 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            stderr_file.display(),
            exit_file.display(),
        ),
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;

    // The wrong-step report was refused, but the reviewer still went on to report the right
    // outcome, so the task completes normally.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    let exit_code: i32 = std::fs::read_to_string(&exit_file)?.trim().parse()?;
    assert_eq!(exit_code, 2);
    let stderr = std::fs::read_to_string(&stderr_file)?;
    assert!(
        stderr.contains("does not belong to the review step"),
        "{stderr}"
    );
    assert!(stderr.contains("approved"), "{stderr}");
    assert!(stderr.contains("changes-requested"), "{stderr}");
    Ok(())
}

/// A bash block that reports `done` for the implementation step, `approved` for the review
/// step, and `outcome` (with `reason`, when it is not empty) for the test step.
fn test_body(outcome: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    } else {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    }
}

#[test]
fn an_accepting_tester_carries_the_task_on_as_done_and_the_testing_line_shows_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &test_body("accepted", ""))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    assert_eq!(
        status.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn a_tester_that_rejects_ends_the_task_failed_with_what_failed_as_the_reason_and_stops_the_run()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &test_body("rejected", "the login button does nothing"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (_, _, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(status, "failed");
    assert_eq!(reason.as_deref(), Some("the login button does nothing"));
    assert!(outcome.stdout.contains("failed"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("the login button does nothing"),
        "{}",
        outcome.stdout
    );
    // Both interfaces show the testing line and the reason.
    let status_lines = fixture.run(&["status"])?;
    assert_eq!(
        status_lines.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tfailed\ta",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\trejected\tthe login button does nothing",
        ]
    );
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let entries: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(entries[0]["attempt"]["outcome"], "rejected");
    assert_eq!(
        entries[0]["attempt"]["reason"],
        "the login button does nothing"
    );
    Ok(())
}

#[test]
fn a_tester_that_reports_nothing_ends_the_task_failed_unknown_and_stops_the_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  echo did nothing\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;
    fixture.add_agent_task("b", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    assert_eq!(fixture.task_status(2)?, "pending");
    let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, Some(0));
    assert_eq!(status, "failed-unknown");
    assert!(
        reason.as_deref().unwrap().contains("reported nothing"),
        "{reason:?}"
    );
    Ok(())
}

#[test]
fn a_tester_that_crashes_ends_the_task_failed_unknown_and_stops_the_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  exit 7\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");
    let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
    assert_eq!(exit_code, Some(7));
    assert_eq!(status, "failed-unknown");
    assert!(
        reason.as_deref().unwrap().contains("reported nothing"),
        "{reason:?}"
    );
    Ok(())
}

#[test]
fn a_tester_past_its_time_limit_is_killed_and_ends_the_task_failed_unknown() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  sleep 30\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

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
