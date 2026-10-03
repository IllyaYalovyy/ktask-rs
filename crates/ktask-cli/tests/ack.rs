//! `ktask-rs ack` on the real binary: a pending human task can be acknowledged, optionally
//! recording the operator's message, so subsequent runs continue past it.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use serde_json::Value;
use support::{Outcome, Result, Sandbox};

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
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn add_human(&self, title: &str) -> Result<()> {
        let added = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "approved",
            "--kind",
            "human",
        ])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(())
    }

    fn add_agent(&self) -> Result<()> {
        let added = self.run(&["add", "--title", "agent", "--criterion", "works"])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(())
    }

    fn events(&self) -> Result<Vec<(String, String)>> {
        let database = rusqlite::Connection::open(
            self.sandbox.state_home().join("ktask-rs/my-app/journal.db"),
        )?;
        database
            .prepare("SELECT kind, payload FROM events ORDER BY seq")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[test]
fn ack_marks_a_human_task_done_records_its_message_and_unblocks_the_next_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_human("Approve the design")?;

    let stopped = fixture.run(&["run"])?;
    assert_eq!(stopped.code, Some(0), "{}", stopped.stderr);
    assert!(stopped.stdout.contains("human task"), "{}", stopped.stdout);

    let ack = fixture.run(&["ack", "1", "--message", "approved in review"])?;
    assert_eq!(ack.code, Some(0), "{}", ack.stderr);
    assert_eq!(ack.stdout, "acknowledged task 1; it is done\n");
    assert_eq!(ack.stderr, "");

    let listed = fixture.run(&["list", "--json"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let tasks: Value = serde_json::from_str(&listed.stdout)?;
    assert_eq!(tasks[0]["status"], "done");
    assert_eq!(tasks[0]["kind"], "human");

    let events = fixture.events()?;
    assert_eq!(events[1].0, "task_acknowledged");
    assert_eq!(
        serde_json::from_str::<Value>(&events[1].1)?["message"],
        "approved in review"
    );

    let resumed = fixture.run(&["run"])?;
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    assert!(
        resumed.stdout.contains("nothing is pending"),
        "{}",
        resumed.stdout
    );
    Ok(())
}

#[test]
fn ack_without_a_message_records_none_and_refuses_non_human_or_settled_tasks() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_human("Approve")?;
    fixture.add_agent()?;

    let ack = fixture.run(&["ack", "1"])?;
    assert_eq!(ack.code, Some(0), "{}", ack.stderr);
    let events = fixture.events()?;
    assert_eq!(
        serde_json::from_str::<Value>(&events[2].1)?["message"],
        Value::Null
    );

    for id in ["1", "2", "9"] {
        let refused = fixture.run(&["ack", id])?;
        assert_eq!(refused.code, Some(2), "{id}: {}", refused.stderr);
        assert_eq!(refused.stdout, "");
    }
    assert_eq!(
        fixture.events()?.len(),
        3,
        "a refused ack changed the journal"
    );
    Ok(())
}

#[test]
fn ack_help_and_invalid_arguments_are_reported_by_the_real_binary() -> Result<()> {
    let fixture = Fixture::new()?;
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("ack"), "{}", top.stdout);
    let help = fixture.run(&["ack", "--help"])?;
    assert_eq!(help.code, Some(0));
    for word in ["ID", "--message", "--project"] {
        assert!(help.stdout.contains(word), "{word}: {}", help.stdout);
    }
    for args in [&["ack"][..], &["ack", "x"][..], &["ack", "-1"][..]] {
        let outcome = fixture.run(args)?;
        assert_eq!(outcome.code, Some(2), "{:?}: {}", args, outcome.stderr);
        assert_eq!(outcome.stdout, "");
    }
    Ok(())
}
