//! Switching a step off or on, on the real binary: a step switched off does not run and has
//! no line, the others run in the usual order; switching `commit` off while `push` is on, and
//! switching `implementation` off, are refused.

#[path = "support/repo.rs"]
mod repo;
mod support;
#[path = "support/tracked_branch.rs"]
mod tracked_branch;

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// A bash block that reports `outcome` for the implementation step and, for whatever step
/// `$3` names next, approves the review step and accepts the testing step — so a task meant
/// to succeed end to end still does, whether or not review or testing are switched off.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block like [`reporting_body`], that also writes a file before reporting `done` for
/// the implementation step — so the commit step has something to commit.
fn reporting_body_with_a_change() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  echo fresh > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        .to_owned()
}

/// A sandbox with a plain git repository called `my-app`: no remote, so the sync and push
/// steps never run unless a test's own fixture adds one.
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

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` with the directory of the `ktask-rs` under test on `PATH`, so a
    /// task's own bash block can call it back with `ktask-rs report`.
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

    /// Switches step `name`'s own setting to `value` (`"on"` or `"off"`).
    fn set_step(&self, name: &str, value: &str) -> Result<()> {
        let set = self.run(&["settings", "set", &format!("step-{name}"), value])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    /// Sets the project's health-check command to `command`.
    fn set_health_check(&self, command: &str) -> Result<()> {
        let set = self.run(&["settings", "set", "health-check", command])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempts to
    /// commit are not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        let mut email = Command::new("git");
        email.args(["config", "user.email", "test@example.com"]);
        assert!(
            self.sandbox
                .isolate(&mut email, &self.repository)
                .status()?
                .success()
        );
        let mut name = Command::new("git");
        name.args(["config", "user.name", "Test User"]);
        assert!(
            self.sandbox
                .isolate(&mut name, &self.repository)
                .status()?
                .success()
        );
        Ok(())
    }

    /// How many commits the repository holds, however few.
    fn commit_count(&self) -> Result<u64> {
        let mut command = Command::new("git");
        command.args(["rev-list", "--all", "--count"]);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(output.status.success());
        Ok(String::from_utf8(output.stdout)?.trim().parse()?)
    }

    /// The steps named on `status`'s lines, in order — the tab-indented lines, their own first
    /// tab-separated field.
    fn step_names(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status
            .stdout
            .lines()
            .filter(|line| line.starts_with('\t'))
            .map(|line| {
                line.trim_start_matches('\t')
                    .split('\t')
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
            .collect())
    }
}

/// A sandbox whose repository is a clone of a local bare repository, tracking it as `origin`
/// and checked out on `main` — so `origin/main` names a real remote branch, and the sync and
/// push steps have somewhere to work against.
struct TrackedFixture {
    sandbox: Sandbox,
    repository: PathBuf,
    bare: PathBuf,
    _keep: tempfile::TempDir,
}

impl TrackedFixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = cloned_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        let bare = work.join("my-app.git");
        Ok(Self {
            sandbox,
            repository,
            bare,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn run_the_queue(&self) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)
    }

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

    fn track_origin_main(&self) -> Result<()> {
        let set = self.run(&["settings", "set", "tracked-branch", "origin/main"])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    fn set_step(&self, name: &str, value: &str) -> Result<()> {
        let set = self.run(&["settings", "set", &format!("step-{name}"), value])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    fn step_names(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status
            .stdout
            .lines()
            .filter(|line| line.starts_with('\t'))
            .map(|line| {
                line.trim_start_matches('\t')
                    .split('\t')
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
            .collect())
    }

    /// The bare repository's `main`, full hash — the remote's own tip, read live.
    fn remote_tip(&self) -> Result<String> {
        let mut command = Command::new("git");
        command.args([
            "ls-remote",
            self.bare.to_str().ok_or("not text")?,
            "refs/heads/main",
        ]);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(output.status.success());
        Ok(String::from_utf8(output.stdout)?
            .split_whitespace()
            .next()
            .ok_or("ls-remote printed nothing")?
            .to_owned())
    }

    fn head(&self) -> Result<String> {
        let mut command = Command::new("git");
        command.args(["rev-parse", "HEAD"]);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(output.status.success());
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}

#[test]
fn switching_sync_off_skips_it_and_the_rest_still_run_in_the_usual_order() -> Result<()> {
    let fixture = TrackedFixture::new()?;
    fixture.track_origin_main()?;
    fixture.set_step("sync", "off")?;
    fixture.add_agent_task("a", &reporting_body_with_a_change())?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(
        names,
        ["implementation", "review", "testing", "commit", "push"]
    );
    let head = fixture.head()?;
    assert_eq!(fixture.remote_tip()?, head, "the push still landed");
    Ok(())
}

#[test]
fn switching_the_health_check_off_skips_it_even_though_one_is_configured() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_health_check("echo checking")?;
    fixture.set_step("health-check", "off")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(names, ["implementation", "review", "testing", "commit"]);
    Ok(())
}

#[test]
fn switching_review_off_skips_it_and_the_rest_still_run_in_the_usual_order() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_step("review", "off")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(names, ["implementation", "testing", "commit"]);
    Ok(())
}

#[test]
fn switching_testing_off_skips_it_and_the_rest_still_run_in_the_usual_order() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_step("testing", "off")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(names, ["implementation", "review", "commit"]);
    Ok(())
}

#[test]
fn switching_commit_off_skips_it_and_leaves_no_line_even_with_something_to_commit() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    // Push defaults on independently of whether a branch is tracked, so switching commit off
    // is refused unless push is switched off first.
    fixture.set_step("push", "off")?;
    fixture.set_step("commit", "off")?;
    fixture.add_agent_task("a", &reporting_body_with_a_change())?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(names, ["implementation", "review", "testing"]);
    assert_eq!(fixture.commit_count()?, 0, "nothing was ever committed");
    Ok(())
}

#[test]
fn switching_push_off_skips_it_even_with_a_tracked_branch_and_a_commit_made() -> Result<()> {
    let fixture = TrackedFixture::new()?;
    fixture.track_origin_main()?;
    fixture.set_step("push", "off")?;
    fixture.add_agent_task("a", &reporting_body_with_a_change())?;
    let before = fixture.remote_tip()?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let names = fixture.step_names()?;
    assert_eq!(
        names,
        ["sync", "implementation", "review", "testing", "commit"]
    );
    assert_eq!(
        fixture.remote_tip()?,
        before,
        "nothing was ever pushed to the remote"
    );
    Ok(())
}

#[test]
fn switching_commit_off_while_push_is_on_is_refused_and_the_run_is_unaffected() -> Result<()> {
    let fixture = TrackedFixture::new()?;
    fixture.track_origin_main()?;

    let outcome = fixture.run(&["settings", "set", "step-commit", "off"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("cannot switch off while push is on"),
        "{}",
        outcome.stderr
    );
    fixture.add_agent_task("a", &reporting_body_with_a_change())?;
    let run = fixture.run_the_queue()?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let names = fixture.step_names()?;
    assert_eq!(
        names,
        [
            "sync",
            "implementation",
            "review",
            "testing",
            "commit",
            "push"
        ],
        "commit and push both still ran: the refusal changed nothing"
    );
    Ok(())
}

#[test]
fn switching_implementation_off_is_refused_and_every_task_still_runs_it() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "step-implementation", "off"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("the implementation step always runs and cannot be switched off"),
        "{}",
        outcome.stderr
    );
    fixture.add_agent_task("a", &reporting_body("done"))?;
    let run = fixture.run_the_queue()?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let names = fixture.step_names()?;
    assert_eq!(names, ["implementation", "review", "testing", "commit"]);
    Ok(())
}
