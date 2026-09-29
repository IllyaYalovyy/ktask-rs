//! `ktask-rs remove` and `ktask-rs list --all` on the real binary, against the journal it
//! keeps.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use serde_json::{Value, json};
use support::{Outcome, Result, Sandbox};

/// A sandbox with a git repository called `my-app` in a scratch directory, whose queue
/// holds `a`, `b` and `c`, numbered 1, 2 and 3.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// A bash block that waits for the file at `go` to exist, then reports `done` (or, for the
/// review step, `approved`; for the test step, `accepted`): an attempt that stays running
/// until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
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

impl Fixture {
    fn new() -> Result<Self> {
        let fixture = Self::empty()?;
        for title in ["a", "b", "c"] {
            fixture.add(title)?;
        }
        Ok(fixture)
    }

    fn empty() -> Result<Self> {
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

    fn add(&self, title: &str) -> Result<Outcome> {
        let outcome = self.run(&["add", "--title", title, "--criterion", "it works"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome)
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

    /// Spawns `ktask-rs run` in the background and returns at once, so the caller can watch
    /// or interrupt it. The binary under test puts its own directory on the child's `PATH`
    /// itself, so a task's bash block calling back into `ktask-rs report` needs no help.
    fn spawn_run(&self) -> Result<Child> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        Ok(command.spawn()?)
    }

    /// The status column of task `task` in the journal, read directly so the test does not
    /// have to wait on the CLI to observe it.
    fn task_status(&self, task: u64) -> Result<String> {
        let database = rusqlite::Connection::open(self.journal())?;
        Ok(database.query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [i64::try_from(task)?],
            |row| row.get(0),
        )?)
    }

    fn journal(&self) -> PathBuf {
        self.sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("journal.db")
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

    /// Asserts that `remove ID` exits 2 naming what is wrong, and that nothing changed.
    fn assert_refused(&self, id: &str, naming: &[&str]) -> Result<()> {
        let before = (self.events()?, self.run(&["list", "--all"])?.stdout);
        let outcome = self.run(&["remove", id])?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        for name in naming {
            assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
        }
        let after = (self.events()?, self.run(&["list", "--all"])?.stdout);
        assert_eq!(after, before, "a refused removal changed something");
        Ok(())
    }
}

/// The lines `list` (or `list --all`) prints, each as `position:#id:status:title`.
fn lines(fixture: &Fixture, all: bool) -> Result<Vec<String>> {
    let args: &[&str] = if all { &["list", "--all"] } else { &["list"] };
    let listed = fixture.run(args)?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    listed
        .stdout
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            let [position, id, status, _kind, title] = fields[..] else {
                return Err(format!("not a list line: {line:?}").into());
            };
            Ok(format!("{position}:{id}:{status}:{title}"))
        })
        .collect()
}

#[test]
fn remove_exits_zero_and_the_task_leaves_list_and_shows_cancelled_in_list_all() -> Result<()> {
    let fixture = Fixture::new()?;

    let removed = fixture.run(&["remove", "2"])?;

    assert_eq!(removed.code, Some(0), "{}", removed.stderr);
    assert_eq!(removed.stdout, "removed task 2\n");
    assert_eq!(removed.stderr, "");
    assert_eq!(
        lines(&fixture, false)?,
        ["1:#1:pending:a", "2:#3:pending:c"]
    );
    assert_eq!(
        lines(&fixture, true)?,
        ["1:#1:pending:a", "2:#2:cancelled:b", "3:#3:pending:c"]
    );
    Ok(())
}

#[test]
fn list_all_json_holds_the_cancelled_task_and_list_json_does_not() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["remove", "2"])?;

    let hidden = fixture.run(&["list", "--json"])?;
    let shown = fixture.run(&["list", "--all", "--json"])?;

    assert_eq!(hidden.code, Some(0), "{}", hidden.stderr);
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    let summary = |outcome: &Outcome| -> Result<Vec<Value>> {
        let tasks: Value = serde_json::from_str(&outcome.stdout)?;
        Ok(tasks
            .as_array()
            .ok_or("not an array")?
            .iter()
            .map(|t| json!([t["position"], t["id"], t["status"], t["title"]]))
            .collect())
    };
    assert_eq!(
        summary(&hidden)?,
        [json!([1, 1, "pending", "a"]), json!([2, 3, "pending", "c"])]
    );
    assert_eq!(
        summary(&shown)?,
        [
            json!([1, 1, "pending", "a"]),
            json!([2, 2, "cancelled", "b"]),
            json!([3, 3, "pending", "c"])
        ]
    );
    // The cancelled task keeps everything it was written with.
    let tasks: Value = serde_json::from_str(&shown.stdout)?;
    assert_eq!(tasks[1]["criteria"], json!(["it works"]));
    assert_eq!(tasks[1]["kind"], "agent");
    Ok(())
}

#[test]
fn list_all_with_nothing_removed_is_the_same_as_list() -> Result<()> {
    let fixture = Fixture::new()?;
    for extra in [&[][..], &["--json"][..]] {
        let mut plain = vec!["list"];
        plain.extend_from_slice(extra);
        let mut all = vec!["list", "--all"];
        all.extend_from_slice(extra);
        assert_eq!(fixture.run(&all)?.stdout, fixture.run(&plain)?.stdout);
    }
    Ok(())
}

#[test]
fn list_all_of_an_empty_queue_prints_nothing_or_an_empty_array() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let text = sandbox.run(&repository, &["list", "--all"])?;
    let json = sandbox.run(&repository, &["list", "--all", "--json"])?;
    assert_eq!((text.stdout.as_str(), text.code), ("", Some(0)));
    assert_eq!((json.stdout.as_str(), json.code), ("[]\n", Some(0)));
    Ok(())
}

#[test]
fn removing_every_task_leaves_list_empty_and_list_all_full() -> Result<()> {
    let fixture = Fixture::new()?;
    for id in ["1", "2", "3"] {
        assert_eq!(fixture.run(&["remove", id])?.code, Some(0));
    }
    assert_eq!(lines(&fixture, false)?, Vec::<String>::new());
    assert_eq!(fixture.run(&["list", "--json"])?.stdout, "[]\n");
    assert_eq!(
        lines(&fixture, true)?,
        ["1:#1:cancelled:a", "2:#2:cancelled:b", "3:#3:cancelled:c"]
    );
    Ok(())
}

#[test]
fn the_task_stays_in_the_journal_and_one_event_records_the_removal() -> Result<()> {
    let fixture = Fixture::new()?;
    assert_eq!(fixture.events()?, (3, "task_added".to_owned()));

    fixture.run(&["remove", "2"])?;

    assert_eq!(fixture.events()?, (4, "task_cancelled".to_owned()));
    let database = rusqlite::Connection::open(fixture.journal())?;
    let (title, status, event_task): (String, String, i64) = database.query_row(
        "SELECT title, status, (SELECT task_id FROM events WHERE kind = 'task_cancelled')
         FROM tasks WHERE id = 2",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(
        (title.as_str(), status.as_str(), event_task),
        ("b", "cancelled", 2)
    );
    Ok(())
}

#[test]
fn an_unknown_id_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.assert_refused("9", &["there is no task 9"])
}

#[test]
fn an_already_cancelled_task_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    assert_eq!(fixture.run(&["remove", "2"])?.code, Some(0));
    fixture.assert_refused("2", &["task 2 is already cancelled"])?;
    // Still exactly one removal recorded, and its neighbours are untouched.
    assert_eq!(fixture.events()?, (4, "task_cancelled".to_owned()));
    assert_eq!(
        lines(&fixture, false)?,
        ["1:#1:pending:a", "2:#3:pending:c"]
    );
    Ok(())
}

#[test]
fn removing_from_an_empty_queue_exits_two_naming_the_id() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let outcome = sandbox.run(&repository, &["remove", "1"])?;
    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("there is no task 1"),
        "{}",
        outcome.stderr
    );
    Ok(())
}

#[test]
fn a_missing_or_malformed_id_exits_two_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let missing = fixture.run(&["remove"])?;
    assert_eq!(missing.code, Some(2), "{}", missing.stderr);
    assert!(missing.stderr.contains("<ID>"), "{}", missing.stderr);
    for id in ["abc", "-1", "1.5", ""] {
        let outcome = fixture.run(&["remove", id])?;
        assert_eq!(outcome.code, Some(2), "{id}: {}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
    }
    assert_eq!(fixture.events()?, (3, "task_added".to_owned()));
    Ok(())
}

#[test]
fn a_removed_tasks_id_is_never_reused() -> Result<()> {
    let fixture = Fixture::new()?;
    // The last task, the case where the highest number could be handed out again.
    fixture.run(&["remove", "3"])?;
    assert_eq!(fixture.add("d")?.stdout, "4\n");
    fixture.run(&["remove", "4"])?;
    fixture.run(&["remove", "1"])?;
    assert_eq!(fixture.add("e")?.stdout, "5\n");
    assert_eq!(
        lines(&fixture, true)?,
        [
            "1:#1:cancelled:a",
            "2:#2:pending:b",
            "3:#3:cancelled:c",
            "4:#4:cancelled:d",
            "5:#5:pending:e"
        ]
    );
    // An import numbers past them too.
    let file = fixture.work.join("more.json");
    std::fs::write(&file, r#"[{"title": "f", "criteria": ["c"]}]"#)?;
    let imported = fixture.run(&["import", &file.to_string_lossy()])?;
    assert_eq!(imported.stdout, "6\n", "{}", imported.stderr);
    Ok(())
}

#[test]
fn a_task_cannot_be_placed_next_to_a_removed_one_but_can_next_to_its_neighbours() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run(&["remove", "2"])?;
    for flag in ["--before", "--after"] {
        let outcome = fixture.run(&["add", "--title", "t", "--criterion", "c", flag, "2"])?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("task 2 is cancelled"),
            "{}",
            outcome.stderr
        );
    }
    fixture.run(&["add", "--title", "n", "--criterion", "c", "--after", "1"])?;
    assert_eq!(
        lines(&fixture, false)?,
        ["1:#1:pending:a", "2:#4:pending:n", "3:#3:pending:c"]
    );
    Ok(())
}

#[test]
fn project_works_on_another_project_from_any_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    let shown = fixture.sandbox.run(&other, &["project", "show"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);

    let removed = fixture
        .sandbox
        .run(&other, &["remove", "1", "--project", "my-app"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);

    assert_eq!(
        lines(&fixture, false)?,
        ["1:#2:pending:b", "2:#3:pending:c"]
    );
    let all = fixture
        .sandbox
        .run(&other, &["list", "--all", "--project", "my-app"])?;
    assert_eq!(all.stdout.lines().count(), 3);
    // The other project has no such task.
    let theirs = fixture.sandbox.run(&other, &["remove", "1"])?;
    assert_eq!(theirs.code, Some(2), "{}", theirs.stderr);
    let unknown = fixture.run(&["remove", "1", "--project", "ghost"])?;
    assert_eq!(unknown.code, Some(2), "{}", unknown.stderr);
    assert!(unknown.stderr.contains("ghost"), "{}", unknown.stderr);
    Ok(())
}

#[test]
fn a_journal_that_is_not_a_database_exits_one_naming_the_file() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = fixture.journal();
    std::fs::write(
        &path,
        "this is not sqlite, and it is long enough to be checked",
    )?;
    for args in [&["remove", "1"][..], &["list", "--all"][..]] {
        let outcome = fixture.run(args)?;
        assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome.stderr.contains(&path.display().to_string()),
            "{}",
            outcome.stderr
        );
    }
    Ok(())
}

#[test]
fn remove_and_all_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["--help"])?;
    assert!(help.stdout.contains("remove"), "{}", help.stdout);
    let remove = fixture.run(&["remove", "--help"])?;
    assert_eq!(remove.code, Some(0));
    for word in ["ID", "--project"] {
        assert!(remove.stdout.contains(word), "{word}: {}", remove.stdout);
    }
    let list = fixture.run(&["list", "--help"])?;
    for word in ["--all", "--json", "--project"] {
        assert!(list.stdout.contains(word), "{word}: {}", list.stdout);
    }
    Ok(())
}

#[test]
fn removing_the_running_task_exits_two_saying_so_and_the_run_finishes_as_if_nothing_was_asked()
-> Result<()> {
    let fixture = Fixture::empty()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("gated", &gated_body(&go))?;
    let mut run = fixture.spawn_run()?;
    // Waits for the implementation step itself to have begun, not merely for the task to show
    // `running` (set as soon as the attempt starts, a moment earlier): the journal keeps
    // gaining events — `attempt_running`, then `step_started` — for a little while after that,
    // and the script's own gate on `go` guarantees nothing more is appended until this test
    // lets it go, so waiting for `step_started` is what actually makes the before/after
    // comparison below race-free.
    wait_until("the implementation step to have begun", || {
        fixture
            .events()
            .is_ok_and(|(_, kind)| kind == "step_started")
    })?;

    fixture.assert_refused("1", &["task 1 is running"])?;

    assert_eq!(fixture.task_status(1)?, "running");
    std::fs::write(&go, "")?;
    let status = run.wait()?;
    assert!(status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}
