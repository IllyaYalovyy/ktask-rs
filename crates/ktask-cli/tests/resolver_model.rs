//! M4-05 on the real binary: the resolver's `retry` accepts `--provider` and `--model`, so a
//! task a cheap model keeps failing can be finished by a stronger one — the next attempt runs
//! with them, and every interface shows what it ran with.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A sandbox with a git repository called `my-app`, `max-attempts` left at its default of 3.
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
/// review and test steps, if ever reached, approve and accept; the resolve step reports
/// `resolve_branch`; attempt 2's implementation step writes the model it was run with (`$6`) to
/// `model_file` and reports done.
fn retry_with_model_body(
    reason: &str,
    resolve_branch: &str,
    model_file: &std::path::Path,
) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  {resolve_branch}\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" failed --reason \"{reason}\"\nelse\n  printf '%s' \"$6\" > \"{model}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        model = model_file.display(),
    )
}

#[test]
fn retry_with_a_model_hands_the_next_attempt_the_model_and_the_provider_runs_with_it() -> Result<()>
{
    let fixture = Fixture::new()?;
    let model_file = fixture.work.join("model-seen");
    fixture.add_agent_task(
        "a",
        &retry_with_model_body(
            "it broke",
            "ktask-rs report --token \"$1\" retry --model other",
            &model_file,
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    // The second attempt's provider command actually received the model as its fourth
    // positional argument — not just a label recorded for display.
    assert_eq!(std::fs::read_to_string(&model_file)?, "other");
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\ta",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke",
            "\tattempt 1: resolve\techo\t0s\tretry",
            "\tattempt 2: implementation\tother\techo\t0s\tdone",
            "\tattempt 2: review\techo\t0s\tapproved",
            "\tattempt 2: testing\techo\t0s\taccepted",
            "\tattempt 2: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn retry_with_a_known_provider_is_accepted_and_the_attempt_still_shows_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let model_file = fixture.work.join("model-seen");
    fixture.add_agent_task(
        "a",
        &retry_with_model_body(
            "it broke",
            "ktask-rs report --token \"$1\" retry --provider echo --model stronger",
            &model_file,
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(std::fs::read_to_string(&model_file)?, "stronger");
    let lines = fixture.status_lines()?;
    assert!(
        lines.contains(&"\tattempt 2: implementation\tstronger\techo\t0s\tdone".to_owned()),
        "{lines:?}"
    );
    Ok(())
}

#[test]
fn retry_with_an_unknown_provider_is_refused_naming_the_known_ones() -> Result<()> {
    let fixture = Fixture::new()?;

    // No attempt needs to exist at all: an unknown `--provider` is refused before the token is
    // even looked up.
    let outcome = fixture.run(&[
        "report",
        "--token",
        "my-app/1/1",
        "retry",
        "--provider",
        "not-a-provider",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stdout);
    assert!(
        outcome
            .stderr
            .contains("unknown provider \"not-a-provider\""),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("echo"), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("claude"), "{}", outcome.stderr);
    Ok(())
}

#[test]
fn provider_and_model_are_refused_outside_the_retry_outcome() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        "my-app/1/1",
        "done",
        "--model",
        "other",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stdout);
    assert!(
        outcome
            .stderr
            .contains("does not accept --provider, --model, --same-session or --reset-tree"),
        "{}",
        outcome.stderr
    );
    Ok(())
}
