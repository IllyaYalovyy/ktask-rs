//! `ktask-rs add` and `ktask-rs list` on the real binary, against the journal it keeps.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::os::unix::fs::PermissionsExt;
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
        "--provider",
        "codex",
        "--model",
        "gpt-5",
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
    assert_eq!(task["provider"], "codex");
    assert_eq!(task["model"], "gpt-5");
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
            "provider": "codex",
            "model": "gpt-5",
        })
    );
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "1\t#1\tpending\thuman\tFull task\n");
    Ok(())
}

#[test]
fn an_unknown_task_provider_is_refused_naming_the_known_providers() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&[
        "add",
        "--title",
        "t",
        "--criterion",
        "c",
        "--provider",
        "unknown",
    ])?;
    assert_usage_error(&outcome, &["unknown provider", "claude", "codex", "echo"]);
    fixture.assert_nothing_added()
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
fn a_control_character_in_the_title_a_criterion_or_a_link_exits_two_naming_it_readably()
-> Result<()> {
    let fixture = Fixture::new()?;
    for (args, naming) in [
        (
            vec!["--title", "bad\ntitle", "--criterion", "c"],
            "the title contains a control character: \\n",
        ),
        (
            vec!["--title", "t", "--criterion", "bad\tcriterion"],
            "an acceptance criterion contains a control character: \\t",
        ),
        (
            vec![
                "--title",
                "t",
                "--criterion",
                "c",
                "--link",
                "https://example.com/a\x1bb",
            ],
            "a link contains a control character: \\x1b",
        ),
    ] {
        let mut full = vec!["add"];
        full.extend(args);
        let outcome = fixture.run(&full)?;
        assert_usage_error(&outcome, &[naming]);
        // Only the readable form is printed, never the raw control character.
        assert!(!outcome.stderr.contains('\t'), "{:?}", outcome.stderr);
        assert!(!outcome.stderr.contains('\x1b'), "{:?}", outcome.stderr);
    }
    fixture.assert_nothing_added()
}

#[test]
fn the_body_accepts_newlines_and_tabs_but_refuses_an_escape_character() -> Result<()> {
    let fixture = Fixture::new()?;
    let ok = fixture.run(&[
        "add",
        "--title",
        "t",
        "--criterion",
        "c",
        "--body",
        "line one\nline two\twith a tab",
    ])?;
    assert_eq!(ok.code, Some(0), "{}", ok.stderr);

    let refused = fixture.run(&[
        "add",
        "--title",
        "t2",
        "--criterion",
        "c",
        "--body",
        "before\x1bafter",
    ])?;
    assert_usage_error(&refused, &["the body contains a control character: \\x1b"]);
    let tasks = fixture.rows("tasks")?;
    assert_eq!(tasks.len(), 1, "only the accepted task was added");
    Ok(())
}

#[test]
fn every_problem_with_a_task_is_reported_not_only_the_first() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&[
        "add",
        "--title",
        "bad\ntitle",
        "--criterion",
        "",
        "--link",
        "not-a-link",
    ])?;
    assert_usage_error(
        &outcome,
        &[
            "the title contains a control character: \\n",
            "criterion is empty",
            "malformed link \"not-a-link\"",
        ],
    );
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
fn two_processes_adding_at_the_same_moment_both_succeed_with_unique_ids_and_contiguous_positions()
-> Result<()> {
    let fixture = Fixture::new()?;
    // Registers the project first, so the two concurrent processes below race only on the
    // journal, not on first-time project registration (a different concern entirely).
    fixture.add("seed")?;

    // Two real `ktask-rs add` processes, started together against the same journal: one of
    // them must find the journal has moved on since it read it, and retry.
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| fixture.add("from a").map_err(|e| e.to_string()));
        let b = scope.spawn(|| fixture.add("from b").map_err(|e| e.to_string()));
        (a.join().unwrap(), b.join().unwrap())
    });
    let first = first?;
    let second = second?;

    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    let first_id: u64 = first.stdout.trim().parse()?;
    let second_id: u64 = second.stdout.trim().parse()?;
    assert_ne!(first_id, second_id, "both processes were given the same id");
    let mut ids = [first_id, second_id];
    ids.sort_unstable();
    assert_eq!(ids, [2, 3], "ids are not unique and contiguous");

    let listed = queue(&fixture)?;
    assert_eq!(listed.len(), 3);
    let positions: Vec<_> = fixture
        .rows("tasks")?
        .iter()
        .map(|task| task["id"].as_i64())
        .collect();
    assert_eq!(positions, [Some(1), Some(2), Some(3)]);
    Ok(())
}

#[test]
fn a_journal_written_by_a_previous_version_is_still_read_correctly_by_the_real_binary() -> Result<()>
{
    let fixture = Fixture::new()?;
    let path = fixture.journal("my-app");
    std::fs::create_dir_all(path.parent().ok_or("journal has no parent directory")?)?;
    {
        // The schema and rows exactly as a previous version — before events and appends
        // moved into `ktask-rs::add_tasks` and `SqliteJournal::append_events` — wrote them,
        // written here with nothing but `rusqlite`, not through this crate's own code.
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE events (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 at INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 task_id INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE TABLE tasks (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 order_key INTEGER NOT NULL,
                 title TEXT NOT NULL,
                 body TEXT NOT NULL,
                 criteria TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 links TEXT NOT NULL,
                 status TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 attempt_number INTEGER NOT NULL DEFAULT 0
             )",
        )?;
        connection.execute(
            "INSERT INTO tasks
                 (id, order_key, title, body, criteria, kind, links, status, created_at)
             VALUES (1, 1, 'old task', '', '[\"it works\"]', 'agent', '[]', 'cancelled', 500)",
            [],
        )?;
        connection.execute(
            "INSERT INTO events (at, kind, task_id, payload) VALUES (500, 'task_added', 1, ?1)",
            [r#"{"title":"old task","body":"","criteria":["it works"],"kind":"agent","links":[]}"#],
        )?;
        connection.execute(
            "INSERT INTO events (at, kind, task_id, payload) VALUES (600, 'task_cancelled', 1, '{}')",
            [],
        )?;
    }

    let listed = fixture.run(&["list", "--all"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stdout, "1\t#1\tcancelled\tagent\told task\n");

    // Adding through the real binary continues the ids on from what the old journal used.
    let added = fixture.add("new")?;
    assert_eq!(added.stdout, "2\n", "{}", added.stderr);
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "1\t#2\tpending\tagent\tnew\n");
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
fn project_named_before_or_after_add_or_list_gives_the_same_result() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    for repository in [&fixture.repository, &other] {
        assert_eq!(
            fixture.sandbox.run(repository, &["project", "show"])?.code,
            Some(0)
        );
    }

    let before = fixture.sandbox.run(
        &other,
        &[
            "--project",
            "my-app",
            "add",
            "--title",
            "t1",
            "--criterion",
            "c",
        ],
    )?;
    assert_eq!(before.stdout, "1\n", "{}", before.stderr);
    let after = fixture.sandbox.run(
        &other,
        &[
            "add",
            "--title",
            "t2",
            "--criterion",
            "c",
            "--project",
            "my-app",
        ],
    )?;
    assert_eq!(after.stdout, "2\n", "{}", after.stderr);

    let listed_before = fixture
        .sandbox
        .run(&other, &["--project", "my-app", "list"])?;
    let listed_after = fixture
        .sandbox
        .run(&other, &["list", "--project", "my-app"])?;
    assert_eq!(listed_before.stdout, listed_after.stdout);
    assert_eq!(listed_before.stderr, listed_after.stderr);
    assert_eq!(listed_before.code, listed_after.code);
    assert_eq!(
        listed_before.stdout,
        "1\t#1\tpending\tagent\tt1\n2\t#2\tpending\tagent\tt2\n"
    );
    Ok(())
}

#[test]
fn project_named_twice_with_different_values_on_add_exits_two_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    assert_eq!(
        fixture.sandbox.run(&other, &["project", "show"])?.code,
        Some(0)
    );

    let outcome = fixture.sandbox.run(
        &other,
        &[
            "--project",
            "my-app",
            "add",
            "--title",
            "t",
            "--criterion",
            "c",
            "--project",
            "other-app",
        ],
    )?;

    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("\"my-app\""), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("\"other-app\""),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    fixture.assert_nothing_added()?;
    let theirs = fixture.sandbox.run(&other, &["list"])?;
    assert_eq!(theirs.stdout, "");
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
    assert!(
        !help.stdout.to_lowercase().contains("editor"),
        "{}",
        help.stdout
    );
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
    assert!(
        !add.stdout.to_lowercase().contains("editor"),
        "{}",
        add.stdout
    );
    Ok(())
}

#[test]
fn a_missing_title_exits_two_and_starts_no_program_whatever_editor_is_set_to() -> Result<()> {
    let fixture = Fixture::new()?;
    let marker = fixture.work.join("editor-ran");
    let script = fixture.work.join("marking-editor");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

    for editor in [
        None,
        Some(String::new()),
        Some(script.display().to_string()),
    ] {
        let outcome = fixture.sandbox.run_with(
            &fixture.repository,
            &["add", "--criterion", "c"],
            |command| {
                if let Some(editor) = &editor {
                    command.env("EDITOR", editor);
                }
            },
        )?;
        assert_eq!(outcome.code, Some(2), "{editor:?}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains("--title"),
            "{editor:?}: {}",
            outcome.stderr
        );
        assert!(!marker.exists(), "{editor:?}: the editor was started");
    }
    fixture.assert_nothing_added()
}

/// The creation time the journal holds for the task numbered `id`, as `list --json` writes it.
fn stored_creation_time(fixture: &Fixture, id: i64) -> Result<String> {
    let rows = fixture.rows("tasks")?;
    let seconds = rows
        .iter()
        .find(|task| task["id"] == id)
        .and_then(|task| task["created_at"].as_i64())
        .ok_or("no such task in the journal")?;
    Ok(jiff::Timestamp::from_second(seconds)?.to_string())
}

#[test]
fn list_json_prints_every_field_of_every_task_in_queue_order() -> Result<()> {
    let fixture = Fixture::new()?;
    let full = fixture.run(&[
        "add",
        "--title",
        "Full task",
        "--criterion",
        "first criterion",
        "--criterion",
        "second \"quoted\" criterion",
        "--body",
        "Some body\nover two lines — with ünïcode",
        "--kind",
        "human",
        "--link",
        "github:owner/repo#123",
        "--link",
        "https://example.com/a",
    ])?;
    assert_eq!(full.code, Some(0), "{}", full.stderr);
    fixture.add("Plain task")?;

    let listed = fixture.run(&["list", "--json"])?;

    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(listed.stderr, "");
    assert!(listed.stdout.ends_with('\n'), "{}", listed.stdout);
    let parsed: Value = serde_json::from_str(&listed.stdout)?;
    assert_eq!(
        parsed,
        json!([
            {
                "id": 1,
                "position": 1,
                "title": "Full task",
                "body": "Some body\nover two lines — with ünïcode",
                "criteria": ["first criterion", "second \"quoted\" criterion"],
                "kind": "human",
                "links": ["github:owner/repo#123", "https://example.com/a"],
                "status": "pending",
                "created_at": stored_creation_time(&fixture, 1)?,
            },
            {
                "id": 2,
                "position": 2,
                "title": "Plain task",
                "body": "",
                "criteria": ["it works"],
                "kind": "agent",
                "links": [],
                "status": "pending",
                "created_at": stored_creation_time(&fixture, 2)?,
            },
        ])
    );
    Ok(())
}

#[test]
fn list_json_of_an_empty_queue_is_an_empty_array() -> Result<()> {
    let fixture = Fixture::new()?;
    let listed = fixture.run(&["list", "--json"])?;
    assert_eq!(listed.stdout, "[]\n");
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    Ok(())
}

#[test]
fn list_json_is_the_same_bytes_every_time_for_the_same_state() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("one")?;
    fixture.add("two")?;
    let first = fixture.run(&["list", "--json"])?;
    let second = fixture.run(&["list", "--json"])?;
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(!first.stdout.is_empty());
    assert_eq!(first.stdout, second.stdout);
    // Fields are written in a fixed order, not sorted or shuffled.
    assert!(
        first.stdout.starts_with(
            "[{\"id\":1,\"position\":1,\"title\":\"one\",\"body\":\"\",\"criteria\":[\"it works\"],\
             \"kind\":\"agent\",\"links\":[],\"status\":\"pending\",\"created_at\":\""
        ),
        "{}",
        first.stdout
    );
    Ok(())
}

#[test]
fn list_json_works_with_project_and_reports_a_broken_journal() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    fixture.add("mine")?;
    let shown = fixture.sandbox.run(&other, &["project", "show"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);

    let remote = fixture
        .sandbox
        .run(&other, &["list", "--json", "--project", "my-app"])?;
    let tasks: Value = serde_json::from_str(&remote.stdout)?;
    assert_eq!(tasks.as_array().map(Vec::len), Some(1), "{}", remote.stdout);
    assert_eq!(tasks[0]["title"], "mine");
    assert_eq!(
        fixture.sandbox.run(&other, &["list", "--json"])?.stdout,
        "[]\n"
    );

    let unknown = fixture.run(&["list", "--json", "--project", "ghost"])?;
    assert_usage_error(&unknown, &["ghost"]);

    let path = fixture.journal("my-app");
    std::fs::write(
        &path,
        "this is not sqlite, and it is long enough to be checked",
    )?;
    let broken = fixture.run(&["list", "--json"])?;
    assert_eq!(broken.code, Some(1), "{}", broken.stderr);
    assert_eq!(broken.stdout, "");
    Ok(())
}

#[test]
fn list_json_is_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["list", "--help"])?;
    assert!(help.stdout.contains("--json"), "{}", help.stdout);
    assert_eq!(help.code, Some(0));
    Ok(())
}

/// Adds `title` with `--before`/`--after` given as `flag`, and returns what was printed.
fn add_placed(fixture: &Fixture, title: &str, flag: &str, id: &str) -> Result<Outcome> {
    fixture.run(&["add", "--title", title, "--criterion", "it works", flag, id])
}

/// The queue as `id:title` pairs in order, from `list`, checked against `list --json`.
fn queue(fixture: &Fixture) -> Result<Vec<String>> {
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let text: Vec<String> = listed
        .stdout
        .lines()
        .map(|line| {
            let mut fields = line.split('\t').skip(1);
            let id = fields.next().unwrap_or_default().trim_start_matches('#');
            let title = fields.nth(2).unwrap_or_default();
            format!("{id}:{title}")
        })
        .collect();
    let json = fixture.run(&["list", "--json"])?;
    let tasks: Value = serde_json::from_str(&json.stdout)?;
    let from_json: Vec<String> = tasks
        .as_array()
        .ok_or("list --json is not an array")?
        .iter()
        .enumerate()
        .map(|(index, task)| {
            assert_eq!(task["position"], index + 1);
            format!("{}:{}", task["id"], task["title"].as_str().unwrap_or(""))
        })
        .collect();
    assert_eq!(text, from_json);
    Ok(text)
}

/// A fixture whose queue is `a`, `b`, `c`, numbered 1, 2, 3.
fn abc() -> Result<Fixture> {
    let fixture = Fixture::new()?;
    for title in ["a", "b", "c"] {
        fixture.add(title)?;
    }
    Ok(fixture)
}

#[test]
fn before_puts_the_task_immediately_before_the_one_named_and_no_id_changes() -> Result<()> {
    let fixture = abc()?;

    let added = add_placed(&fixture, "new", "--before", "2")?;

    assert_eq!(added.stdout, "4\n");
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert_eq!(queue(&fixture)?, ["1:a", "4:new", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn after_puts_the_task_immediately_after_the_one_named_and_no_id_changes() -> Result<()> {
    let fixture = abc()?;

    let added = add_placed(&fixture, "new", "--after", "2")?;

    assert_eq!(added.stdout, "4\n");
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert_eq!(queue(&fixture)?, ["1:a", "2:b", "4:new", "3:c"]);
    Ok(())
}

#[test]
fn a_task_can_go_before_the_first_and_after_the_last() -> Result<()> {
    let fixture = abc()?;

    assert_eq!(
        add_placed(&fixture, "first", "--before", "1")?.stdout,
        "4\n"
    );
    assert_eq!(add_placed(&fixture, "last", "--after", "3")?.stdout, "5\n");

    assert_eq!(queue(&fixture)?, ["4:first", "1:a", "2:b", "3:c", "5:last"]);
    Ok(())
}

#[test]
fn a_task_can_go_before_the_first_and_after_the_last_in_a_queue_of_one() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("only")?;
    add_placed(&fixture, "before", "--before", "1")?;
    add_placed(&fixture, "after", "--after", "1")?;
    assert_eq!(queue(&fixture)?, ["2:before", "1:only", "3:after"]);
    Ok(())
}

#[test]
fn placing_repeatedly_keeps_every_task_where_it_was_put() -> Result<()> {
    let fixture = abc()?;
    add_placed(&fixture, "d", "--before", "3")?;
    add_placed(&fixture, "e", "--after", "4")?;
    add_placed(&fixture, "f", "--before", "1")?;
    fixture.add("g")?;
    add_placed(&fixture, "h", "--after", "7")?;

    assert_eq!(
        queue(&fixture)?,
        ["6:f", "1:a", "2:b", "4:d", "5:e", "3:c", "7:g", "8:h"]
    );
    Ok(())
}

#[test]
fn a_placed_task_keeps_the_ids_in_the_journal_and_records_where_it_went() -> Result<()> {
    let fixture = abc()?;
    add_placed(&fixture, "new", "--before", "2")?;
    add_placed(&fixture, "newer", "--after", "3")?;

    let ids: Vec<_> = fixture
        .rows("tasks")?
        .iter()
        .map(|task| {
            (
                task["id"].as_i64(),
                task["title"].as_str().map(str::to_owned),
            )
        })
        .collect();
    let expected: Vec<_> = [(1, "a"), (2, "b"), (3, "c"), (4, "new"), (5, "newer")]
        .into_iter()
        .map(|(id, title)| (Some(id), Some(title.to_owned())))
        .collect();
    assert_eq!(ids, expected);
    let events = fixture.rows("events")?;
    assert_eq!(events.len(), 5, "adding a task is one event, placed or not");
    let payload = |index: usize| -> Result<Value> {
        Ok(serde_json::from_str(
            events[index]["payload"].as_str().unwrap_or_default(),
        )?)
    };
    assert_eq!(payload(0)?.get("before"), None);
    assert_eq!(payload(0)?.get("after"), None);
    assert_eq!(payload(3)?["before"], 2);
    assert_eq!(payload(4)?["after"], 3);
    Ok(())
}

#[test]
fn a_placed_task_keeps_every_option_it_was_given() -> Result<()> {
    let fixture = abc()?;
    let added = fixture.run(&[
        "add",
        "--title",
        "Full",
        "--criterion",
        "one",
        "--body",
        "text",
        "--kind",
        "human",
        "--link",
        "https://example.com",
        "--after",
        "1",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let listed = fixture.run(&["list", "--json"])?;
    let tasks: Value = serde_json::from_str(&listed.stdout)?;
    assert_eq!(tasks[1]["id"], 4);
    assert_eq!(tasks[1]["title"], "Full");
    assert_eq!(tasks[1]["body"], "text");
    assert_eq!(tasks[1]["kind"], "human");
    assert_eq!(tasks[1]["links"], json!(["https://example.com"]));
    assert_eq!(tasks[1]["criteria"], json!(["one"]));
    Ok(())
}

#[test]
fn before_and_after_together_exit_two_naming_both_and_add_nothing() -> Result<()> {
    let fixture = abc()?;
    let events = fixture.rows("events")?.len();

    let outcome = fixture.run(&[
        "add",
        "--title",
        "t",
        "--criterion",
        "c",
        "--before",
        "1",
        "--after",
        "2",
    ])?;

    assert_usage_error(&outcome, &["--before", "--after"]);
    assert_eq!(queue(&fixture)?, ["1:a", "2:b", "3:c"]);
    assert_eq!(fixture.rows("events")?.len(), events);
    Ok(())
}

#[test]
fn an_unknown_id_exits_two_naming_it_and_adds_nothing() -> Result<()> {
    let fixture = abc()?;
    let events = fixture.rows("events")?.len();

    for (flag, id) in [("--before", "9"), ("--after", "9"), ("--before", "0")] {
        let outcome = add_placed(&fixture, "t", flag, id)?;
        assert_usage_error(&outcome, &[&format!("no task {id}"), "ktask-rs list"]);
    }

    assert_eq!(queue(&fixture)?, ["1:a", "2:b", "3:c"]);
    assert_eq!(fixture.rows("events")?.len(), events);
    Ok(())
}

#[test]
fn an_id_in_an_empty_queue_is_unknown() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = add_placed(&fixture, "t", "--after", "1")?;
    assert_usage_error(&outcome, &["no task 1"]);
    fixture.assert_nothing_added()
}

#[test]
fn a_cancelled_id_exits_two_naming_it_and_adds_nothing() -> Result<()> {
    let fixture = abc()?;
    let removed = fixture.run(&["remove", "2"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);
    let events = fixture.rows("events")?.len();

    for flag in ["--before", "--after"] {
        let outcome = add_placed(&fixture, "t", flag, "2")?;
        assert_usage_error(&outcome, &["task 2 is cancelled"]);
    }

    assert_eq!(queue(&fixture)?, ["1:a", "3:c"]);
    assert_eq!(fixture.rows("events")?.len(), events);
    // Its neighbours are still fine to place next to.
    add_placed(&fixture, "new", "--after", "1")?;
    assert_eq!(queue(&fixture)?, ["1:a", "4:new", "3:c"]);
    Ok(())
}

#[test]
fn an_id_that_is_not_a_number_exits_two_naming_the_option_and_adds_nothing() -> Result<()> {
    let fixture = abc()?;
    for (flag, id) in [("--before", "second"), ("--after", "1.5"), ("--after", "")] {
        let outcome = add_placed(&fixture, "t", flag, id)?;
        assert_usage_error(&outcome, &[flag]);
    }
    assert_eq!(queue(&fixture)?, ["1:a", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn a_task_that_breaks_a_rule_is_refused_before_its_place_is_looked_up() -> Result<()> {
    let fixture = abc()?;
    let outcome = fixture.run(&["add", "--title", "", "--criterion", "c", "--before", "9"])?;
    assert_usage_error(&outcome, &["title is empty"]);
    assert_eq!(queue(&fixture)?, ["1:a", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn before_and_after_work_with_project_from_any_directory() -> Result<()> {
    let fixture = abc()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    let shown = fixture.sandbox.run(&other, &["project", "show"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);

    let added = fixture.sandbox.run(
        &other,
        &[
            "add",
            "--project",
            "my-app",
            "--title",
            "remote",
            "--criterion",
            "c",
            "--before",
            "1",
        ],
    )?;

    assert_eq!(added.stdout, "4\n", "{}", added.stderr);
    assert_eq!(queue(&fixture)?, ["4:remote", "1:a", "2:b", "3:c"]);
    // The other project has no task 1 to place next to.
    let unknown = fixture.sandbox.run(
        &other,
        &["add", "--title", "t", "--criterion", "c", "--before", "1"],
    )?;
    assert_usage_error(&unknown, &["no task 1"]);
    Ok(())
}

#[test]
fn before_and_after_are_in_the_add_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let add = fixture.run(&["add", "--help"])?;
    assert_eq!(add.code, Some(0));
    for option in ["--before <ID>", "--after <ID>"] {
        assert!(add.stdout.contains(option), "{option}: {}", add.stdout);
    }
    Ok(())
}
