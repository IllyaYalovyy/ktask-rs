//! `ktask-rs status` on the real binary: what ran and how it ended.
//!
//! `run` refuses to attempt anything once an earlier task is left `failed`, `blocked` or
//! `failed-unknown`, so a queue that exercises every ending `status` distinguishes needs one
//! project per ending, each run once: a `done` filler task, then the task whose ending is
//! under test.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
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

/// A fresh project with just a `done` task, run to completion.
fn done_fixture() -> Result<Fixture> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    Ok(fixture)
}

/// A fresh project with a `done` filler task `x`, then a task named `title` whose body is
/// `body`, run until the second task stops it — so `status` has both, in queue order.
fn fixture_stopped_by(title: &str, body: &str) -> Result<Fixture> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("x", &reporting_body("done"))?;
    fixture.add_agent_task(title, body)?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    Ok(fixture)
}

fn failed_fixture() -> Result<Fixture> {
    fixture_stopped_by("b", &reporting_body_with_reason("failed", "it broke"))
}

fn too_large_fixture() -> Result<Fixture> {
    fixture_stopped_by("c", &reporting_body_with_reason("too-large", "split me"))
}

fn needs_input_fixture() -> Result<Fixture> {
    fixture_stopped_by(
        "d",
        &reporting_body_with_reason("needs-input", "which path?"),
    )
}

fn failed_unknown_fixture() -> Result<Fixture> {
    fixture_stopped_by("e", "```bash\necho did nothing\n```\n")
}

#[test]
fn status_shows_every_ending_in_queue_order_with_its_title_status_and_attempt_line() -> Result<()> {
    let fixture = done_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );

    let fixture = failed_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\tx",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
            "#2\tfailed\tb",
            "\timplementation\techo\t0s\tfailed\tit broke",
        ]
    );

    let fixture = too_large_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\tx",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
            "#2\tfailed\tc",
            "\timplementation\techo\t0s\ttoo-large\tsplit me",
        ]
    );

    let fixture = needs_input_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\tx",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
            "#2\tblocked\td",
            "\timplementation\techo\t0s\tneeds-input\twhich path?",
        ]
    );

    let fixture = failed_unknown_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines.len(), 7, "{lines:#?}");
    assert_eq!(lines[0], "#1\tdone\tx");
    assert_eq!(lines[1], "\timplementation\techo\t0s\tdone");
    assert_eq!(lines[2], "\treview\techo\t0s\tapproved");
    assert_eq!(lines[3], "\ttesting\techo\t0s\taccepted");
    assert_eq!(lines[4], "\tcommit\t-\t0s\tpassed\tnothing was changed");
    assert_eq!(lines[5], "#2\tfailed-unknown\te");
    assert!(
        lines[6].starts_with("\timplementation\techo\t0s\tfailed-unknown\t"),
        "{}",
        lines[6]
    );
    assert!(lines[6].contains("reported nothing"), "{}", lines[6]);
    Ok(())
}

/// `(id, status, title, attempt.outcome, attempt.reason)` for one entry of a `status --json`
/// array.
type ShownEntry = (u64, String, String, String, Option<String>);

#[test]
fn status_json_carries_the_same_information_as_the_text_form() -> Result<()> {
    /// `entries["id"/"status"/"title"/"attempt"."outcome"/"attempt"."reason"]`, in order, for
    /// every entry of a `status --json` array.
    fn shown(stdout: &str) -> Result<Vec<ShownEntry>> {
        let entries: serde_json::Value = serde_json::from_str(stdout)?;
        Ok(entries
            .as_array()
            .unwrap()
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
            .collect())
    }

    /// The entry a `done` filler task shows, once its commit step found nothing to commit.
    fn done_row(id: u64, title: &str) -> ShownEntry {
        (
            id,
            "done".into(),
            title.into(),
            "passed".into(),
            Some("nothing was changed".into()),
        )
    }

    let fixture = done_fixture()?;
    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(shown(&outcome.stdout)?, vec![done_row(1, "a")]);

    let fixture = failed_fixture()?;
    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        shown(&outcome.stdout)?,
        vec![
            done_row(1, "x"),
            (
                2,
                "failed".into(),
                "b".into(),
                "failed".into(),
                Some("it broke".into()),
            ),
        ]
    );

    let fixture = too_large_fixture()?;
    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        shown(&outcome.stdout)?,
        vec![
            done_row(1, "x"),
            (
                2,
                "failed".into(),
                "c".into(),
                "too-large".into(),
                Some("split me".into()),
            ),
        ]
    );

    let fixture = needs_input_fixture()?;
    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        shown(&outcome.stdout)?,
        vec![
            done_row(1, "x"),
            (
                2,
                "blocked".into(),
                "d".into(),
                "needs-input".into(),
                Some("which path?".into()),
            ),
        ]
    );

    let fixture = failed_unknown_fixture()?;
    let outcome = fixture.run(&["status", "--json"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let shown_entries = shown(&outcome.stdout)?;
    assert_eq!(shown_entries[0], done_row(1, "x"));
    assert_eq!(shown_entries[1].0, 2);
    assert_eq!(shown_entries[1].1, "failed-unknown");
    assert_eq!(shown_entries[1].2, "e");
    assert_eq!(shown_entries[1].3, "failed-unknown");
    // The failed_unknown attempt still carries a reason of its own — the tool's, not the
    // agent's, since the agent never reported one.
    assert!(
        shown_entries[1]
            .4
            .as_deref()
            .unwrap()
            .contains("reported nothing"),
        "{:?}",
        shown_entries[1].4
    );

    let entries: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    for entry in entries.as_array().unwrap() {
        assert_eq!(entry["attempt"]["number"], 1);
        assert!(entry["attempt"]["time_spent_seconds"].as_u64().is_some());
        let steps = entry["attempt"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["step"], "implementation");
        // Steps an agent runs name the provider it ran with; steps the tool runs itself name
        // none.
        assert_eq!(steps[0]["provider"], "echo");
        if entry["status"] == "done" {
            // `x`, the filler task: all four steps ran and passed, so the commit step —
            // which found nothing to commit — is current.
            assert_eq!(entry["attempt"]["step"], "commit");
            assert!(entry["attempt"]["provider"].is_null());
            assert_eq!(steps.len(), 4, "{steps:?}");
            assert_eq!(steps[1]["step"], "review");
            assert_eq!(steps[1]["provider"], "echo");
            assert_eq!(steps[2]["step"], "testing");
            assert_eq!(steps[2]["provider"], "echo");
            assert_eq!(steps[3]["step"], "commit");
            assert!(steps[3]["provider"].is_null());
        } else {
            // `e`: the implementation step itself never reported, so it is the only one, and
            // stays current.
            assert_eq!(entry["attempt"]["step"], "implementation");
            assert_eq!(entry["attempt"]["provider"], "echo");
            assert_eq!(steps.len(), 1, "{steps:?}");
        }
    }
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
    assert!(
        !outcome.stdout.lines().any(|line| line.ends_with("\tc")),
        "{}",
        outcome.stdout
    );
    assert_eq!(outcome.stdout.lines().count(), 10);
    Ok(())
}

#[test]
fn while_a_run_is_in_progress_the_running_task_shows_its_elapsed_time_so_far() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            go.display()
        ),
    )?;

    let mut child = fixture.spawn_the_queue(&["run"])?;
    wait_until("the attempt to start running", || {
        fixture.attempt_running_provider(1).is_ok()
    })?;
    wait_until("the attempt to have run for at least a second", || {
        fixture
            .run(&["status", "--json"])
            .ok()
            .and_then(|outcome| serde_json::from_str::<serde_json::Value>(&outcome.stdout).ok())
            .and_then(|entries| entries[0]["attempt"]["time_spent_seconds"].as_u64())
            .is_some_and(|seconds| seconds >= 1)
    })?;

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
    let provider_pid: u32 = wait_until_some("the provider to record its process id", || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    })?;
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

#[test]
fn project_named_before_or_after_status_gives_the_same_result() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;

    let before = fixture
        .sandbox
        .run(&elsewhere, &["--project", "my-app", "status"])?;
    let after = fixture
        .sandbox
        .run(&elsewhere, &["status", "--project", "my-app"])?;

    assert_eq!(before.stdout, after.stdout);
    assert_eq!(before.stderr, after.stderr);
    assert_eq!(before.code, after.code);
    assert!(before.stdout.contains("#1\tdone\ta"), "{}", before.stdout);
    Ok(())
}

#[test]
fn project_named_twice_with_different_values_on_status_exits_two() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;
    assert_eq!(
        fixture.sandbox.run(&elsewhere, &["project", "show"])?.code,
        Some(0)
    );

    let outcome = fixture.sandbox.run(
        &elsewhere,
        &["--project", "my-app", "status", "--project", "elsewhere"],
    )?;

    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("\"my-app\""), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("\"elsewhere\""),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}
