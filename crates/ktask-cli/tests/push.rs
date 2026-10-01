//! The push step on the real binary: once the commit step has made a commit, and the project
//! tracks a branch, the tool pushes it there and confirms the remote branch's tip is now that
//! commit — a local bare repository standing in for the remote. A push the remote refuses
//! because it has moved on, or that cannot reach the remote, ends the task `failed`, leaves
//! the commit sitting in the project, and tells the operator what is expected. A task with
//! nothing to commit makes no push and leaves no line.

#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;
#[path = "support/tracked_branch.rs"]
mod tracked_branch;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use support::{Outcome, Result, Sandbox};
use tracked_branch::cloned_repository;

/// A scratch directory, canonical so that it can be compared with what the binary prints.
fn scratch() -> Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    Ok((dir, path))
}

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

/// Runs `git` with `args` inside `dir`, in `sandbox`'s isolation, and asserts it succeeded,
/// returning its standard output.
fn git(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args);
    let output = sandbox.isolate(&mut command, dir).output()?;
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// A sandbox with `repository` cloned from a local bare repository at `bare`, checked out on
/// `main` and tracking it as `origin`, and `seed`, the repository `bare` was itself cloned
/// from — so a test can land a new commit on the tracked branch, from someone else, by
/// committing it in `seed` and pushing.
struct Fixture {
    sandbox: Sandbox,
    scratch: PathBuf,
    repository: PathBuf,
    seed: PathBuf,
    bare: PathBuf,
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
        let bare = scratch.join("my-app.git");
        Ok(Self {
            sandbox,
            scratch,
            repository,
            seed,
            bare,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` with the directory of the `ktask-rs` under test on `PATH`, so a
    /// task's own bash block can call it back with `ktask-rs report`.
    fn run_the_queue(&self) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)
    }

    /// Like [`Fixture::run_the_queue`], without waiting for it: the caller drives it itself.
    fn spawn_the_queue(&self) -> Result<std::process::Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
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

    /// The bare repository's `main`, full hash — the remote's own tip, read live rather than
    /// from any locally cached ref.
    fn remote_tip(&self) -> Result<String> {
        let listing = git(
            &self.sandbox,
            &self.repository,
            &[
                "ls-remote",
                self.bare.to_str().ok_or("not text")?,
                "refs/heads/main",
            ],
        )?;
        Ok(listing
            .split_whitespace()
            .next()
            .ok_or("ls-remote printed nothing")?
            .to_owned())
    }

    /// `repository`'s own `HEAD`, full hash.
    fn head(&self) -> Result<String> {
        Ok(
            git(&self.sandbox, &self.repository, &["rev-parse", "HEAD"])?
                .trim()
                .to_owned(),
        )
    }

    /// Commits a new file named `name` in `seed` and pushes it to `bare`'s `main` — a commit
    /// landing on the tracked branch from someone else.
    fn push_new_commit_from_seed(&self, name: &str) -> Result<()> {
        std::fs::write(self.seed.join(name), "content\n")?;
        git(&self.sandbox, &self.seed, &["add", "."])?;
        git(
            &self.sandbox,
            &self.seed,
            &["commit", "--quiet", "-m", name],
        )?;
        git(
            &self.sandbox,
            &self.seed,
            &["push", "--quiet", "origin", "main"],
        )?;
        Ok(())
    }

    /// Points `repository`'s `origin` at a path that names no repository at all, so any push
    /// to it fails to reach a remote rather than being refused by one.
    fn break_the_remote(&self) -> Result<()> {
        git(
            &self.sandbox,
            &self.repository,
            &[
                "remote",
                "set-url",
                "origin",
                self.scratch
                    .join("no-such-remote")
                    .to_str()
                    .ok_or("path is not text")?,
            ],
        )?;
        Ok(())
    }
}

/// A bash block: for the review and test steps, approves and accepts at once; for the
/// implementation step, blocks on a fifo at `go` until this test writes to it, then writes
/// `new.txt` and reports `done`.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  echo fresh > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
}

#[test]
fn a_pushed_commit_lands_on_the_remote_its_line_says_so_and_the_task_ends_done() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  echo fresh > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");

    let hash = fixture.head()?;
    let remote_tip = fixture.remote_tip()?;
    assert_eq!(remote_tip, hash, "the remote's tip is the task's commit");

    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    let lines: Vec<&str> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tdone\ta");
    let short = &hash[..7];
    assert_eq!(
        lines,
        [
            "#1\tdone\ta".to_owned(),
            "\tsync\t-\t0s\tpassed\tnothing new".to_owned(),
            "\timplementation\techo\t0s\tdone".to_owned(),
            "\treview\techo\t0s\tapproved".to_owned(),
            "\ttesting\techo\t0s\taccepted".to_owned(),
            format!("\tcommit\t-\t0s\tpassed\tcommitted as {short}"),
            format!("\tpush\t-\t0s\tpassed\tpushed {short} to origin/main"),
        ]
    );
    Ok(())
}

#[test]
fn a_push_rejected_because_the_remote_moved_on_ends_the_task_failed_and_the_commit_stays_local()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    let go = fixture.scratch.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;

    let mut child = fixture.spawn_the_queue()?;
    // Wait until the sync step has passed — proving the remote was reachable and unchanged
    // then — before anyone else's work lands on it.
    let lines = fixture.wait_for_status_lines(2)?;
    assert_eq!(lines[1], "\tsync\t-\t0s\tpassed\tnothing new");

    fixture.push_new_commit_from_seed("upstream.txt")?;
    std::fs::write(&go, "")?;

    let status = child.wait()?;
    assert!(!status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "failed");

    let stdout = fixture.run(&["status"])?;
    let lines: Vec<&str> = stdout.stdout.lines().collect();
    let push_line = lines
        .iter()
        .find(|line| line.starts_with("\tpush\t"))
        .expect("a push line");
    assert!(push_line.contains("has moved on"), "{push_line}");
    assert!(push_line.contains("run again"), "{push_line}");

    // The task's own commit is sitting in the project, on top of `HEAD`, never reaching the
    // remote: the remote's tip is still the commit `push_new_commit_from_seed` landed.
    let head = fixture.head()?;
    let remote_tip = fixture.remote_tip()?;
    assert_ne!(head, remote_tip);
    let subject = git(
        &fixture.sandbox,
        &fixture.repository,
        &["log", "-1", "--format=%s"],
    )?;
    assert_eq!(subject.trim(), "a");
    Ok(())
}

#[test]
fn a_push_that_cannot_reach_the_remote_ends_the_task_failed_and_the_commit_stays_local()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    let go = fixture.scratch.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;

    let mut child = fixture.spawn_the_queue()?;
    let lines = fixture.wait_for_status_lines(2)?;
    assert_eq!(lines[1], "\tsync\t-\t0s\tpassed\tnothing new");

    fixture.break_the_remote()?;
    std::fs::write(&go, "")?;

    let status = child.wait()?;
    assert!(!status.success(), "{status:?}");
    assert_eq!(fixture.task_status(1)?, "failed");

    let stdout = fixture.run(&["status"])?;
    let lines: Vec<&str> = stdout.stdout.lines().collect();
    let push_line = lines
        .iter()
        .find(|line| line.starts_with("\tpush\t"))
        .expect("a push line");
    assert!(push_line.contains("git push"), "{push_line}");

    // The commit is sitting in the project, never having reached anywhere.
    let subject = git(
        &fixture.sandbox,
        &fixture.repository,
        &["log", "-1", "--format=%s"],
    )?;
    assert_eq!(subject.trim(), "a");
    Ok(())
}

#[test]
fn a_task_with_nothing_to_commit_has_no_push_line_and_ends_done() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.track_origin_main()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    let status = fixture.run(&["status"])?;
    assert_eq!(
        status.stdout.lines().collect::<Vec<_>>(),
        [
            "#1\tdone\ta",
            "\tsync\t-\t0s\tpassed\tnothing new",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    assert!(
        !status.stdout.contains("push"),
        "no push line: {}",
        status.stdout
    );
    Ok(())
}
