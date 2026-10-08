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

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
        self.sandbox.state_dir().join("my-app").join("journal.db")
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
fn a_project_with_no_attempts_prints_only_the_run_band_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run(&["status"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "idle · 1 pending · r to run\n");
    Ok(())
}

#[test]
fn an_empty_queue_shows_an_idle_band_with_nothing_pending_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["status"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "idle · nothing pending\n");
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(parsed["run"]["state"], "idle");
    assert_eq!(parsed["run"]["pending"], 0);
    assert_eq!(parsed["tasks"], serde_json::json!([]));
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
fn status_shows_fresh_output_then_silence_and_the_stuck_warning() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["settings", "set", "silent-after", "1"])?;
    fixture.add_agent_task(
        "watch me",
        "```bash\nif [ \"$3\" = \"implementation\" ]; then\n  for n in 1 2 3 4; do\n    echo working-$n\n    sleep 0.2\n  done\n  sleep 2\n  ktask-rs report --token \"$1\" done\nelif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelse\n  ktask-rs report --token \"$1\" accepted\nfi\n```\n",
    )?;
    let mut run = fixture.spawn_the_queue(&["run"])?;

    wait_until("status to show fresh output", || {
        fixture
            .run(&["status"])
            .is_ok_and(|outcome| outcome.stdout.contains("last output <1s ago"))
    })?;
    let silent = wait_until_some("status to call the silence possibly stuck", || {
        fixture.run(&["status"]).ok().filter(|outcome| {
            outcome.stdout.contains("silent for ") && outcome.stdout.contains("may be stuck")
        })
    })?;
    assert!(silent.stdout.contains("○ silent for "), "{}", silent.stdout);
    assert!(
        silent.stdout.contains("— may be stuck"),
        "{}",
        silent.stdout
    );
    assert!(run.wait()?.success());
    Ok(())
}

/// `stdout`'s lines, with the first — the run band, which carries a timestamp this test
/// cannot predict — checked separately with `band_check`, so the rest can still be compared
/// for exact equality.
fn task_lines(stdout: &str, band_check: impl FnOnce(&str)) -> Vec<&str> {
    let Some((band, rest)) = stdout.split_once('\n') else {
        band_check(stdout);
        return Vec::new();
    };
    band_check(band);
    rest.lines().collect()
}

#[test]
fn status_shows_a_done_ending_in_queue_order_with_its_title_status_and_attempt_line() -> Result<()>
{
    let fixture = done_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        task_lines(&outcome.stdout, |band| assert_eq!(
            band,
            "idle · nothing pending"
        )),
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
fn status_shows_a_failed_ending_in_queue_order_with_its_title_status_and_attempt_line() -> Result<()>
{
    let fixture = failed_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        task_lines(&outcome.stdout, |band| {
            assert!(band.starts_with("run stopped "), "{band}");
            assert!(
                band.ends_with(
                    ": #2 failed (routed: decide — agent failed) — it broke \
                     · next: fix the cause, then t to retry #2"
                ),
                "{band}"
            );
        }),
        [
            "#1\tdone\tx\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
            "#2\tfailed\tb\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\trouted: decide — agent failed\tit broke\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn status_shows_a_too_large_ending_in_queue_order_with_its_title_status_and_attempt_line()
-> Result<()> {
    let fixture = too_large_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        task_lines(&outcome.stdout, |band| {
            assert!(band.starts_with("run stopped "), "{band}");
            assert!(
                band.ends_with(
                    ": #2 failed (routed: decide — agent failed) — split me \
                     · next: fix the cause, then t to retry #2"
                ),
                "{band}"
            );
        }),
        [
            "#1\tdone\tx\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
            "#2\tfailed\tc\tusage none",
            "\tattempt 1: implementation\techo\t0s\ttoo-large\trouted: decide — agent failed\tsplit me\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn status_shows_a_needs_input_ending_in_queue_order_with_its_title_status_and_attempt_line()
-> Result<()> {
    let fixture = needs_input_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        task_lines(&outcome.stdout, |band| {
            assert!(band.starts_with("run stopped "), "{band}");
            assert!(
                band.ends_with(
                    ": #2 blocked — which path? \
                     · next: answer the question, then A to answer #2"
                ),
                "{band}"
            );
        }),
        [
            "#1\tdone\tx\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
            "#2\tblocked\td\tusage none",
            "\tattempt 1: implementation\techo\t0s\tneeds-input\twhich path?\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn status_shows_a_failed_unknown_ending_in_queue_order_with_its_title_status_and_attempt_line()
-> Result<()> {
    let fixture = failed_unknown_fixture()?;
    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let lines = task_lines(&outcome.stdout, |band| {
        assert!(band.starts_with("run stopped "), "{band}");
        assert!(band.contains(": #2 failed-unknown"), "{band}");
        assert!(
            band.ends_with("· next: fix the cause, then t to retry #2"),
            "{band}"
        );
    });
    assert_eq!(lines.len(), 7, "{lines:#?}");
    assert_eq!(lines[0], "#1\tdone\tx\tusage none");
    assert_eq!(
        lines[1],
        "\tattempt 1: implementation\techo\t0s\tdone\tusage none"
    );
    assert_eq!(
        lines[2],
        "\tattempt 1: review\techo\t0s\tapproved\tusage none"
    );
    assert_eq!(
        lines[3],
        "\tattempt 1: testing\techo\t0s\taccepted\tusage none"
    );
    assert_eq!(
        lines[4],
        "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed"
    );
    assert_eq!(lines[5], "#2\tfailed-unknown\te\tusage none");
    assert!(
        lines[6].starts_with("\tattempt 1: implementation\techo\t0s\tfailed-unknown\t"),
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
    /// `tasks["id"/"status"/"title"/"attempt"."outcome"/"attempt"."reason"]`, in order, for
    /// every entry of `status --json`'s `tasks` array.
    fn shown(stdout: &str) -> Result<Vec<ShownEntry>> {
        let parsed: serde_json::Value = serde_json::from_str(stdout)?;
        Ok(parsed["tasks"]
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

    let parsed: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    for entry in parsed["tasks"].as_array().unwrap() {
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
    assert_eq!(outcome.stdout.lines().count(), 11);
    Ok(())
}

#[test]
fn while_a_run_is_in_progress_the_running_task_shows_its_elapsed_time_so_far() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
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
            .and_then(|parsed| parsed["tasks"][0]["attempt"]["time_spent_seconds"].as_u64())
            .is_some_and(|seconds| seconds >= 1)
    })?;

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert!(
        lines[0].starts_with("running: #1 implementation"),
        "{lines:#?}"
    );
    assert_eq!(lines[1], "#1\trunning\ta\tusage none");
    assert_eq!(lines.len(), 3, "{lines:#?}");
    let fields: Vec<&str> = lines[2].split('\t').collect();
    assert_eq!(fields.len(), 7, "{}", lines[2]);
    assert_eq!(
        &fields[..3],
        ["", "attempt 1: implementation", "echo"],
        "{}",
        lines[2]
    );
    assert_eq!(fields[4], "running");
    assert_eq!(fields[5], "usage none");
    assert!(fields[6].contains("silent for "), "{}", lines[2]);
    let seconds: u64 = fields[3].strip_suffix('s').unwrap().parse().unwrap();
    assert!((1..10).contains(&seconds), "{seconds}");

    let json = fixture.run(&["status", "--json"])?;
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let seconds = parsed["tasks"][0]["attempt"]["time_spent_seconds"]
        .as_u64()
        .unwrap();
    assert!((1..10).contains(&seconds), "{seconds}");
    assert_eq!(parsed["tasks"][0]["attempt"]["outcome"], "running");
    assert!(parsed["tasks"][0]["attempt"]["reason"].is_null());
    assert_eq!(
        parsed["tasks"][0]["attempt"]["output_activity"]["active"],
        false
    );
    assert_eq!(parsed["run"]["state"], "running");
    assert_eq!(parsed["run"]["task"], 1);

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
    let mut lines = outcome.stdout.lines();
    assert_eq!(
        lines.next(),
        Some(
            "run stopped: #1 interrupted — the run was killed \
             · next: r to run, then t to retry #1"
        )
    );
    assert_eq!(lines.next(), Some("#1\tinterrupted\ta\tusage none"));
    assert!(!outcome.stdout.contains("running"), "{}", outcome.stdout);

    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(parsed["run"]["state"], "stopped");
    assert_eq!(parsed["run"]["cause"], "interrupted");
    assert_eq!(parsed["tasks"][0]["status"], "interrupted");
    assert_eq!(parsed["tasks"][0]["attempt"]["outcome"], "interrupted");

    Ok(())
}

#[test]
fn a_failing_health_check_gate_shows_a_stopped_band_naming_the_gate_and_to_run_again() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.run(&["settings", "set", "health-check", "exit 1"])?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let run = fixture.run_the_queue(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let band = outcome.stdout.lines().next().expect("the run band");
    assert!(band.starts_with("run stopped"), "{band}");
    assert!(band.contains("#1 health check failed"), "{band}");
    assert!(
        band.ends_with("· next: fix the problem, then r to run"),
        "{band}"
    );
    // The band is followed by the task's own line and its synthetic gate-stop step line,
    // exactly as `status` always shows a gate-stopped pending task.
    assert_eq!(outcome.stdout.lines().count(), 3, "{}", outcome.stdout);

    let json = fixture.run(&["status", "--json"])?;
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(parsed["run"]["state"], "stopped");
    assert_eq!(parsed["run"]["cause"], "environment_fault");
    assert_eq!(parsed["run"]["task"], 1);
    assert_eq!(parsed["run"]["step"], "health check");
    Ok(())
}

#[test]
fn a_human_task_at_the_head_shows_a_stopped_band_naming_the_acknowledge_key() -> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.run(&[
        "add",
        "--title",
        "approve the plan",
        "--criterion",
        "approved",
        "--kind",
        "human",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let outcome = fixture.run(&["status"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "run stopped: #1 is a human task · next: H to acknowledge #1, then r to run\n"
    );

    let json = fixture.run(&["status", "--json"])?;
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(parsed["run"]["state"], "stopped");
    assert_eq!(parsed["run"]["cause"], "human_task");
    assert_eq!(parsed["run"]["task"], 1);
    Ok(())
}

/// Installs a provider named `reported` that reports usage for every step it runs: the
/// implementation step fails with usage once, is resolved with `retry` (its own usage too),
/// then the second attempt's implementation step reports `done` with a third usage figure —
/// so the task's recorded history holds one earlier attempt built from two usage-reporting
/// steps, and its current attempt a third. The token for whichever outcome is wanted is
/// pulled from the prompt's own worked examples, exactly as a real agent would read it.
fn install_two_attempt_usage_reporter(fixture: &Fixture) -> Result<()> {
    let script = fixture.sandbox.tmpdir().join("two-attempts-with-usage");
    let marker = fixture.sandbox.tmpdir().join("attempt-one-ran");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             prompt=$(cat)\n\
             if printf '%s\\n' \"$prompt\" | grep -q 'retry \\[--model'; then\n\
             \x20\x20token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* stop --reason \"<why>\"$/p' | head -n 1 | awk '{{print $4}}')\n\
             \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":5,\"cost_usd\":0.50,\"model\":\"m\"}}}}'\n\
             \x20\x20ktask-rs report --token \"$token\" retry\n\
             elif [ -f \"{marker}\" ]; then\n\
             \x20\x20report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\n\
             \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":200,\"output_tokens\":100,\"cost_usd\":20.00,\"model\":\"m\"}}}}'\n\
             \x20\x20eval \"$report\"\n\
             else\n\
             \x20\x20touch \"{marker}\"\n\
             \x20\x20token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1 | awk '{{print $4}}')\n\
             \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":100,\"output_tokens\":50,\"cost_usd\":10.00,\"model\":\"m\"}}}}'\n\
             \x20\x20ktask-rs report --token \"$token\" failed --reason \"it broke\"\n\
             fi\n",
            marker = marker.display()
        ),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let settings = fixture.sandbox.state_dir().join("my-app/settings.toml");
    let definition = format!(
        "\n[providers.reported]\ncommand = \"{script}\"\nparser = \"plain\"\nusage = \"usage\"\n",
        script = script.display()
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(settings)?
        .write_all(definition.as_bytes())?;
    for (name, value) in [("provider", "reported"), ("resolver-provider", "reported")] {
        fixture.run(&["settings", "set", name, value])?;
    }
    Ok(())
}

#[test]
fn a_tasks_total_sums_every_attempts_usage_resolutions_included() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["settings", "set", "max-attempts", "2"])?;
    for (name, value) in [
        ("step-review", "off"),
        ("step-testing", "off"),
        ("step-push", "off"),
        ("step-commit", "off"),
    ] {
        fixture.run(&["settings", "set", name, value])?;
    }
    install_two_attempt_usage_reporter(&fixture)?;
    fixture.add_agent_task("a", "do the recorded work")?;

    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);

    // The task's own line sums every step of every attempt, resolutions included: $10.00
    // (attempt 1's implementation) + $0.50 (its resolution) + $20.00 (attempt 2's
    // implementation) = $30.50 — not attempt 2's $20.00 alone.
    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    let lines: Vec<_> = status.stdout.lines().skip(1).collect();
    assert_eq!(
        lines[0],
        "#1\tdone\ta\ttokens in 305 out 155 cost $30.500000"
    );
    // Each attempt keeps its own subtotal on its first line.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("attempt 1: implementation")
                && line.ends_with("tokens in 100 out 50 cost $10.000000")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("attempt 1: resolve")
            && line.ends_with("tokens in 5 out 5 cost $0.500000")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("attempt 2: implementation")
                && line.ends_with("tokens in 200 out 100 cost $20.000000")),
        "{lines:?}"
    );

    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let task = &parsed["tasks"][0];
    assert_eq!(task["input_tokens"], 305);
    assert_eq!(task["output_tokens"], 155);
    assert_eq!(task["cost_usd"], "30.500000");
    // The current attempt's own fields still carry only its own subtotal.
    assert_eq!(task["attempt"]["input_tokens"], 200);
    assert_eq!(task["attempt"]["output_tokens"], 100);
    assert_eq!(task["attempt"]["cost_usd"], "20.000000");
    // The earlier attempt, in history, carries its own subtotal too: implementation plus the
    // resolution that ran inside it.
    assert_eq!(task["history"][0]["input_tokens"], 105);
    assert_eq!(task["history"][0]["output_tokens"], 55);
    assert_eq!(task["history"][0]["cost_usd"], "10.500000");

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

/// `status --help` must not undersell what `status` shows: a retried task's earlier, failed
/// attempts stay visible under its current one (proved end to end by
/// `a_second_run_after_retry_adds_attempt_two_under_the_first_in_status_and_the_task_builds_on_it`
/// in `tests/retry.rs`) — not just its "most recent attempt", the claim this help text used
/// to make before that history was added.
#[test]
fn status_help_does_not_undersell_the_history_it_keeps() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["status", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    assert!(
        !help.stdout.contains("most recent attempt"),
        "status shows every attempt a retried task had, not only its most recent one: {}",
        help.stdout
    );
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
