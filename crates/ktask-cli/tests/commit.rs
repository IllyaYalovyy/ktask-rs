//! The commit step on the real binary: once a task's implementation, review and test steps
//! have all passed, the tool commits everything the task changed under the user's own git
//! identity — one commit, message built from the task, no trailer of any kind. A task that
//! changed nothing makes no commit and carries on; a task whose commit git itself refuses ends
//! `failed` telling the operator what is expected. A task whose identity is not configured is a
//! known cause instead: the task goes back to `pending` rather than ending `failed`.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

/// `stdout`'s lines after the first — the run band, which this file's tests do not need to
/// check since it is already covered, line by line, in `tests/status.rs`.
fn after_band(stdout: &str) -> Vec<&str> {
    stdout.lines().skip(1).collect()
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

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step — after running `setup`, e.g. writing files — with the review and test
/// steps, when reached, approving and accepting.
fn reporting_body_after(setup: &str, outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  {setup}\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` inside the repository, its `PATH` carrying the directory of the
    /// `ktask-rs` under test, so a task's own bash block can call it back with
    /// `ktask-rs report`.
    fn run_the_queue(&self) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)
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

    /// Runs `git` with `args` inside the repository, in the sandbox's isolation, and asserts
    /// it succeeded, returning its standard output.
    fn git(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new("git");
        command.args(args);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempts to
    /// commit are not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        self.git(&["config", "user.email", "test@example.com"])?;
        self.git(&["config", "user.name", "Test User"])?;
        Ok(())
    }

    /// Commits `name` with `content` under the identity already configured — a commit already
    /// in place before the task's own attempt runs, so its own commit has something to change
    /// rather than only to add.
    fn seed_commit(&self, name: &str, content: &str) -> Result<()> {
        std::fs::write(self.repository.join(name), content)?;
        self.git(&["add", name])?;
        self.git(&["commit", "--quiet", "-m", "seed"])?;
        Ok(())
    }

    /// Installs a `pre-commit` hook that exits 1, printing `message` to standard error — so
    /// any commit the commit step attempts is refused by git itself, not by the tool.
    fn refuse_every_commit(&self, message: &str) -> Result<()> {
        let hooks = self.repository.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks)?;
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\necho '{message}' >&2\nexit 1\n"))?;
        let mut permissions = std::fs::metadata(&hook)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&hook, permissions)?;
        Ok(())
    }

    /// How many commits the repository holds, however few — `0` is a valid answer, unlike
    /// `git rev-list --count HEAD`, which fails outright when there is no `HEAD` yet.
    fn commit_count(&self) -> Result<u64> {
        Ok(self
            .git(&["rev-list", "--all", "--count"])?
            .trim()
            .parse()?)
    }

    /// `HEAD`, short form.
    fn head_short(&self) -> Result<String> {
        Ok(self
            .git(&["rev-parse", "--short", "HEAD"])?
            .trim()
            .to_owned())
    }

    /// The files `HEAD`'s own commit touched.
    fn head_files(&self) -> Result<Vec<String>> {
        Ok(self
            .git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

#[test]
fn a_task_that_changes_and_adds_files_gets_one_commit_holding_them_and_the_line_shows_its_short_hash()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.seed_commit("existing.txt", "before\n")?;
    fixture.add_agent_task(
        "Do the thing",
        &reporting_body_after("echo after > existing.txt\necho fresh > new.txt", "done"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        fixture.commit_count()?,
        2,
        "seed commit plus the task's own"
    );

    let hash = fixture.head_short()?;
    let mut files = fixture.head_files()?;
    files.sort();
    assert_eq!(files, ["existing.txt", "new.txt"]);
    assert_eq!(
        std::fs::read_to_string(fixture.repository.join("existing.txt"))?,
        "after\n"
    );

    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    assert_eq!(
        after_band(&status.stdout),
        [
            "#1\tdone\tDo the thing\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            &format!("\tattempt 1: commit\t-\t0s\tpassed\tcommitted as {hash}"),
        ]
    );

    // The message: the task's title as the subject, its ID and acceptance criteria in the
    // body, and no trailer of any kind.
    let subject = fixture.git(&["log", "-1", "--format=%s"])?;
    assert_eq!(subject.trim(), "Do the thing");
    let body = fixture.git(&["log", "-1", "--format=%b"])?;
    assert!(body.contains("Task #1"), "{body}");
    assert!(body.contains("it works"), "{body}");
    let whole_message = fixture.git(&["log", "-1", "--format=%B"])?;
    let lower = whole_message.to_lowercase();
    assert!(!lower.contains("co-authored-by"), "{whole_message}");
    assert!(!lower.contains("claude"), "{whole_message}");
    assert!(!lower.contains("generated"), "{whole_message}");
    assert!(!lower.contains("anthropic"), "{whole_message}");

    // Author and committer are the identity git is configured with.
    let identity = fixture.git(&["log", "-1", "--format=%an|%ae|%cn|%ce"])?;
    assert_eq!(
        identity.trim(),
        "Test User|test@example.com|Test User|test@example.com"
    );

    Ok(())
}

#[test]
fn a_task_that_changes_nothing_makes_no_commit_the_line_says_so_and_the_task_carries_on()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task("a", &reporting_body_after("true", "done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.commit_count()?, 0, "nothing was ever committed");
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
fn an_unconfigured_git_identity_is_a_known_cause_the_task_stays_pending_over() -> Result<()> {
    // No `configure_git_identity` call, and the sandbox's `HOME` carries no `.gitconfig`
    // either: git has nothing at all to say whose the commit would be.
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_after("echo fresh > new.txt", "done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("git identity is not configured"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("user.name"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("user.email"), "{}", outcome.stdout);

    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    let lines = after_band(&status.stdout);
    assert_eq!(lines[0], "#1\tpending\ta\tusage none");
    assert!(
        lines[4].starts_with("\tattempt 1: commit\t-\t0s\tfailed\t"),
        "{}",
        lines[4]
    );
    assert!(
        lines[4].contains("git identity is not configured"),
        "{}",
        lines[4]
    );
    assert!(
        lines[4].contains("routed: stop — no git identity"),
        "{}",
        lines[4]
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("\tattempt 1: resolve\t")),
        "no resolver ran over a known cause: {lines:?}"
    );

    // Nothing was committed, and the file the task wrote is still sitting uncommitted.
    assert_eq!(fixture.commit_count()?, 0);
    let dirty = fixture.git(&["status", "--porcelain"])?;
    assert!(dirty.contains("new.txt"), "{dirty}");

    // Fixing the identity and running again picks the task straight back up, with no attempt
    // spent on the environment's own problem.
    fixture.configure_git_identity()?;
    let second = fixture.run_the_queue()?;
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(
        fixture.run(&["status"])?.stdout.lines().nth(1),
        Some("#1\tdone\ta\tusage none")
    );
    Ok(())
}

#[test]
fn a_commit_git_itself_refuses_ends_the_task_failed_with_what_git_said() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.refuse_every_commit("no thanks")?;
    fixture.add_agent_task("a", &reporting_body_after("echo fresh > new.txt", "done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(outcome.stdout.contains("git commit"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("exited with code 1"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("no thanks"), "{}", outcome.stdout);

    let status = fixture.run(&["status"])?;
    assert_eq!(status.code, Some(0), "{}", status.stderr);
    let lines = after_band(&status.stdout);
    assert_eq!(lines[0], "#1\tfailed\ta\tusage none");
    assert!(
        lines[4].starts_with("\tattempt 1: commit\t-\t0s\tfailed\t"),
        "{}",
        lines[4]
    );

    // Nothing was committed, and the file the task wrote is still sitting uncommitted.
    assert_eq!(fixture.commit_count()?, 0);
    let dirty = fixture.git(&["status", "--porcelain"])?;
    assert!(dirty.contains("new.txt"), "{dirty}");
    Ok(())
}
