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
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use repo::{git_repository, scratch};
use rusqlite::OptionalExtension;
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// `stdout`'s lines after the first — the run band, which this file's tests do not need to
/// check since it is already covered, line by line, in `tests/status.rs`.
fn after_band(stdout: &str) -> Vec<&str> {
    stdout.lines().skip(1).collect()
}

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

/// However a test above left its `run` — waited on, signalled, or neither, when it panicked
/// first — nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step — the same block, run again for the review step (`$3`), approves it,
/// and again for the test step, accepts it, so a task meant to succeed end to end still does.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let fixture = Self {
            sandbox,
            work,
            repository,
            _keep: keep,
        };
        // Most of this file's tests are about one attempt's own outcome, not the resolver —
        // M4-04's own tests set `max-attempts` back up when they want it to run.
        fixture.run(&["settings", "set", "max-attempts", "1"])?;
        Ok(fixture)
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
        reason.as_deref().unwrap().contains("killed after"),
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
        reason.as_deref().unwrap().contains("killed after"),
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
        "```bash\nsleep 2\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
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
    assert!(help.stdout.contains("--json"), "{}", help.stdout);
    Ok(())
}

#[test]
fn json_reports_every_task_attempted_and_why_it_ended() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run", "--json"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let shown: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    assert_eq!(
        shown,
        serde_json::json!({
            "attempted": [{"id": 1, "status": "done", "reason": null}],
            "end": {"kind": "completed"},
        })
    );
    Ok(())
}

#[test]
fn json_reports_a_refusal_to_start_the_same_way_text_does() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let ran = fixture.run_the_queue(&["run"])?;
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);

    let outcome = fixture.run_the_queue(&["run", "--json"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let shown: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    assert_eq!(
        shown,
        serde_json::json!({"attempted": [], "end": {"kind": "nothing_pending"}})
    );
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
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
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

#[test]
fn the_report_command_the_prompt_gives_the_agent_is_the_full_path_and_works_with_no_path_at_all()
-> Result<()> {
    // Each step reads its own real prompt from the file path it receives as `$7`, pulls out
    // the one line ending in the outcome it is meant to report — `approved` for review,
    // `accepted` for testing, `done` for implementation — blanks its own `PATH`, then runs
    // that line verbatim. This proves the report line a real prompt gives an agent for each
    // step is a complete, self-sufficient command using the binary's full path, not merely
    // `ktask-rs`, against what the running system actually wrote — not a prediction from
    // `ktask_core::build_prompt` and friends called directly.
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  LINE=$(grep -E ' approved$' \"$7\")\nelif [ \"$3\" = \"testing\" ]; then\n  LINE=$(grep -E ' accepted$' \"$7\")\nelse\n  LINE=$(grep -E ' done$' \"$7\")\nfi\nPATH=\neval \"$LINE\"\n```\n",
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn the_prompt_scratch_file_lives_under_the_state_directory_and_is_removed_once_the_step_has_run()
-> Result<()> {
    let fixture = Fixture::new()?;
    let seen_path_file = fixture.work.join("prompt-path-seen");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  printf '%s' \"$7\" > {seen_path_file}\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            seen_path_file = seen_path_file.display(),
        ),
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);

    let prompt_path = PathBuf::from(std::fs::read_to_string(&seen_path_file)?);
    let project_state_dir = fixture.journal().parent().unwrap().to_path_buf();
    assert!(
        prompt_path.starts_with(&project_state_dir),
        "{} should live under {}",
        prompt_path.display(),
        project_state_dir.display()
    );
    assert!(
        !prompt_path.exists(),
        "{} should have been removed once the step ran",
        prompt_path.display()
    );
    Ok(())
}

/// A bash block that reports `done` for the implementation step, `outcome` (with `reason`,
/// when it is not empty) for the review step, and `accepted` for the test step, when reached.
fn review_body(outcome: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    } else {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
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
        after_band(&status.stdout),
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
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
        after_band(&status_lines.stdout),
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tchanges-requested\trouted: decide — rejected\tfix the thing\tusage none",
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
        reason.as_deref().unwrap().contains("killed after"),
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
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" done 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
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
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" {outcome}\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        )
    } else {
        format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
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
        after_band(&status.stdout),
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
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
        after_band(&status_lines.stdout),
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\trejected\trouted: decide — rejected\tthe login button does nothing\tusage none",
        ]
    );
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let entries: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(entries["tasks"][0]["attempt"]["outcome"], "rejected");
    assert_eq!(
        entries["tasks"][0]["attempt"]["reason"],
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
        reason.as_deref().unwrap().contains("killed after"),
        "{reason:?}"
    );
    Ok(())
}

/// Installs the hermetic `claude` executable used by the Claude provider tests. It receives
/// the real prompt on stdin, emits a representative stream-json transcript, then executes the
/// exact `report ... done` command the prompt gave it.
fn claude_script(script: &str) -> Result<TempDir> {
    let dir = TempDir::new()?;
    let path = dir.path().join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn select_claude(fixture: &Fixture) -> Result<()> {
    for (name, value) in [
        ("provider", "claude"),
        ("model", "claude-sonnet-5"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

/// Selects Codex for the implementation step alone, keeping this test focused on the provider
/// transcript rather than the review and testing workflows.
fn select_codex(fixture: &Fixture) -> Result<()> {
    for (name, value) in [
        ("provider", "codex"),
        ("model", "gpt-5-codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

/// Installs the hermetic `codex` executable used by the Codex provider tests.
fn codex_script(script: &str) -> Result<TempDir> {
    let dir = TempDir::new()?;
    let path = dir.path().join("codex");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

#[test]
fn recorded_codex_jsonl_runs_the_task_with_its_requested_model_session_and_usage() -> Result<()> {
    let fixture = Fixture::new()?;
    select_codex(&fixture)?;
    fixture.add_agent_task("a", "do the recorded work")?;
    let script = [
        "[ \"$1\" = exec ] && [ \"$2\" = --json ] && [ \"$3\" = --dangerously-bypass-approvals-and-sandbox ] && [ \"$4\" = --skip-git-repo-check ] && [ \"$5\" = -C ] && [ \"$6\" = \"$PWD\" ] && [ \"$7\" = --model ] && [ \"$8\" = gpt-5-codex ] && [ \"$9\" = - ] || exit 9",
        &format!("printf '%s' '{}'", include_str!("../../../test-fixtures/codex/codex-0.160.0-success.jsonl")),
        "prompt=$(cat)\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
    ]
    .join("\n");
    let codex = codex_script(&script)?;
    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            with_dir_first_on_path(command, codex.path());
        })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.attempt_running_provider(1, 1)?, "codex");
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.contains("implementation\tgpt-5-codex\tcodex"),
        "{}",
        status.stdout
    );
    assert!(
        status
            .stdout
            .contains("session:01a10555-6a4c-7f21-8ac4-aed0bf10dbb2"),
        "{}",
        status.stdout
    );
    assert!(
        status
            .stdout
            .contains("tokens in 13282 out 5 cost not reported"),
        "{}",
        status.stdout
    );
    let status_json: serde_json::Value =
        serde_json::from_str(&fixture.run(&["status", "--json"])?.stdout)?;
    assert_eq!(status_json["tasks"][0]["attempt"]["input_tokens"], 13_282);
    assert_eq!(status_json["tasks"][0]["attempt"]["output_tokens"], 5);
    assert_eq!(
        status_json["tasks"][0]["attempt"]["cost_usd"],
        serde_json::Value::Null
    );
    assert_eq!(
        status_json["tasks"][0]["attempt"]["steps"][0]["session"],
        "01a10555-6a4c-7f21-8ac4-aed0bf10dbb2"
    );
    let output = fixture.run(&["output", "1"])?;
    assert_eq!(output.code, Some(0), "{}", output.stderr);
    assert_eq!(
        output.stdout,
        "--- implementation · codex · gpt-5-codex ---\nturn started\nassistant: OK"
    );
    Ok(())
}

#[test]
fn a_codex_transport_failure_ends_in_the_decider_and_other_errors_keep_their_exit_reason()
-> Result<()> {
    for (script, timeout, expected_exit, expected_status, expected_reason) in [
        (
            format!(
                "cat >/dev/null\nprintf '%s' '{}' >&2\nexit 7",
                include_str!("../../../test-fixtures/codex/transport-failure-stderr.txt")
            ),
            None,
            Some(7),
            "failed-unknown",
            "stream disconnected before completion",
        ),
        (
            "cat >/dev/null\nsleep 30".to_owned(),
            Some("1"),
            None,
            "failed-unknown",
            "killed after",
        ),
    ] {
        let fixture = Fixture::new()?;
        select_codex(&fixture)?;
        fixture.add_agent_task("a", "do work")?;
        let codex = codex_script(&script)?;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(["run"]);
        if let Some(timeout) = timeout {
            command.args(["--attempt-timeout", timeout]);
        }
        fixture.sandbox.isolate(&mut command, &fixture.repository);
        with_dir_first_on_path(&mut command, codex.path());
        let output = command.output()?;
        assert_eq!(output.status.code(), Some(1));
        let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
        assert_eq!(exit_code, expected_exit);
        assert_eq!(status, expected_status);
        assert!(
            reason
                .as_deref()
                .unwrap_or_default()
                .contains(expected_reason),
            "{reason:?}"
        );
    }
    Ok(())
}

#[test]
fn recorded_claude_stream_json_runs_the_task_with_its_model() -> Result<()> {
    let fixture = Fixture::new()?;
    select_claude(&fixture)?;
    fixture.run(&["settings", "set", "model", "claude-haiku-4-5-20251001"])?;
    fixture.add_agent_task("a", "do the recorded work")?;
    let script = [
        "[ \"$1\" = --print ] && [ \"$2\" = --output-format ] && [ \"$3\" = stream-json ] && [ \"$4\" = --verbose ] && [ \"$5\" = --permission-mode ] && [ \"$6\" = bypassPermissions ] && [ \"$7\" = --model ] && [ \"$8\" = claude-haiku-4-5-20251001 ] && [ \"$9\" = --disallowedTools ] || exit 9\nfor tool in CronCreate CronDelete CronList Monitor ScheduleWakeup TaskOutput TaskStop; do case \",${10},\" in *\",$tool,\"*) ;; *) exit 9 ;; esac; done\ncase \",${10},\" in *\",Agent,\"*) exit 9 ;; esac",
        &format!("printf '%s' '{}'", include_str!("../../../test-fixtures/claude/success.jsonl")),
        "prompt=$(cat)\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
    ]
    .join("\n");
    let claude = claude_script(&script)?;
    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            with_dir_first_on_path(command, claude.path());
        })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    assert_eq!(fixture.attempt_running_provider(1, 1)?, "claude");
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.contains("claude-haiku-4-5-20251001\tclaude"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("tokens in 10 out 56 cost $0.011002"),
        "{}",
        status.stdout
    );
    let status_json: serde_json::Value =
        serde_json::from_str(&fixture.run(&["status", "--json"])?.stdout)?;
    assert_eq!(status_json["tasks"][0]["attempt"]["input_tokens"], 10);
    assert_eq!(status_json["tasks"][0]["attempt"]["output_tokens"], 56);
    assert_eq!(status_json["tasks"][0]["attempt"]["cost_usd"], "0.011002");
    assert_eq!(
        status_json["tasks"][0]["attempt"]["steps"][0]["model"],
        "claude-haiku-4-5-20251001"
    );
    Ok(())
}

#[test]
fn a_provider_reported_model_mismatch_fails_the_attempt_with_both_model_names() -> Result<()> {
    let fixture = Fixture::new()?;
    select_claude(&fixture)?;
    fixture.add_agent_task("a", "do the recorded work")?;
    let claude = claude_script(&format!(
        "prompt=$(cat)\nprintf '%s' '{}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
        include_str!("../../../test-fixtures/claude/success.jsonl")
    ))?;
    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            with_dir_first_on_path(command, claude.path());
        })?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "failed");
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.contains(
            "failed\trouted: decide — unmatched\tasked for claude-sonnet-5, the provider used claude-haiku-4-5-20251001"
        ),
        "{}",
        status.stdout
    );
    Ok(())
}

#[test]
fn a_project_claude_deny_list_replaces_the_built_in_list() -> Result<()> {
    let fixture = Fixture::new()?;
    select_claude(&fixture)?;
    fixture.add_agent_task("a", "do the recorded work")?;
    let settings = fixture.sandbox.state_dir().join("my-app/settings.toml");
    let mut configured = std::fs::read_to_string(&settings)?;
    configured.push_str(
        "\n[providers.claude]\ndenied-tools = [\"ProjectSchedule\", \"ProjectMonitor\"]\n",
    );
    std::fs::write(settings, configured)?;
    let claude = claude_script(
        "[ \"$9\" = --disallowedTools ] && [ \"${10}\" = ProjectSchedule,ProjectMonitor ] || exit 9\nprompt=$(cat)\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"finished\"}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
    )?;

    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            with_dir_first_on_path(command, claude.path());
        })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn a_claude_error_or_timeout_keeps_the_exit_reason() -> Result<()> {
    for (script, timeout, expected_exit, expected_reason) in [
        ("cat >/dev/null\nexit 7", None, Some(7), "reported nothing"),
        ("cat >/dev/null\nsleep 30", Some("1"), None, "killed after"),
    ] {
        let fixture = Fixture::new()?;
        select_claude(&fixture)?;
        fixture.add_agent_task("a", "do work")?;
        let claude = claude_script(script)?;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(["run"]);
        if let Some(timeout) = timeout {
            command.args(["--attempt-timeout", timeout]);
        }
        fixture.sandbox.isolate(&mut command, &fixture.repository);
        with_dir_first_on_path(&mut command, claude.path());
        let output = command.output()?;
        assert_eq!(output.status.code(), Some(1));
        let (_, exit_code, status, reason) = fixture.attempt_ended(1)?;
        assert_eq!(exit_code, expected_exit);
        assert_eq!(status, "failed-unknown");
        assert!(
            reason
                .as_deref()
                .unwrap_or_default()
                .contains(expected_reason)
        );
    }
    Ok(())
}

#[test]
fn agent_and_resolver_settings_run_only_their_own_steps_and_status_names_each_actual_provider_and_model()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["settings", "set", "max-attempts", "2"])?;
    let seen = fixture.work.join("providers-seen");
    let runner = fixture.work.join("record-provider");
    std::fs::write(
        &runner,
        format!(
            "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in\n  *'# Review:'*) step=review; outcome=approved ;;\n  *'# Test:'*) step=testing; outcome=rejected ;;\n  *'# Resolve:'*) step=resolve; outcome=stop ;;\n  *) step=implementation; outcome=done ;;\nesac\nprintf '%s:%s\\n' \"$step\" \"$1\" >> \"{}\"\nreport=$(printf '%s\\n' \"$prompt\" | sed -n \"s/^    //; / report --token .* $outcome/p\" | head -n 1)\neval \"$report\"\n",
            seen.display()
        ),
    )?;
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))?;
    let settings = fixture.sandbox.state_dir().join("my-app/settings.toml");
    let mut configured = std::fs::read_to_string(&settings)?;
    let _ = write!(
        configured,
        "\nprovider = \"agent\"\nmodel = \"agent-model\"\nresolver-provider = \"resolver\"\nresolver-model = \"resolver-model\"\n\
         [providers.agent]\ncommand = \"{runner}\"\nmodel = [\"{{model}}\"]\nparser = \"plain\"\n\
         [providers.resolver]\ncommand = \"{runner}\"\nmodel = [\"{{model}}\"]\nparser = \"plain\"\n",
        runner = runner.display(),
    );
    std::fs::write(&settings, configured)?;
    fixture.add_agent_task(
        "separate agent settings",
        "the configured providers report each pipeline outcome",
    )?;

    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert_eq!(
        std::fs::read_to_string(seen)?,
        "implementation:agent-model\nreview:agent-model\ntesting:agent-model\nresolve:resolver-model\n"
    );
    let status = fixture.run(&["status"])?;
    for line in [
        "implementation\tagent-model\tagent",
        "review\tagent-model\tagent",
        "testing\tagent-model\tagent",
        "resolve\tresolver-model\tresolver",
    ] {
        assert!(
            status.stdout.contains(line),
            "missing {line:?} in {}",
            status.stdout
        );
    }
    Ok(())
}

#[test]
fn recorded_claude_and_codex_runs_mix_project_roles_and_a_tasks_own_provider_and_model()
-> Result<()> {
    let fixture = Fixture::new()?;
    for (name, value) in [
        ("max-attempts", "2"),
        ("provider", "claude"),
        ("model", "claude-haiku-4-5-20251001"),
        ("resolver-provider", "codex"),
        ("resolver-model", "gpt-5-codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    fixture.add_agent_task("project provider", "first task reaches the resolver")?;
    let added = fixture.run(&[
        "add",
        "--title",
        "task override",
        "--criterion",
        "it works",
        "--provider",
        "codex",
        "--model",
        "gpt-5-codex",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let claude = claude_script(&format!(
        "prompt=$(cat)\nprintf '%s' '{}'\ntoken=$(printf '%s\\n' \"$prompt\" | sed -n 's/.*--token \\([^ ]*\\).*/\\1/p' | head -n 1)\nktask-rs report --token \"$token\" failed --reason recorded",
        include_str!("../../../test-fixtures/claude/success.jsonl")
    ))?;
    let codex = codex_script(&format!(
        "prompt=$(cat)\nprintf '%s' '{}'\ntoken=$(printf '%s\\n' \"$prompt\" | sed -n 's/.*--token \\([^ ]*\\).*/\\1/p' | head -n 1)\ncase \"$prompt\" in *'# Resolve:'*) ktask-rs report --token \"$token\" skip --reason resolved;; *) ktask-rs report --token \"$token\" done;; esac",
        include_str!("../../../test-fixtures/codex/codex-0.160.0-success.jsonl")
    ))?;
    let mut paths = vec![claude.path().to_path_buf(), codex.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let providers_path = std::env::join_paths(paths)?;
    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            command.env("PATH", &providers_path);
        })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);

    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    for line in [
        "attempt 1: implementation\tclaude-haiku-4-5-20251001\tclaude",
        "attempt 1: resolve\tgpt-5-codex\tcodex",
        "attempt 1: implementation\tgpt-5-codex\tcodex",
    ] {
        assert!(
            status.stdout.contains(line),
            "missing {line:?} in {}",
            status.stdout
        );
    }
    let raw = fixture.run(&["output", "1", "--attempt", "1", "--raw"])?;
    assert!(
        raw.stdout.contains("{\"type\":\"assistant\""),
        "{}",
        raw.stdout
    );
    assert!(
        raw.stdout.contains("{\"type\":\"thread.started\""),
        "{}",
        raw.stdout
    );
    let listed: serde_json::Value =
        serde_json::from_str(&fixture.run(&["list", "--all", "--json"])?.stdout)?;
    assert_eq!(listed[1]["provider"], "codex");
    assert_eq!(listed[1]["model"], "gpt-5-codex");
    Ok(())
}
