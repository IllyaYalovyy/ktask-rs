//! M4-09: the echo provider's own fixed limit line makes the task wait until the reset it
//! names, showing the countdown in `status` while it waits, then runs the very same attempt
//! again rather than starting a new one.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Result, Sandbox};

const TIMEOUT: Duration = Duration::from_secs(10);

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
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However this test leaves its `run`, nothing of it survives the test itself.
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
            repository,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<support::Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempt to
    /// commit the task's own change is not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        let mut email = Command::new("git");
        email.args(["config", "user.email", "test@example.com"]);
        self.sandbox
            .isolate(&mut email, &self.repository)
            .status()?;
        let mut name = Command::new("git");
        name.args(["config", "user.name", "Test User"]);
        self.sandbox.isolate(&mut name, &self.repository).status()?;
        Ok(())
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

    /// Starts `ktask-rs run` in the background, its `PATH` carrying the directory of the
    /// `ktask-rs` under test, piping its output so the test can read it back once it ends.
    fn spawn_the_queue(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        with_nested_ktask_rs_on_path(&mut command);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command.spawn()?)
    }

    /// Polls `ktask-rs status` until a complete line of its text output satisfies
    /// `condition`, or fails after [`TIMEOUT`].
    fn wait_for_status(&self, what: &str, condition: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = self.run(&["status"])?;
            if condition(&status.stdout) {
                return Ok(status.stdout);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {what}; status shows:\n{}",
                    status.stdout
                )
                .into());
            }
            std::thread::park_timeout(Duration::from_millis(50));
        }
    }
}

#[test]
fn a_limit_message_waits_for_its_reset_then_runs_the_same_attempt_again() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    let tries = fixture.repository.join("tries");
    let body = format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  n=$(cat \"{tries}\" 2>/dev/null || echo 0)\n  echo $((n + 1)) > \"{tries}\"\n  if [ \"$n\" = \"0\" ]; then\n    echo \"KTASK_LIMIT: $(( $(date -u +%s) + 2 ))\"\n    exit 1\n  fi\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        tries = tries.display(),
    );
    fixture.add_agent_task("a", &body)?;

    let mut child = fixture.spawn_the_queue()?;

    // The countdown shows up in `status` while the run is waiting out the limit's own reset —
    // the same attempt, number 1, not a second one.
    let waiting = fixture.wait_for_status("the attempt to show waiting", |stdout| {
        stdout.contains("\twaiting\t")
    })?;
    let lines: Vec<&str> = waiting.lines().collect();
    assert_eq!(lines[0], "#1\trunning\ta");
    let step_line = lines
        .iter()
        .find(|line| line.contains("\twaiting\t"))
        .expect("a waiting step line");
    assert!(step_line.starts_with("\timplementation\t"), "{step_line}");
    assert!(step_line.contains("usage limit"), "{step_line}");

    // Waits for the run, started in the background, to finish on its own.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the run did not finish in time");
        std::thread::park_timeout(Duration::from_millis(50));
    }
    let status = child.wait()?;
    assert!(status.success(), "{status:?}");

    // It ran the provider's script exactly twice: the limit, then the success — one attempt,
    // never two.
    assert_eq!(std::fs::read_to_string(&tries)?.trim(), "2");
    let final_status = fixture.run(&["status"])?;
    assert_eq!(
        final_status.stdout.lines().next(),
        Some("#1\tdone\ta"),
        "{}",
        final_status.stdout
    );
    assert_eq!(
        final_status.stdout.matches("attempt").count(),
        0,
        "no earlier attempt is shown: the same attempt 1 just ran twice: {}",
        final_status.stdout
    );
    Ok(())
}
