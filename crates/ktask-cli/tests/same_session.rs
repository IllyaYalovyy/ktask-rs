//! M4-05b on the real binary: the resolver's `retry --same-session` resumes the failed
//! attempt's own session instead of starting over — the echo provider implements sessions for
//! real, so this is tested with no model and no network.

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

/// A bash block: on attempt 1, the implementation step reports the session `carried-over` and
/// fails with `reason`; the resolve step retries with `--same-session`; attempt 2's
/// implementation step is told to resume `carried-over`, receiving it as `$4` and the path of
/// its transcript as `$5` — it writes both to files under `work`, reads the transcript, and
/// reports `done` only if it finds `marker`, the exact text attempt 1's own output carried;
/// the review and test steps, when reached, approve and accept.
fn same_session_body(reason: &str, marker: &str, work: &std::path::Path) -> String {
    let resume_session_file = work.join("resume-session-seen");
    let found_marker_file = work.join("found-marker");
    format!(
        "```bash\n\
         if [ \"$3\" = \"review\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" approved\n\
         elif [ \"$3\" = \"testing\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" accepted\n\
         elif [ \"$3\" = \"resolve\" ]; then\n\
         \x20\x20ktask-rs report --token \"$1\" retry --same-session\n\
         elif [ \"$2\" = \"1\" ]; then\n\
         \x20\x20echo \"KTASK_SESSION: carried-over\"\n\
         \x20\x20echo \"{marker}\"\n\
         \x20\x20ktask-rs report --token \"$1\" failed --reason \"{reason}\"\n\
         else\n\
         \x20\x20printf '%s' \"$4\" > \"{resume_session_file}\"\n\
         \x20\x20if grep -q \"{marker}\" \"$5\"; then\n\
         \x20\x20\x20\x20printf 'yes' > \"{found_marker_file}\"\n\
         \x20\x20\x20\x20echo \"KTASK_SESSION: carried-over\"\n\
         \x20\x20\x20\x20ktask-rs report --token \"$1\" done\n\
         \x20\x20else\n\
         \x20\x20\x20\x20printf 'no' > \"{found_marker_file}\"\n\
         \x20\x20\x20\x20ktask-rs report --token \"$1\" failed --reason \"transcript missing the marker\"\n\
         \x20\x20fi\n\
         fi\n\
         ```\n",
        resume_session_file = resume_session_file.display(),
        found_marker_file = found_marker_file.display(),
    )
}

#[test]
fn a_resumed_attempt_reads_the_earlier_transcript_and_the_session_stays_the_same() -> Result<()> {
    let fixture = Fixture::new()?;
    let resume_session_file = fixture.work.join("resume-session-seen");
    let found_marker_file = fixture.work.join("found-marker");
    fixture.add_agent_task(
        "a",
        &same_session_body("it broke", "MARKER-FROM-ATTEMPT-ONE", &fixture.work),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    // The resumed invocation actually received the earlier session as its fourth positional
    // argument, and actually read the earlier attempt's own output back from the transcript
    // path it received as its fifth — not just a label recorded for display.
    assert_eq!(
        std::fs::read_to_string(&resume_session_file)?,
        "carried-over"
    );
    assert_eq!(std::fs::read_to_string(&found_marker_file)?, "yes");
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\tsession:carried-over\tusage none",
            "\tattempt 1: resolve\techo\t0s\tretry\tusage none",
            "\tattempt 2: implementation\techo\t0s\tdone\tsession:carried-over\tusage none",
            "\tattempt 2: review\techo\t0s\tapproved\tusage none",
            "\tattempt 2: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 2: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn a_script_that_never_reports_a_session_leaves_same_session_with_nothing_to_resume() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"no session to resume\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue()?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);

    // The attempt that just failed never reported any session: `--same-session` against it is
    // refused by `ktask-rs report` itself, naming why.
    let refused = fixture.run(&["report", "--token", "my-app/1/1", "retry", "--same-session"])?;
    assert_eq!(refused.code, Some(2), "{}", refused.stdout);
    assert!(
        refused.stderr.contains("reported no session to continue"),
        "{}",
        refused.stderr
    );
    Ok(())
}

#[test]
fn same_session_and_its_flag_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["report", "--help"])?;
    assert!(help.stdout.contains("--same-session"), "{}", help.stdout);
    Ok(())
}
