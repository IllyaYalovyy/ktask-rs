//! M4-04 on the real binary: after an attempt ends `failed` or `failed-unknown`, the tool runs
//! a resolver — an agent in the resolve role — before anything else happens. `retry` starts a
//! fresh attempt; `stop` ends the task `failed` with the resolver's own reason; `skip` ends the
//! task `skipped` with the resolver's own reason and the run goes on to the next task
//! (M4-07); a resolver that reports nothing ends the task `failed-unknown`; `max-attempts`
//! caps how many attempts get a resolver at all; and `ktask-rs report` only accepts `retry`,
//! `stop` or `skip` while the resolve step is the one running, never otherwise.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A sandbox with a git repository called `my-app`, `max-attempts` left at its default of 3
/// unless a test changes it.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: TempDir,
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

    /// Runs `ktask-rs run` inside the repository.
    fn run_the_queue(&self) -> Result<Outcome> {
        self.run(&["run"])
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
}

/// A bash block for attempt 1 of a task: fails the implementation step with `reason`; the
/// review and test steps, if ever reached, approve and accept; the resolve step, when it runs,
/// reports whatever `resolve_branch` says to.
fn failing_once_body(reason: &str, resolve_branch: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  {resolve_branch}\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" failed --reason \"{reason}\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
    )
}

/// A bash block for a task that simply succeeds: the implementation step reports `done` at
/// once, and the review and test steps, if ever reached, approve and accept.
fn succeeding_body() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        .to_owned()
}

#[test]
fn a_retry_decision_starts_a_second_attempt_and_the_resolution_shows_between_the_two() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body("it broke", "ktask-rs report --token \"$1\" retry"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\ta",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke",
            "\tattempt 1: resolve\techo\t0s\tretry",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn a_stop_decision_ends_the_task_failed_with_the_resolvers_own_reason_not_the_attempts()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" stop --reason \"not worth retrying\"",
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert!(
        outcome
            .stdout
            .contains("task 1: failed: not worth retrying"),
        "{}",
        outcome.stdout
    );
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta",
            "\timplementation\techo\t0s\tfailed\tit broke",
            "\tresolve\techo\t0s\tstop\tnot worth retrying",
        ]
    );
    Ok(())
}

#[test]
fn a_skip_decision_ends_the_task_skipped_with_the_resolvers_own_reason_and_the_run_moves_on_to_the_next_task()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" skip --reason \"no longer relevant\"",
        ),
    )?;
    fixture.add_agent_task("b", &succeeding_body())?;

    let outcome = fixture.run_the_queue()?;

    // Unlike `stop`, `skip` does not stop the run: task b is attempted and finishes.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tskipped\ta",
            "\timplementation\techo\t0s\tfailed\tit broke",
            "\tresolve\techo\t0s\tskip\tno longer relevant",
            "#2\tdone\tb",
            "\timplementation\techo\t0s\tdone",
            "\treview\techo\t0s\tapproved",
            "\ttesting\techo\t0s\taccepted",
            "\tcommit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn skip_without_a_reason_is_refused_and_the_task_stays_running_its_attempt() -> Result<()> {
    let fixture = Fixture::new()?;
    let skip_stderr = fixture.work.join("skip-stderr");
    let skip_exit = fixture.work.join("skip-exit");
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" skip 2> \"{}\"; echo $? > \"{}\"\n  \
                 ktask-rs report --token \"$1\" stop --reason \"skip was refused\"",
                skip_stderr.display(),
                skip_exit.display(),
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    let exit: i32 = std::fs::read_to_string(&skip_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&skip_stderr)?;
    assert!(stderr.contains("needs a reason"), "{stderr}");
    // The refused `skip` recorded nothing: the task ended through the `stop` that followed it.
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta",
            "\timplementation\techo\t0s\tfailed\tit broke",
            "\tresolve\techo\t0s\tstop\tskip was refused",
        ]
    );
    Ok(())
}

#[test]
fn list_hides_a_skipped_task_the_same_as_a_cancelled_one_and_all_shows_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" skip --reason \"no longer relevant\"",
        ),
    )?;
    fixture.add_agent_task("b", &succeeding_body())?;
    let outcome = fixture.run_the_queue()?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);

    let hidden = fixture.run(&["list"])?;
    assert_eq!(hidden.code, Some(0), "{}", hidden.stderr);
    assert_eq!(hidden.stdout, "1\t#2\tdone\tagent\tb\n");

    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.code, Some(0), "{}", all.stderr);
    assert_eq!(
        all.stdout,
        "1\t#1\tskipped\tagent\ta\n2\t#2\tdone\tagent\tb\n"
    );
    Ok(())
}

#[test]
fn a_resolver_that_reports_nothing_ends_the_task_failed_unknown_with_what_was_observed()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &failing_once_body("it broke", "true"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains(
            "task 1: failed-unknown: the provider exited with code 0 and reported nothing"
        ),
        "{}",
        outcome.stdout
    );
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed-unknown\ta",
            "\timplementation\techo\t0s\tfailed\tit broke",
            "\tresolve\techo\t0s\tfailed-unknown\tthe provider exited with code 0 and reported nothing",
        ]
    );
    Ok(())
}

#[test]
fn with_max_attempts_1_the_resolver_never_runs_and_the_task_ends_failed_at_once() -> Result<()> {
    let fixture = Fixture::new()?;
    let set = fixture.run(&["settings", "set", "max-attempts", "1"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    // The resolve branch panics the queue (an unknown command) if it is ever reached, so this
    // test fails loudly rather than silently passing were the resolver run anyway.
    fixture.add_agent_task("a", &failing_once_body("it broke", "exit 99"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta",
            "\timplementation\techo\t0s\tfailed\tit broke",
        ]
    );
    Ok(())
}

#[test]
fn with_max_attempts_3_the_third_failure_ends_the_task_without_a_resolution_line() -> Result<()> {
    let fixture = Fixture::new()?;
    // Every attempt fails and the resolver always retries — attempts 1 and 2 each get a
    // resolution, but the third, reaching `max-attempts`, does not.
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke",
            "\tattempt 1: resolve\techo\t0s\tretry",
            "\tattempt 2: implementation\techo\t0s\tfailed\tit broke",
            "\tattempt 2: resolve\techo\t0s\tretry",
            "\timplementation\techo\t0s\tfailed\tit broke",
        ]
    );
    Ok(())
}

#[test]
fn report_refuses_a_decision_outside_the_resolve_step_and_a_non_decision_inside_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let outside_stderr = fixture.work.join("outside-stderr");
    let outside_exit = fixture.work.join("outside-exit");
    let inside_stderr = fixture.work.join("inside-stderr");
    let inside_exit = fixture.work.join("inside-exit");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" done 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" retry\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" retry 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            inside_stderr.display(),
            inside_exit.display(),
            outside_stderr.display(),
            outside_exit.display(),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    // The wrong decisions were refused, but each script went on to report validly afterwards,
    // so the run completes as if they were never tried.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    assert_eq!(fixture.status_lines()?[0], "#1\tdone\ta");

    let exit: i32 = std::fs::read_to_string(&outside_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&outside_stderr)?;
    assert!(
        stderr.contains("does not belong to the implementation step"),
        "{stderr}"
    );
    assert!(stderr.contains("done"), "{stderr}");
    assert!(stderr.contains("needs-input"), "{stderr}");
    assert!(!stderr.contains("retry or stop"), "{stderr}");

    let exit: i32 = std::fs::read_to_string(&inside_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&inside_stderr)?;
    assert!(
        stderr.contains("does not belong to the resolve step"),
        "{stderr}"
    );
    assert!(stderr.contains("expected retry or stop"), "{stderr}");
    Ok(())
}
