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

/// Waits, for up to a few seconds, until `condition` holds, polling every 20ms; fails naming
/// `what` when it never does.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
fn a_second_run_while_one_is_in_progress_exits_two_naming_the_running_process() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nktask-rs report --token \"$1\" done\n```\n",
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

    wait_until("the provider to record its process id", || {
        pid_file.exists()
    })?;
    let provider_pid: u32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    assert!(
        is_running(provider_pid),
        "provider {provider_pid} is not running"
    );
    // Gives the attempt some real wall-clock time to run, so the duration `run` records for
    // it below is provably more than an instant, not just never having been reset.
    std::thread::sleep(Duration::from_millis(300));

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

    wait_until("the provider to record its process id", || {
        pid_file.exists()
    })?;
    let provider_pid: u32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
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

    // The task's body is that exact line, with its own `PATH` blanked out first — proving
    // the line is a complete, self-sufficient command that in no way depends on `ktask-rs`
    // being found on `PATH`, unlike the bare `ktask-rs report ...` other tests here use.
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &format!("```bash\nPATH=\n{done_line}\n```\n"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}
