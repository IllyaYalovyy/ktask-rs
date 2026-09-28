//! `ktask-rs status` on the real binary: what ran and how it ended.
//!
//! `run` stops at the first task whose attempt does not report `done`, so a queue that
//! exercises every ending needs one `run` call per stop; each call picks up at the next
//! pending task, in queue order, exactly where the previous one left off.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

/// A bash block that reports `outcome` for whatever token it is given as `$1`.
fn reporting_body(outcome: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome}\n```\n")
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\n```\n")
}

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, so a task's own
/// bash block can call back into `ktask-rs report`.
fn with_nested_ktask_rs_on_path(command: &mut std::process::Command) {
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

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` with `args` inside the repository, its `PATH` carrying the
    /// directory of the `ktask-rs` under test, so a task's own bash block can call it back in
    /// with `ktask-rs report`.
    fn run_the_queue(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, args, with_nested_ktask_rs_on_path)
    }

    /// Like [`Fixture::run_the_queue`], without waiting for it: the caller drives or kills the
    /// child itself.
    fn spawn_the_queue(&self, args: &[&str]) -> Result<Child> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        with_nested_ktask_rs_on_path(&mut command);
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

    fn journal(&self) -> PathBuf {
        self.sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("journal.db")
    }

    /// The provider recorded for attempt 1 of `task`.
    fn attempt_running_provider(&self, task: u64) -> Result<String> {
        let database = rusqlite::Connection::open(self.journal())?;
        let payload: String = database.query_row(
            "SELECT payload FROM events WHERE kind = 'attempt_running' AND task_id = ?1",
            [i64::try_from(task)?],
            |row| row.get(0),
        )?;
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        Ok(payload
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    /// Runs `ktask-rs run` again and again, until the queue has nothing left pending or is
    /// empty, so that every task that stops one run is given its attempt.
    fn run_the_queue_to_completion(&self) -> Result<()> {
        for _ in 0..10 {
            let outcome = self.run_the_queue(&["run"])?;
            if outcome.stdout.contains("nothing is pending") || outcome.stdout.contains("empty") {
                return Ok(());
            }
        }
        Err("the queue never finished".into())
    }
}

#[test]
fn a_project_with_no_attempts_prints_nothing_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run(&["status"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    Ok(())
}

#[test]
fn an_empty_queue_also_prints_nothing_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["status"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    assert_eq!(json.stdout, "[]\n");
    Ok(())
}

/// A queue whose five tasks, once run to completion, end at every one of `status`'s outcome
/// labels: `done`, `failed`, `too-large`, `needs-input` and the tool's own `failed-unknown`.
fn queue_with_every_outcome(fixture: &Fixture) -> Result<()> {
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("c", &reporting_body_with_reason("too-large", "split me"))?;
    fixture.add_agent_task(
        "d",
        &reporting_body_with_reason("needs-input", "which path?"),
    )?;
    fixture.add_agent_task("e", "```bash\necho did nothing\n```\n")?;
    fixture.run_the_queue_to_completion()?;
    Ok(())
}

#[test]
fn status_shows_every_task_in_queue_order_with_its_title_status_and_attempt_line() -> Result<()> {
    let fixture = Fixture::new()?;
    queue_with_every_outcome(&fixture)?;

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines.len(), 10, "{lines:#?}");

    assert_eq!(lines[0], "#1\tdone\ta");
    assert_eq!(lines[1], "\timplementation\techo\t0s\tdone");

    assert_eq!(lines[2], "#2\tfailed\tb");
    assert_eq!(lines[3], "\timplementation\techo\t0s\tfailed\tit broke");

    assert_eq!(lines[4], "#3\tfailed\tc");
    assert_eq!(lines[5], "\timplementation\techo\t0s\ttoo-large\tsplit me");

    assert_eq!(lines[6], "#4\tblocked\td");
    assert_eq!(
        lines[7],
        "\timplementation\techo\t0s\tneeds-input\twhich path?"
    );

    assert_eq!(lines[8], "#5\tfailed-unknown\te");
    assert!(
        lines[9].starts_with("\timplementation\techo\t0s\tfailed-unknown\t"),
        "{}",
        lines[9]
    );
    assert!(lines[9].contains("reported nothing"), "{}", lines[9]);
    Ok(())
}

#[test]
fn status_json_carries_the_same_information_as_the_text_form() -> Result<()> {
    let fixture = Fixture::new()?;
    queue_with_every_outcome(&fixture)?;

    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let entries: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 5);

    let shown: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry["id"].as_u64().unwrap(),
                entry["status"].as_str().unwrap().to_owned(),
                entry["title"].as_str().unwrap().to_owned(),
                entry["attempt"]["outcome"].as_str().unwrap().to_owned(),
                entry["attempt"]["reason"].as_str().map(str::to_owned),
            )
        })
        .collect();
    let expected: Vec<(u64, String, String, String, Option<String>)> = vec![
        (1, "done".into(), "a".into(), "done".into(), None),
        (
            2,
            "failed".into(),
            "b".into(),
            "failed".into(),
            Some("it broke".into()),
        ),
        (
            3,
            "failed".into(),
            "c".into(),
            "too-large".into(),
            Some("split me".into()),
        ),
        (
            4,
            "blocked".into(),
            "d".into(),
            "needs-input".into(),
            Some("which path?".into()),
        ),
        (
            5,
            "failed-unknown".into(),
            "e".into(),
            "failed-unknown".into(),
            Some("the provider exited with code 0 and reported nothing".into()),
        ),
    ];
    assert_eq!(shown, expected);
    for entry in entries {
        assert_eq!(entry["attempt"]["step"], "implementation");
        assert_eq!(entry["attempt"]["provider"], "echo");
        assert_eq!(entry["attempt"]["number"], 1);
        assert!(entry["attempt"]["time_spent_seconds"].as_u64().is_some());
    }
    // The one failed_unknown attempt still carries a reason of its own — the tool's, not the
    // agent's, since the agent never reported one.
    assert!(
        entries[4]["attempt"]["reason"]
            .as_str()
            .unwrap()
            .contains("reported nothing")
    );
    Ok(())
}

#[test]
fn a_pending_task_never_attempted_does_not_appear() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    fixture.add_agent_task("c", &reporting_body("done"))?;

    let outcome = fixture.run(&["status"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert!(!outcome.stdout.contains("\tc"), "{}", outcome.stdout);
    assert_eq!(outcome.stdout.lines().count(), 4);
    Ok(())
}

#[test]
fn while_a_run_is_in_progress_the_running_task_shows_its_elapsed_time_so_far() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nktask-rs report --token \"$1\" done\n```\n",
            go.display()
        ),
    )?;

    let mut child = fixture.spawn_the_queue(&["run"])?;
    wait_until("the attempt to start running", || {
        fixture.attempt_running_provider(1).is_ok()
    })?;
    std::thread::sleep(Duration::from_millis(1_200));

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines[0], "#1\trunning\ta");
    assert_eq!(lines.len(), 2, "{lines:#?}");
    let fields: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(fields.len(), 5, "{}", lines[1]);
    assert_eq!(&fields[..3], ["", "implementation", "echo"], "{}", lines[1]);
    assert_eq!(fields[4], "running");
    let seconds: u64 = fields[3].strip_suffix('s').unwrap().parse().unwrap();
    assert!((1..10).contains(&seconds), "{seconds}");

    let json = fixture.run(&["status", "--json"])?;
    let entries: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let seconds = entries[0]["attempt"]["time_spent_seconds"]
        .as_u64()
        .unwrap();
    assert!((1..10).contains(&seconds), "{seconds}");
    assert_eq!(entries[0]["attempt"]["outcome"], "running");
    assert!(entries[0]["attempt"]["reason"].is_null());

    std::fs::write(&go, "")?;
    let status = child.wait()?;
    assert!(status.success(), "{status:?}");
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

#[test]
fn a_task_left_running_by_a_run_killed_outright_shows_interrupted_not_running() -> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.work.join("provider.pid");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\necho $$ > \"{}\"\nsleep 30\n```\n",
            pid_file.display()
        ),
    )?;

    let mut run = fixture.spawn_the_queue(&["run"])?;
    let run_pid = run.id();
    wait_until("the provider to record its process id", || {
        pid_file.exists()
    })?;
    let provider_pid: u32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    assert!(
        is_running(provider_pid),
        "provider {provider_pid} is not running"
    );

    // `SIGKILL` cannot be caught: `run` gets no chance to record anything, unlike
    // `SIGTERM`/`SIGINT`. `status` must not show `running` regardless, with no next `run`
    // needed to notice.
    signal::kill(Pid::from_raw(i32::try_from(run_pid)?), Signal::SIGKILL)?;
    let status = run.wait()?;
    assert!(!status.success(), "{status:?}");
    wait_until(
        "the provider to end once the run that started it is killed outright",
        || !is_running(provider_pid),
    )?;

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout.lines().next(), Some("#1\tinterrupted\ta"));
    assert!(!outcome.stdout.contains("running"), "{}", outcome.stdout);

    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let entries: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(entries[0]["status"], "interrupted");
    assert_eq!(entries[0]["attempt"]["outcome"], "interrupted");

    Ok(())
}

#[test]
fn status_and_its_options_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("status"), "{}", top.stdout);
    let help = fixture.run(&["status", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    assert!(help.stdout.contains("--json"), "{}", help.stdout);
    assert!(help.stdout.contains("--project"), "{}", help.stdout);
    Ok(())
}

#[test]
fn status_works_from_a_subdirectory_and_with_project_from_any_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;

    let outcome = fixture
        .sandbox
        .run(&elsewhere, &["status", "--project", "my-app"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert!(outcome.stdout.contains("#1\tdone\ta"), "{}", outcome.stdout);
    Ok(())
}
