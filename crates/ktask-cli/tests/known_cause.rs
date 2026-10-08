//! M4-09: a failure the tool can explain itself never costs the task an attempt. Before the
//! resolver ever runs, four known causes are checked mechanically: a program not found (exit
//! code 127), the disk or its file slots full, a missing git identity (`commit.rs`'s own
//! test), and an unreachable remote (`push.rs`'s own test). Each one stops the run, tells the
//! operator what, why and the exact fix, and leaves the task `pending` rather than `failed` —
//! so no resolver ever runs over it and the attempt it happened on is never weighed against
//! `max-attempts`.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

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

/// A bash block that, for the implementation step, runs `trigger`; the review and test steps,
/// when reached, approve and accept, exactly as every other fixture's own body does.
fn triggering_body(trigger: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  {trigger}\nfi\n```\n"
    )
}

/// A sandbox with a git repository called `my-app`, `max-attempts` set to `3` so a later real
/// failure still gets its own fair attempts even though an earlier one was a known cause.
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
        sandbox.run(&repository, &["settings", "set", "max-attempts", "3"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
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

    fn run_the_queue(&self) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)
    }

    fn task_status(&self) -> Result<String> {
        let status = self.run(&["status"])?;
        Ok(status
            .stdout
            .lines()
            .nth(1)
            .unwrap_or_default()
            .split('\t')
            .nth(1)
            .unwrap_or_default()
            .to_owned())
    }
}

#[test]
fn a_program_not_found_exit_code_is_a_known_cause_the_task_stays_pending_over() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &triggering_body("this-program-does-not-exist-anywhere-on-path"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("could not be found"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("PATH"), "{}", outcome.stdout);
    assert_eq!(fixture.task_status()?, "pending");

    let status = fixture.run(&["status"])?;
    assert!(
        !status.stdout.contains("\tresolve\t"),
        "no resolver ran over a known cause: {}",
        status.stdout
    );
    Ok(())
}

#[test]
fn a_disk_full_message_from_the_provider_is_a_known_cause_the_task_stays_pending_over() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &triggering_body("echo \"cannot write: No space left on device\" >&2\n  exit 1"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("the disk is full"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("free up disk space"),
        "{}",
        outcome.stdout
    );
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.contains("routed: stop — disk full"),
        "{}",
        status.stdout
    );
    assert_eq!(fixture.task_status()?, "pending");
    Ok(())
}

#[test]
fn a_file_slots_full_message_from_the_provider_is_a_known_cause_the_task_stays_pending_over()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &triggering_body("echo \"cannot open: Too many open files\" >&2\n  exit 1"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("too many files are open"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("open-file limit"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.task_status()?, "pending");
    Ok(())
}

#[test]
fn after_a_known_cause_a_later_real_failure_still_gets_the_resolvers_full_budget() -> Result<()> {
    // `max-attempts` is 3. The first attempt is a known cause (program not found); fixing it
    // — simulated here by `touch`ing `ready` — and running again must still give the task
    // three real attempts, not two — the known cause's own attempt is never weighed against
    // the budget.
    let fixture = Fixture::new()?;
    let ready = fixture.repository.join("ready");
    let tries = fixture.repository.join("tries");
    let body = format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry\nelse\n  if [ ! -f \"{ready}\" ]; then\n    exec this-program-does-not-exist-anywhere-on-path\n  fi\n  n=$(cat \"{tries}\" 2>/dev/null || echo 0)\n  echo $((n + 1)) > \"{tries}\"\n  ktask-rs report --token \"$1\" failed --reason \"try $((n + 1))\"\nfi\n```\n",
        ready = ready.display(),
        tries = tries.display(),
    );
    fixture.add_agent_task("a", &body)?;

    // Attempt 1: known cause, task stays pending, no try recorded.
    let first = fixture.run_the_queue()?;
    assert_eq!(first.code, Some(1), "{}", first.stderr);
    assert_eq!(fixture.task_status()?, "pending");
    assert!(!tries.exists());

    // Fixing the missing program and running again now really tries, and really fails, three
    // times — the resolver retrying in between — before the task is finally left failed with
    // the third try's own reason.
    std::fs::write(&ready, "")?;
    let second = fixture.run_the_queue()?;
    assert_eq!(second.code, Some(1), "{}", second.stderr);
    assert_eq!(fixture.task_status()?, "failed");
    assert_eq!(std::fs::read_to_string(&tries)?.trim(), "3");
    assert!(second.stdout.contains("try 3"), "{}", second.stdout);
    Ok(())
}
