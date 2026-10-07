//! M4-06 on the real binary: the resolver's `retry --reset-tree` returns the working tree to
//! the commit the failed attempt started from before the task's next attempt begins, so a mess
//! one attempt made never poisons the next.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A sandbox with a git repository called `my-app`, `max-attempts` left at its default of 3,
/// one commit already made — `README`, holding `first`, and `untouched.txt`, holding `keep` —
/// so the first real attempt has history to call its own start, and a file that is never
/// touched by the task at all, to prove a reset never reaches past what the task itself
/// changed.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
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
        let fixture = Self {
            sandbox,
            repository,
            _keep: keep,
        };
        fixture.git(&["config", "user.email", "test@example.com"])?;
        fixture.git(&["config", "user.name", "Test User"])?;
        std::fs::write(fixture.repository.join("README"), "first\n")?;
        std::fs::write(fixture.repository.join("untouched.txt"), "keep\n")?;
        fixture.git(&["add", "."])?;
        fixture.git(&["commit", "--quiet", "-m", "first"])?;
        Ok(fixture)
    }

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` inside the repository.
    fn run_the_queue(&self) -> Result<Outcome> {
        self.run(&["run"])
    }

    /// Runs `git` with `args` inside the repository, asserting it succeeded.
    fn git(&self, args: &[&str]) -> Result<()> {
        let mut command = std::process::Command::new("git");
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
        Ok(())
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

    /// `ktask-rs status`'s own stdout lines.
    fn status_lines(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status.stdout.lines().map(str::to_owned).collect())
    }

    fn read(&self, name: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.repository.join(name))?)
    }

    fn exists(&self, name: &str) -> bool {
        self.repository.join(name).exists()
    }
}

/// A bash block for attempt 1 of a task: its implementation step creates `mess.txt`, an
/// untracked file, and commits a change to `README` under its own identity, then fails; the
/// review and test steps, when reached, approve and accept; the resolve step reports `retry`,
/// with `--reset-tree` appended when `reset` is `true`; attempt 2's implementation step just
/// reports `done`.
fn body(reset: bool) -> String {
    let flag = if reset { " --reset-tree" } else { "" };
    format!(
        "```bash\n\
         if [ \"$3\" = \"review\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" approved\n\
         elif [ \"$3\" = \"testing\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" accepted\n\
         elif [ \"$3\" = \"resolve\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" retry{flag}\n\
         elif [ \"$2\" = \"1\" ]; then\n\
         \x20\x20echo mess > mess.txt\n\
         \x20\x20echo changed > README\n\
         \x20\x20git add README\n\
         \x20\x20git -c user.name=t -c user.email=t@t commit -q -m wip\n\
         \x20\x20ktask-rs report --token \"$1\" failed --reason \"it broke\"\n\
         else\n\
         \x20\x20ktask-rs report --token \"$1\" done\n\
         fi\n\
         ```\n"
    )
}

#[test]
fn with_reset_tree_the_failed_attempts_changes_are_gone_before_the_next_attempt_starts()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body(true))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    // The untracked file the failed attempt created is gone, and the committed change it made
    // to `README` was undone — the tree is back to exactly the commit the task started from.
    assert!(!fixture.exists("mess.txt"));
    assert_eq!(fixture.read("README")?, "first\n");
    // A file committed before the task ever started, and never touched by it, is untouched.
    assert_eq!(fixture.read("untouched.txt")?, "keep\n");
    // The resolution line says the tree was reset.
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tretry\tthe working tree was reset to the commit \
             this attempt started from\tusage none",
            "\tattempt 2: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 2: review\techo\t0s\tapproved\tusage none",
            "\tattempt 2: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 2: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn without_reset_tree_the_failed_attempts_changes_are_still_there() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &body(false))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    // Nothing was reset: both the untracked file and the committed change survive into the
    // next attempt.
    assert!(fixture.exists("mess.txt"));
    assert_eq!(fixture.read("mess.txt")?, "mess\n");
    assert_eq!(fixture.read("README")?, "changed\n");
    assert_eq!(fixture.read("untouched.txt")?, "keep\n");
    // The resolution line carries no reason: nothing was reset to say anything about.
    assert_eq!(
        fixture.status_lines()?[2],
        "\tattempt 1: resolve\techo\t0s\tretry\tusage none"
    );
    Ok(())
}

#[test]
fn reset_tree_and_its_flag_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["report", "--help"])?;
    assert!(help.stdout.contains("--reset-tree"), "{}", help.stdout);
    Ok(())
}
