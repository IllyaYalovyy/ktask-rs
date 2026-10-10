//! B-58 on the real binary: an agent step that ends with exit 0 and no report is resumed once,
//! in the same session, with a nudge that carries nothing but the exact `report` command — the
//! echo provider implements sessions for real, so this is tested with no model and no network.
//!
//! A reviewer that reports on the nudge continues the attempt exactly as if it had reported
//! first time: no resolve step runs, and the review step's own line says it was nudged. A
//! reviewer that ignores the nudge too goes to the resolve step, which is shown the last 80
//! lines of the nudge's own output.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A sandbox with a git repository called `my-app`.
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

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn run_the_queue(&self) -> Result<Outcome> {
        self.run(&["run"])
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

    /// `status`'s step lines, after the run band — its own first line — which this file's
    /// tests do not need to check since it is already covered, line by line, in
    /// `tests/status.rs`.
    fn status_lines(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status.stdout.lines().skip(1).map(str::to_owned).collect())
    }
}

#[test]
fn a_reviewer_that_reports_on_the_nudge_continues_with_no_resolve_line() -> Result<()> {
    let fixture = Fixture::new()?;
    let nudge_prompt_file = fixture.work.join("nudge-prompt.txt");
    let body = format!(
        "```bash\n\
         if [ \"$3\" = \"implementation\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" done\n\
         elif [ \"$3\" = \"review\" ]; then\n\
         \x20\x20if [ -z \"$4\" ]; then\n\
         \x20\x20\x20\x20echo \"KTASK_SESSION: nudge-sess\"\n\
         \x20\x20else\n\
         \x20\x20\x20\x20cp \"$7\" \"{nudge_prompt_file}\"\n\
         \x20\x20\x20\x20ktask-rs report --token \"$1\" approved\n\
         \x20\x20fi\n\
         elif [ \"$3\" = \"testing\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" accepted\n\
         fi\n\
         ```\n",
        nudge_prompt_file = nudge_prompt_file.display(),
    );
    fixture.add_agent_task("Review nudge", &body)?;

    let outcome = fixture.run_the_queue()?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);

    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\tReview nudge\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\trouted: nudged\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );

    // The nudge's own prompt carries nothing the step's own full prompt would have: no task
    // title, no acceptance criteria, no diff, no role instructions — only the one sentence
    // and the exact report commands the review step may run.
    let nudge_prompt = std::fs::read_to_string(&nudge_prompt_file)?;
    assert!(
        nudge_prompt.starts_with(
            "You ended without running the report command. Run exactly one of these now:\n\n"
        ),
        "{nudge_prompt}"
    );
    assert!(
        nudge_prompt.contains("report --token my-app/1/1 approved"),
        "{nudge_prompt}"
    );
    assert!(
        nudge_prompt.contains("report --token my-app/1/1 changes-requested --findings <file>"),
        "{nudge_prompt}"
    );
    assert!(!nudge_prompt.contains("Review nudge"), "{nudge_prompt}");
    assert!(!nudge_prompt.contains("it works"), "{nudge_prompt}");
    assert!(
        !nudge_prompt.contains("Reviewer's own instructions"),
        "{nudge_prompt}"
    );
    Ok(())
}

#[test]
fn a_reviewer_that_ignores_the_nudge_goes_to_decide_with_the_tail_of_its_output() -> Result<()> {
    let fixture = Fixture::new()?;
    let resolve_prompt_file = fixture.work.join("resolve-prompt.txt");
    let body = format!(
        "```bash\n\
         if [ \"$3\" = \"implementation\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" done\n\
         elif [ \"$3\" = \"review\" ]; then\n\
         \x20\x20if [ -z \"$4\" ]; then\n\
         \x20\x20\x20\x20echo \"KTASK_SESSION: nudge-sess\"\n\
         \x20\x20else\n\
         \x20\x20\x20\x20echo \"the nudge was heard but ignored\"\n\
         \x20\x20fi\n\
         elif [ \"$3\" = \"resolve\" ]; then\n\
         \x20\x20cp \"$7\" \"{resolve_prompt_file}\"\n\
         \x20\x20ktask-rs report --token \"$1\" stop --reason \"giving up\"\n\
         elif [ \"$3\" = \"testing\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" accepted\n\
         fi\n\
         ```\n",
        resolve_prompt_file = resolve_prompt_file.display(),
    );
    fixture.add_agent_task("Review ignores nudge", &body)?;

    let outcome = fixture.run_the_queue()?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);

    let lines = fixture.status_lines()?;
    let review_line = lines
        .iter()
        .find(|line| line.starts_with("\tattempt 1: review\t"))
        .expect("the review step's own line");
    assert!(
        review_line.contains("failed-unknown\trouted: decide — no report"),
        "{review_line}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("\tattempt 1: resolve\t")),
        "{lines:?}"
    );

    let resolve_prompt = std::fs::read_to_string(&resolve_prompt_file)?;
    assert!(
        resolve_prompt.contains("## Why this came to you"),
        "{resolve_prompt}"
    );
    assert!(
        resolve_prompt.contains("The end of its output:\nthe nudge was heard but ignored"),
        "{resolve_prompt}"
    );
    Ok(())
}
