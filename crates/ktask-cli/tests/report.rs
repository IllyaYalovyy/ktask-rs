//! `ktask-rs report` on the real binary: an agent states the outcome of an attempt, by its
//! token, from any directory.
//!
//! `run` (a later task) is the one that will normally hand out tokens; until then, these
//! tests start attempts directly through `ktask_core`, against the same journal the binary
//! opens, exactly as the task's acceptance criteria call for.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::{Path, PathBuf};

use ktask_adapters::SqliteJournal;
use ktask_core::TaskId;
use repo::{git_repository, scratch};
use rusqlite::OptionalExtension;
use support::{Outcome as ProcessOutcome, Result, Sandbox};

/// A sandbox with a git repository called `my-app`, whose queue holds one pending task,
/// numbered 1.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let fixture = Self {
            sandbox,
            work,
            repository,
            _keep: keep,
        };
        let added = fixture.run(&["add", "--title", "a", "--criterion", "it works"])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(fixture)
    }

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<ProcessOutcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs` with `args` from `other`, a directory nothing was registered from.
    fn run_from(&self, other: &Path, args: &[&str]) -> Result<ProcessOutcome> {
        self.sandbox.run(other, args)
    }

    fn journal(&self) -> PathBuf {
        self.sandbox.state_dir().join("my-app").join("journal.db")
    }

    /// Starts the next attempt at task `task`, through `ktask_core`, directly against the
    /// journal the binary itself will open, and returns its token.
    fn start_attempt(&self, task: u64) -> Result<String> {
        let journal = SqliteJournal::open(&self.journal())?;
        let token = ktask_core::start_attempt(
            &journal,
            &ktask_adapters::SystemClock,
            "my-app",
            TaskId(task),
        )?;
        Ok(token.to_string())
    }

    /// The number of `attempt_reported` events, and the outcome and reason of the most recent
    /// one.
    fn last_report(&self) -> Result<(i64, Option<String>, Option<String>)> {
        let database = rusqlite::Connection::open(self.journal())?;
        let count: i64 = database.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'attempt_reported'",
            [],
            |row| row.get(0),
        )?;
        let payload: Option<String> = database
            .query_row(
                "SELECT payload FROM events WHERE kind = 'attempt_reported'
                 ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(payload) = payload else {
            return Ok((count, None, None));
        };
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        let outcome = payload
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok((count, outcome, reason))
    }

    fn task_status(&self, task: u64) -> Result<String> {
        let database = rusqlite::Connection::open(self.journal())?;
        Ok(database.query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [i64::try_from(task)?],
            |row| row.get(0),
        )?)
    }
}

#[test]
fn a_valid_report_for_a_running_attempt_is_recorded_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let outcome = fixture.run(&["report", "--token", &token, "done"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stderr, "");
    assert_eq!(fixture.last_report()?, (1, Some("done".to_owned()), None));
    Ok(())
}

#[test]
fn failed_needs_input_and_too_large_require_a_reason_but_done_does_not() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    for outcome in ["failed", "needs-input", "too-large"] {
        let result = fixture.run(&["report", "--token", &token, outcome])?;
        assert_eq!(result.code, Some(2), "{outcome}: {}", result.stderr);
        assert_eq!(result.stdout, "");
        assert!(
            result.stderr.contains("reason"),
            "{outcome}: {}",
            result.stderr
        );
    }
    // None of the refusals recorded anything.
    assert_eq!(fixture.last_report()?, (0, None, None));

    // The same outcomes succeed once a reason is given.
    for outcome in ["failed", "needs-input", "too-large"] {
        let result = fixture.run(&["report", "--token", &token, outcome, "--reason", "why"])?;
        assert_eq!(result.code, Some(0), "{outcome}: {}", result.stderr);
    }

    // `done` needs no reason at all.
    let done = fixture.run(&["report", "--token", &token, "done"])?;
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    Ok(())
}

#[test]
fn an_unknown_outcome_exits_two_naming_it_and_records_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let outcome = fixture.run(&["report", "--token", &token, "bogus"])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("unknown outcome") && outcome.stderr.contains("bogus"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn a_malformed_token_exits_two_naming_the_problem_and_records_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    for token in ["", "no-slashes", "my-app/1", "my-app/x/1", "my-app/1/x"] {
        let outcome = fixture.run(&["report", "--token", token, "done"])?;
        assert_eq!(outcome.code, Some(2), "{token:?}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains("token"),
            "{token:?}: {}",
            outcome.stderr
        );
    }
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn a_token_naming_an_unregistered_project_exits_two_naming_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&["report", "--token", "ghost/1/1", "done"])?;
    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("ghost"), "{}", outcome.stderr);
    Ok(())
}

#[test]
fn a_token_naming_an_unknown_task_or_the_wrong_attempt_exits_two_and_records_nothing() -> Result<()>
{
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let unknown_task = fixture.run(&["report", "--token", "my-app/9/1", "done"])?;
    assert_eq!(unknown_task.code, Some(2), "{}", unknown_task.stderr);
    assert!(
        unknown_task.stderr.contains("attempt") && unknown_task.stderr.contains('9'),
        "{}",
        unknown_task.stderr
    );

    let wrong_number = fixture.run(&["report", "--token", "my-app/1/2", "done"])?;
    assert_eq!(wrong_number.code, Some(2), "{}", wrong_number.stderr);
    assert!(
        wrong_number.stderr.contains("attempt 2"),
        "{}",
        wrong_number.stderr
    );

    assert_eq!(fixture.last_report()?, (0, None, None));
    // The valid token still works: neither refusal touched its attempt.
    let valid = fixture.run(&["report", "--token", &token, "done"])?;
    assert_eq!(valid.code, Some(0), "{}", valid.stderr);
    Ok(())
}

#[test]
fn a_token_whose_attempt_has_ended_exits_two_naming_it_and_records_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    // `run`, finding the attempt already running from outside itself, treats it exactly as
    // it would an attempt a crashed run left behind: it ends it `failed-unknown` and stops.
    let interrupted = fixture.run(&["run"])?;
    assert_eq!(interrupted.code, Some(1), "{}", interrupted.stderr);
    assert_eq!(fixture.task_status(1)?, "failed-unknown");

    let outcome = fixture.run(&["report", "--token", &token, "done"])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("ended"), "{}", outcome.stderr);
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn a_second_valid_report_for_the_same_attempt_replaces_the_first() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let first = fixture.run(&[
        "report",
        "--token",
        &token,
        "failed",
        "--reason",
        "first try",
    ])?;
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(
        fixture.last_report()?,
        (1, Some("failed".to_owned()), Some("first try".to_owned()))
    );

    let second = fixture.run(&["report", "--token", &token, "done"])?;
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(fixture.last_report()?, (2, Some("done".to_owned()), None));
    Ok(())
}

#[test]
fn report_works_from_any_directory_with_no_project_flag() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    let elsewhere = git_repository(&fixture.sandbox, &fixture.work, "elsewhere")?;

    let outcome = fixture.run_from(&elsewhere, &["report", "--token", &token, "done"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.last_report()?, (1, Some("done".to_owned()), None));
    Ok(())
}

impl Fixture {
    /// The `findings` array of the most recent `attempt_reported` event's payload.
    fn last_report_findings(&self) -> Result<serde_json::Value> {
        let database = rusqlite::Connection::open(self.journal())?;
        let payload: String = database.query_row(
            "SELECT payload FROM events WHERE kind = 'attempt_reported'
             ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        Ok(payload.get("findings").cloned().unwrap_or_default())
    }

    /// Writes `text` to a file named `name` in the sandbox's own working directory, so a
    /// `--findings` or `--tasks` flag can name it by a relative path.
    fn write_file(&self, name: &str, text: &str) -> Result<PathBuf> {
        let path = self.repository.join(name);
        std::fs::write(&path, text)?;
        Ok(path)
    }
}

#[test]
fn changes_requested_with_a_json_findings_file_is_recorded() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.json",
        r#"[{"location": "src/a.rs:1", "problem": "p", "fix": "f", "scope": "task"}]"#,
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        fixture.last_report()?.1,
        Some("changes-requested".to_owned())
    );
    assert_eq!(
        fixture.last_report_findings()?,
        serde_json::json!([{
            "location": "src/a.rs:1",
            "problem": "p",
            "fix": "f",
            "scope": "task",
        }])
    );
    Ok(())
}

#[test]
fn changes_requested_with_a_toml_findings_file_is_recorded() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.toml",
        "[[findings]]\nlocation = \"src/a.rs:1\"\nproblem = \"p\"\nfix = \"f\"\nscope = \"elsewhere\"\n",
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--findings",
        "findings.toml",
    ])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let findings = fixture.last_report_findings()?;
    assert_eq!(
        findings.get(0).and_then(|f| f.get("scope")),
        Some(&serde_json::json!("elsewhere"))
    );
    Ok(())
}

#[test]
fn changes_requested_without_findings_is_refused_and_records_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let outcome = fixture.run(&["report", "--token", &token, "changes-requested"])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("--findings"), "{}", outcome.stderr);
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn changes_requested_with_reason_but_no_findings_is_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--reason",
        "fix it",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("--findings"), "{}", outcome.stderr);
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn approved_takes_no_findings_file() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.json",
        r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "task"}]"#,
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "approved",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("approved") && outcome.stderr.contains("--findings"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn an_empty_findings_list_is_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file("findings.json", "[]")?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("at least one finding"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn a_finding_missing_a_field_is_refused_naming_its_index_and_the_field() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.json",
        r#"[
            {"location": "a", "problem": "p", "fix": "f", "scope": "task"},
            {"location": "a", "problem": "p", "fix": "f"}
        ]"#,
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("finding 2") && outcome.stderr.contains("scope"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn a_finding_with_an_unknown_scope_is_refused_naming_its_index_and_the_field() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.json",
        r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "urgent"}]"#,
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "changes-requested",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("finding 1")
            && outcome.stderr.contains("scope")
            && outcome.stderr.contains("urgent"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn findings_is_refused_for_every_outcome_but_changes_requested() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.start_attempt(1)?;
    fixture.write_file(
        "findings.json",
        r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "task"}]"#,
    )?;

    let outcome = fixture.run(&[
        "report",
        "--token",
        &token,
        "done",
        "--findings",
        "findings.json",
    ])?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("only changes-requested"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.last_report()?, (0, None, None));
    Ok(())
}

#[test]
fn report_and_its_options_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("report"), "{}", top.stdout);
    let help = fixture.run(&["report", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    for word in ["--token", "OUTCOME", "--reason", "--findings"] {
        assert!(help.stdout.contains(word), "{word}: {}", help.stdout);
    }
    Ok(())
}
