//! `health-check` on the real binary: a task's implementation only starts once the project's
//! configured health check has passed; a failing one stops the run before any attempt begins,
//! and one that runs past the attempt time limit is killed and counts as failing too.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, so a task's own
/// bash block can call back into `ktask-rs report`.
fn with_nested_ktask_rs_on_path(command: &mut Command) {
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
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
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

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` with the directory of the `ktask-rs` under test on `PATH`, so a
    /// task's own bash block can call it back with `ktask-rs report`.
    fn run_the_queue(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, args, with_nested_ktask_rs_on_path)
    }

    /// Like [`Fixture::run_the_queue`], without waiting for it: the caller drives or kills the
    /// child itself.
    fn spawn_the_queue(&self, args: &[&str]) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        with_nested_ktask_rs_on_path(&mut command);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
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

    /// Sets the project's health-check command to `command`.
    fn set_health_check(&self, command: &str) -> Result<()> {
        let set = self.run(&["settings", "set", "health-check", command])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    /// Task `id`'s current status, as `list --json` shows it.
    fn task_status(&self, id: u64) -> Result<String> {
        let outcome = self.run(&["list", "--json"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        let tasks: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
        let task = tasks
            .as_array()
            .ok_or("not a JSON array")?
            .iter()
            .find(|task| task["id"].as_u64() == Some(id))
            .ok_or("no such task")?;
        Ok(task["status"]
            .as_str()
            .ok_or("status is not a string")?
            .to_owned())
    }

    /// `status`'s text lines, once there are at least `count` of them; fails after 10s.
    fn wait_for_status_lines(&self, count: usize) -> Result<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let outcome = self.run(&["status"])?;
            let lines: Vec<String> = outcome.stdout.lines().map(str::to_owned).collect();
            if lines.len() >= count {
                return Ok(lines);
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} status lines: {lines:?}"
            );
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}

/// A bash block that reports `outcome` for whatever token it is given as `$1` — for the
/// implementation step; the same block, run again for the review and test steps, approves and
/// accepts, so a task meant to succeed end to end still does.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

#[test]
fn a_passing_health_check_is_the_first_line_with_its_time_and_passed_and_the_task_carries_on()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_health_check("echo checking")?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            go.display()
        ),
    )?;

    let mut child = fixture.spawn_the_queue(&["run"])?;
    let lines = fixture.wait_for_status_lines(3)?;

    assert_eq!(lines[0], "#1\trunning\ta");
    assert!(
        lines[1].starts_with("\tattempt 1: health check\t-\t") && lines[1].ends_with("\tpassed"),
        "{lines:?}"
    );
    assert!(
        lines[2].starts_with("\tattempt 1: implementation\techo\t"),
        "{lines:?}"
    );

    let json = fixture.run(&["status", "--json"])?;
    let entries: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let steps = entries[0]["attempt"]["steps"].as_array().unwrap();
    // The implementation step is still gated on `go`: the review step has not begun yet.
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert_eq!(steps[0]["step"], "health check");
    assert_eq!(steps[0]["outcome"], "passed");
    assert!(steps[0]["time_spent_seconds"].as_u64().is_some());
    assert_eq!(steps[1]["step"], "implementation");

    std::fs::write(&go, "")?;
    let status = child.wait()?;
    assert!(status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn a_failing_health_check_stops_the_run_before_any_attempt_and_the_task_stays_pending() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.set_health_check("echo building; echo ERROR: nope >&2; exit 1")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome
            .stdout
            .contains("health check failed: echo building; echo ERROR: nope >&2; exit 1"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("exited with code 1"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("nope"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("was not started"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.task_status(1)?, "pending");
    // No attempt was ever begun, but the stop itself is not lost: `status` shows why the task
    // did not start, in the run's own words, and the task is still pending.
    let status = fixture.run(&["status"])?;
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tpending\ta");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("\thealth check\t-\t0s\tfailed\t"),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("exited with code 1"), "{}", lines[1]);
    assert!(lines[1].contains("fix the health check"), "{}", lines[1]);
    Ok(())
}

#[test]
fn once_a_later_run_gets_past_the_health_check_the_earlier_stop_is_no_longer_current() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.set_health_check("exit 1")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    let status = fixture.run(&["status"])?;
    assert!(status.stdout.contains("health check"), "{}", status.stdout);

    // The health check now passes: a second run gets past it and the task finishes.
    fixture.set_health_check("true")?;
    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");

    // The earlier gate stop is no longer shown as current: `status` now shows the real
    // attempt's own steps, with no trace of the health check ever having failed.
    let status = fixture.run(&["status"])?;
    assert!(!status.stdout.contains("failed"), "{}", status.stdout);
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tdone\ta");
    assert!(
        lines[1].starts_with("\tattempt 1: health check\t-\t") && lines[1].ends_with("\tpassed"),
        "{}",
        lines[1]
    );
    Ok(())
}

#[test]
fn a_health_check_past_its_time_limit_is_killed_and_counts_as_failing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_health_check("sleep 30")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let started = Instant::now();
    let outcome = fixture.run_the_queue(&["run", "--attempt-timeout", "1"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(outcome.stdout.contains("time limit"), "{}", outcome.stdout);
    assert_eq!(fixture.task_status(1)?, "pending");
    Ok(())
}

#[test]
fn no_health_check_command_set_skips_the_step_and_leaves_no_line() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let run = fixture.run_the_queue(&["run"])?;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let outcome = fixture.run(&["status"])?;
    assert_eq!(
        outcome.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta",
            "\tattempt 1: implementation\techo\t0s\tdone",
            "\tattempt 1: review\techo\t0s\tapproved",
            "\tattempt 1: testing\techo\t0s\taccepted",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}
