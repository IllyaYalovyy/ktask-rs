//! `ktask-rs add` and `ktask-rs list` on the real binary, against the journal it keeps.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use serde_json::{Value, json};
use support::{Outcome, Result, Sandbox};

/// A sandbox with a git repository called `my-app` in a scratch directory.
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

    /// Adds a task with the given title and one criterion, and returns what was printed.
    fn add(&self, title: &str) -> Result<Outcome> {
        self.run(&["add", "--title", title, "--criterion", "it works"])
    }

    fn journal(&self, project: &str) -> PathBuf {
        self.sandbox
            .state_home()
            .join("ktask-rs")
            .join(project)
            .join("journal.db")
    }

    /// The rows of `table` in the journal of `my-app`, as JSON objects, oldest first.
    fn rows(&self, table: &str) -> Result<Vec<Value>> {
        let database = rusqlite::Connection::open(self.journal("my-app"))?;
        let mut statement = database.prepare(&format!("SELECT * FROM {table} ORDER BY 1"))?;
        let names: Vec<String> = statement
            .column_names()
            .iter()
            .map(|n| (*n).to_owned())
            .collect();
        let rows = statement
            .query_map([], |row| {
                let mut object = serde_json::Map::new();
                for (index, name) in names.iter().enumerate() {
                    let value: rusqlite::types::Value = row.get(index)?;
                    object.insert(
                        name.clone(),
                        match value {
                            rusqlite::types::Value::Integer(n) => json!(n),
                            rusqlite::types::Value::Text(text) => json!(text),
                            other => json!(format!("{other:?}")),
                        },
                    );
                }
                Ok(Value::Object(object))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Asserts that nothing was added: the queue lists nothing and no event was recorded.
    fn assert_nothing_added(&self) -> Result<()> {
        let listed = self.run(&["list"])?;
        assert_eq!(listed.stdout, "", "a task was added");
        assert_eq!(listed.code, Some(0));
        assert_eq!(
            self.rows("events")?,
            Vec::<Value>::new(),
            "an event was recorded"
        );
        Ok(())
    }
}

fn assert_usage_error(outcome: &Outcome, naming: &[&str]) {
    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    for name in naming {
        assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
    }
}

#[test]
fn add_with_a_title_and_a_criterion_prints_the_new_id_and_the_task_ends_the_list() -> Result<()> {
    let fixture = Fixture::new()?;

    let first = fixture.add("First task")?;
    assert_eq!(first.stdout, "1\n");
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let second = fixture.add("Second task")?;
    assert_eq!(second.stdout, "2\n");
    assert_eq!(second.code, Some(0), "{}", second.stderr);

    let listed = fixture.run(&["list"])?;
    assert_eq!(
        listed.stdout,
        "1\t#1\tpending\tagent\tFirst task\n2\t#2\tpending\tagent\tSecond task\n"
    );
    assert_eq!(listed.code, Some(0));
    Ok(())
}

#[test]
fn list_with_no_tasks_prints_nothing_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "");
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    Ok(())
}

#[test]
fn every_option_reaches_the_stored_task_and_the_event_that_created_it() -> Result<()> {
    let fixture = Fixture::new()?;

    let added = fixture.run(&[
        "add",
        "--title",
        "Full task",
        "--criterion",
        "first criterion",
        "--criterion",
        "second criterion",
        "--body",
        "Some body\nover two lines",
        "--kind",
        "human",
        "--link",
        "github:owner/repo#123",
        "--link",
        "https://example.com/a",
    ])?;

    assert_eq!(added.stdout, "1\n");
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let tasks = fixture.rows("tasks")?;
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task["id"], 1);
    assert_eq!(task["title"], "Full task");
    assert_eq!(task["body"], "Some body\nover two lines");
    assert_eq!(task["kind"], "human");
    assert_eq!(task["status"], "pending");
    let criteria: Value = serde_json::from_str(task["criteria"].as_str().unwrap_or_default())?;
    assert_eq!(criteria, json!(["first criterion", "second criterion"]));
    let links: Value = serde_json::from_str(task["links"].as_str().unwrap_or_default())?;
    assert_eq!(
        links,
        json!(["github:owner/repo#123", "https://example.com/a"])
    );
    let events = fixture.rows("events")?;
    assert_eq!(events.len(), 1, "adding a task is one event");
    assert_eq!(events[0]["kind"], "task_added");
    assert_eq!(events[0]["task_id"], 1);
    let payload: Value = serde_json::from_str(events[0]["payload"].as_str().unwrap_or_default())?;
    assert_eq!(
        payload,
        json!({
            "title": "Full task",
            "body": "Some body\nover two lines",
            "criteria": ["first criterion", "second criterion"],
            "kind": "human",
            "links": ["github:owner/repo#123", "https://example.com/a"],
        })
    );
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "1\t#1\tpending\thuman\tFull task\n");
    Ok(())
}

#[test]
fn without_options_the_kind_is_agent_and_the_body_and_links_are_empty() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Plain")?;
    let tasks = fixture.rows("tasks")?;
    assert_eq!(tasks[0]["kind"], "agent");
    assert_eq!(tasks[0]["body"], "");
    assert_eq!(tasks[0]["links"], "[]");
    Ok(())
}

#[test]
fn kind_agent_can_be_given_explicitly() -> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.run(&["add", "--title", "t", "--criterion", "c", "--kind", "agent"])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert_eq!(fixture.rows("tasks")?[0]["kind"], "agent");
    Ok(())
}

#[test]
fn a_missing_title_exits_two_naming_the_option_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&["add", "--criterion", "c"])?;
    assert_usage_error(&outcome, &["--title"]);
    fixture.assert_nothing_added()
}

#[test]
fn a_missing_criterion_exits_two_naming_the_option_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&["add", "--title", "t"])?;
    assert_usage_error(&outcome, &["--criterion"]);
    fixture.assert_nothing_added()
}

#[test]
fn an_empty_or_blank_title_exits_two_saying_so_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    for title in ["", "   "] {
        let outcome = fixture.run(&["add", "--title", title, "--criterion", "c"])?;
        assert_usage_error(&outcome, &["title is empty"]);
    }
    fixture.assert_nothing_added()
}

#[test]
fn an_empty_criterion_exits_two_saying_so_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&[
        "add",
        "--title",
        "t",
        "--criterion",
        "ok",
        "--criterion",
        "",
    ])?;
    assert_usage_error(&outcome, &["criterion is empty"]);
    fixture.assert_nothing_added()
}

#[test]
fn an_unknown_kind_exits_two_naming_it_and_the_choices_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&["add", "--title", "t", "--criterion", "c", "--kind", "robot"])?;
    assert_usage_error(&outcome, &["robot", "--kind", "agent", "human"]);
    fixture.assert_nothing_added()
}

#[test]
fn a_malformed_link_exits_two_naming_it_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    for link in [
        "not a link",
        "github:owner/repo",
        "github:owner/repo#abc",
        "ftp://example.com",
        "https://",
    ] {
        let outcome = fixture.run(&[
            "add",
            "--title",
            "t",
            "--criterion",
            "c",
            "--link",
            "https://fine.example",
            "--link",
            link,
        ])?;
        assert_usage_error(&outcome, &["malformed link", link]);
    }
    fixture.assert_nothing_added()
}

#[test]
fn ids_are_sequential_and_survive_restarts() -> Result<()> {
    let fixture = Fixture::new()?;
    // Every run is a new process; the second starts with only the journal on disk.
    assert_eq!(fixture.add("one")?.stdout, "1\n");
    assert_eq!(fixture.add("two")?.stdout, "2\n");
    // A refused task uses no number.
    let refused = fixture.run(&["add", "--title", "", "--criterion", "c"])?;
    assert_eq!(refused.code, Some(2));
    assert_eq!(fixture.add("three")?.stdout, "3\n");

    let ids: Vec<_> = fixture
        .rows("tasks")?
        .iter()
        .map(|task| task["id"].as_i64())
        .collect();
    assert_eq!(ids, [Some(1), Some(2), Some(3)]);
    Ok(())
}

#[test]
fn a_task_can_be_added_and_listed_from_a_subdirectory() -> Result<()> {
    let fixture = Fixture::new()?;
    let deep = fixture.repository.join("src").join("deep");
    std::fs::create_dir_all(&deep)?;

    let added = fixture
        .sandbox
        .run(&deep, &["add", "--title", "from deep", "--criterion", "c"])?;
    assert_eq!(added.stdout, "1\n");
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "1\t#1\tpending\tagent\tfrom deep\n");
    Ok(())
}

#[test]
fn project_selects_the_queue_from_any_directory_and_queues_are_separate() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    fixture.add("mine")?;
    let shown = fixture.sandbox.run(&other, &["project", "show"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);

    // From `other-app`, work on `my-app`.
    let added = fixture.sandbox.run(
        &other,
        &[
            "add",
            "--project",
            "my-app",
            "--title",
            "added remotely",
            "--criterion",
            "c",
        ],
    )?;
    assert_eq!(added.stdout, "2\n", "{}", added.stderr);
    let mine = fixture
        .sandbox
        .run(&other, &["list", "--project", "my-app"])?;
    assert_eq!(
        mine.stdout,
        "1\t#1\tpending\tagent\tmine\n2\t#2\tpending\tagent\tadded remotely\n"
    );
    // `other-app` has its own queue, numbered from 1.
    let theirs = fixture.sandbox.run(&other, &["list"])?;
    assert_eq!(theirs.stdout, "");
    let first_there = fixture
        .sandbox
        .run(&other, &["add", "--title", "theirs", "--criterion", "c"])?;
    assert_eq!(first_there.stdout, "1\n");
    let unknown = fixture.run(&["list", "--project", "ghost"])?;
    assert_usage_error(&unknown, &["ghost", "ktask-rs project list"]);
    let unknown_add = fixture.run(&[
        "add",
        "--project",
        "ghost",
        "--title",
        "t",
        "--criterion",
        "c",
    ])?;
    assert_usage_error(&unknown_add, &["ghost"]);
    Ok(())
}

#[test]
fn a_journal_that_is_not_a_database_exits_one_naming_the_file() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("first")?;
    let path = fixture.journal("my-app");
    std::fs::write(
        &path,
        "this is not sqlite, and it is long enough to be checked",
    )?;

    for args in [
        &["list"][..],
        &["add", "--title", "t", "--criterion", "c"][..],
    ] {
        let outcome = fixture.run(args)?;
        assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome.stderr.contains(&path.display().to_string()),
            "{}",
            outcome.stderr
        );
    }
    Ok(())
}

#[test]
fn add_and_list_are_in_the_help_with_their_options() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["--help"])?;
    assert!(help.stdout.contains("add"), "{}", help.stdout);
    assert!(help.stdout.contains("list"), "{}", help.stdout);
    let add = fixture.run(&["add", "--help"])?;
    for option in [
        "--title",
        "--criterion",
        "--body",
        "--kind",
        "--link",
        "--project",
    ] {
        assert!(add.stdout.contains(option), "{option}: {}", add.stdout);
    }
    assert_eq!(add.code, Some(0));
    Ok(())
}
