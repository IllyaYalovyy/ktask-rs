//! `tracked-branch` on the real binary: a task's attempt starts from the latest code of the
//! tracked branch — pulled with rebase, ahead of the health check — using a local bare
//! repository as the remote. New commits are taken in and recorded as the first line; nothing
//! new says so; uncommitted changes, an unreachable remote, and a rebase conflict each stop
//! the run before any attempt begins, leaving the project's directory untouched and telling
//! the operator what is expected.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;
#[path = "support/tracked_branch.rs"]
mod tracked_branch;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tracked_branch::cloned_repository;

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

/// Runs `git` with `args` inside `dir`, in `sandbox`'s isolation, and asserts it succeeded.
fn git(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command.args(args);
    let status = sandbox.isolate(&mut command, dir).status()?;
    assert!(status.success(), "git {args:?} in {}", dir.display());
    Ok(())
}

/// Commits a new file named `name` in `seed` (as [`cloned_repository`] left it) and pushes it
/// to `origin`'s `main` — a new commit landing on the tracked branch, from someone else.
fn push_new_commit(sandbox: &Sandbox, seed: &Path, name: &str) -> Result<()> {
    std::fs::write(seed.join(name), "content\n")?;
    git(sandbox, seed, &["add", "."])?;
    git(sandbox, seed, &["commit", "--quiet", "-m", name])?;
    git(sandbox, seed, &["push", "--quiet", "origin", "main"])?;
    Ok(())
}

/// A sandbox with `repository` cloned from a local bare repository at `origin`, checked out on
/// `main`, and `seed`, the repository `origin` was itself cloned from — so a test can push a
/// new commit to `origin`'s `main` by committing it in `seed` and pushing.
struct Fixture {
    sandbox: Sandbox,
    scratch: PathBuf,
    repository: PathBuf,
    seed: PathBuf,
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
        let (keep, scratch) = scratch()?;
        let repository = cloned_repository(&sandbox, &scratch, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        let seed = scratch.join("my-app-seed");
        Ok(Self {
            sandbox,
            scratch,
            repository,
            seed,
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

    /// Like [`Fixture::run_the_queue`], without waiting for it: the caller drives it itself.
    fn spawn_the_queue(&self, args: &[&str]) -> Result<std::process::Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
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

    /// Sets the project's tracked branch to `origin/main`.
    fn track_origin_main(&self) -> Result<()> {
        let set = self.run(&["settings", "set", "tracked-branch", "origin/main"])?;
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

    /// `status`'s text lines, once there are at least `count` of them and the `count`th has
    /// ended — a step still `running` has not yet said what it did; fails after 10s.
    fn wait_for_status_lines(&self, count: usize) -> Result<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let outcome = self.run(&["status"])?;
            let lines: Vec<String> = outcome.stdout.lines().map(str::to_owned).collect();
            assert!(
                !lines.iter().any(|line| line.contains("\tinterrupted\t")),
                "a run that is alive was shown interrupted: {lines:?}"
            );
            if lines
                .get(count - 1)
                .is_some_and(|last| !last.ends_with("\trunning"))
            {
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

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

#[test]
fn new_commits_are_taken_in_and_held_in_the_directory_before_anything_else_runs() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    push_new_commit(&fixture.sandbox, &fixture.seed, "upstream.txt")?;
    let go = fixture.scratch.join("go");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            go.display()
        ),
    )?;

    let mut child = fixture.spawn_the_queue(&["run"])?;
    let lines = fixture.wait_for_status_lines(2)?;

    assert_eq!(lines[0], "#1\trunning\ta\tusage none");
    assert_eq!(
        lines[1],
        "\tattempt 1: sync\t-\t0s\tpassed\ttook in 1 commit from origin/main"
    );
    // The new commit's file is already in the working tree, before the task's own step ever
    // started.
    assert!(fixture.repository.join("upstream.txt").is_file());

    std::fs::write(&go, "")?;
    let status = child.wait()?;
    assert!(status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn several_new_commits_are_counted_and_pluralised() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    push_new_commit(&fixture.sandbox, &fixture.seed, "one.txt")?;
    push_new_commit(&fixture.sandbox, &fixture.seed, "two.txt")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let run = fixture.run_the_queue(&["run"])?;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(
        lines[0..2],
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: sync\t-\t0s\tpassed\ttook in 2 commits from origin/main",
        ]
    );
    assert_eq!(
        lines[2],
        "\tattempt 1: implementation\techo\t0s\tdone\tusage none"
    );
    assert_eq!(
        lines[3],
        "\tattempt 1: review\techo\t0s\tapproved\tusage none"
    );
    assert_eq!(
        lines[4],
        "\tattempt 1: testing\techo\t0s\taccepted\tusage none"
    );
    assert_eq!(
        lines[5],
        "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed"
    );
    assert!(fixture.repository.join("one.txt").is_file());
    assert!(fixture.repository.join("two.txt").is_file());
    Ok(())
}

#[test]
fn nothing_new_says_so_and_the_task_carries_on() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let run = fixture.run_the_queue(&["run"])?;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    assert_eq!(
        status.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: sync\t-\t0s\tpassed\tnothing new",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn no_tracked_branch_set_skips_the_step_and_leaves_no_line() -> Result<()> {
    // No remote at all is configured — proving the sync step is not merely skipped for lack
    // of anything new, but never touches git in the first place when there is no setting.
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let added = sandbox.run(
        &repository,
        &[
            "add",
            "--title",
            "a",
            "--criterion",
            "it works",
            "--body",
            &reporting_body("done"),
        ],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let run = sandbox.run_with(&repository, &["run"], with_nested_ktask_rs_on_path)?;

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = sandbox.run(&repository, &["status"])?;
    assert_eq!(
        status.stdout.lines().collect::<Vec<_>>(),
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
fn uncommitted_changes_stop_the_run_before_the_task_starts_and_say_what_is_expected() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    std::fs::write(fixture.repository.join("README"), "changed locally\n")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("uncommitted changes"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("README"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("commit or stash"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.task_status(1)?, "pending");

    // The stop is not lost once the run's own terminal is gone: `status` shows the same
    // words, against the task's still-pending status.
    let status = fixture.run(&["status"])?;
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tpending\ta\tusage none");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("\tsync\t-\t0s\tfailed\t"),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("uncommitted changes"), "{}", lines[1]);
    assert!(lines[1].contains("README"), "{}", lines[1]);
    assert!(lines[1].contains("commit or stash"), "{}", lines[1]);
    Ok(())
}

#[test]
fn once_a_later_run_gets_past_the_sync_the_earlier_stop_is_no_longer_current() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    std::fs::write(fixture.repository.join("README"), "changed locally\n")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    let status = fixture.run(&["status"])?;
    assert!(status.stdout.contains("sync"), "{}", status.stdout);

    // The uncommitted change is committed: the sync now has nothing to refuse over.
    git(&fixture.sandbox, &fixture.repository, &["add", "."])?;
    git(
        &fixture.sandbox,
        &fixture.repository,
        &["commit", "--quiet", "-m", "local change"],
    )?;
    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");

    // The earlier stop is no longer shown as current: the passing sync step replaces it.
    let status = fixture.run(&["status"])?;
    assert!(!status.stdout.contains("failed"), "{}", status.stdout);
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tdone\ta\tusage none");
    assert!(
        lines[1].starts_with("\tattempt 1: sync\t-\t")
            && lines[1].ends_with("\tpassed\tnothing new"),
        "{}",
        lines[1]
    );
    Ok(())
}

#[test]
fn an_unreachable_remote_stops_the_run_and_says_what_is_expected() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    // Breaks the remote after the setting was validated against it: exactly the run-time
    // failure the task distinguishes from a value that was never valid.
    git(
        &fixture.sandbox,
        &fixture.repository,
        &[
            "remote",
            "set-url",
            "origin",
            fixture
                .scratch
                .join("no-such-remote")
                .to_str()
                .ok_or("path is not text")?,
        ],
    )?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("could not be reached"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("was not started"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.task_status(1)?, "pending");

    let status = fixture.run(&["status"])?;
    let lines: Vec<_> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tpending\ta\tusage none");
    assert!(
        lines[1].starts_with("\tsync\t-\t0s\tfailed\t"),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("could not be reached"), "{}", lines[1]);
    assert!(
        lines[1].contains("make the remote reachable"),
        "{}",
        lines[1]
    );
    Ok(())
}

#[test]
fn a_rebase_conflict_is_undone_leaving_the_directory_exactly_as_it_was() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    // A local commit that never reached the remote, changing the same file the remote also
    // changes below — the two cannot both apply without a person's judgement.
    std::fs::write(fixture.repository.join("README"), "local change\n")?;
    git(&fixture.sandbox, &fixture.repository, &["add", "."])?;
    git(
        &fixture.sandbox,
        &fixture.repository,
        &["commit", "--quiet", "-m", "local change"],
    )?;
    std::fs::write(fixture.seed.join("README"), "remote change\n")?;
    git(&fixture.sandbox, &fixture.seed, &["add", "."])?;
    git(
        &fixture.sandbox,
        &fixture.seed,
        &["commit", "--quiet", "-m", "remote change"],
    )?;
    git(
        &fixture.sandbox,
        &fixture.seed,
        &["push", "--quiet", "origin", "main"],
    )?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("conflicted in"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("README"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("undone"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("resolve the conflict yourself"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.task_status(1)?, "pending");

    let ktask_status = fixture.run(&["status"])?;
    let lines: Vec<_> = ktask_status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tpending\ta\tusage none");
    assert!(
        lines[1].starts_with("\tsync\t-\t0s\tfailed\t"),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("conflicted in"), "{}", lines[1]);
    assert!(lines[1].contains("README"), "{}", lines[1]);
    assert!(
        lines[1].contains("resolve the conflict yourself"),
        "{}",
        lines[1]
    );

    // The directory is exactly as the local commit left it: no rebase left in progress, no
    // conflict markers, and the same content and history as before the run was attempted.
    let content = std::fs::read_to_string(fixture.repository.join("README"))?;
    assert_eq!(content, "local change\n");
    let mut status_command = Command::new("git");
    status_command.args(["status", "--porcelain"]);
    let status = fixture
        .sandbox
        .isolate(&mut status_command, &fixture.repository)
        .output()?;
    assert!(status.stdout.is_empty(), "{:?}", status.stdout);
    Ok(())
}
