//! `ktask-rs import` on the real binary, against the journal it keeps.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

    /// Runs `ktask-rs` with `args` inside the repository, with `input` on standard input.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Result<Outcome> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("no standard input")?
            .write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        Ok(Outcome {
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
            code: output.status.code(),
        })
    }

    /// Writes `text` to a file outside the repository and returns its path.
    fn file(&self, name: &str, text: &str) -> Result<String> {
        let path = self.work.join(name);
        std::fs::write(&path, text)?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Imports the JSON `tasks` from a file, with `options` after the file name.
    fn import(&self, tasks: &Value, options: &[&str]) -> Result<Outcome> {
        let file = self.file("tasks.json", &tasks.to_string())?;
        let mut args = vec!["import", file.as_str()];
        args.extend_from_slice(options);
        self.run(&args)
    }

    fn add(&self, title: &str) -> Result<()> {
        let outcome = self.run(&["add", "--title", title, "--criterion", "it works"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// The tasks `list --json` prints for the repository.
    fn listed(&self) -> Result<Vec<Value>> {
        let outcome = self.run(&["list", "--json"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(serde_json::from_str(&outcome.stdout)?)
    }

    /// The titles of the queue with the ID of each, in order: `["2:a", "1:b"]`.
    fn queue(&self) -> Result<Vec<String>> {
        Ok(self
            .listed()?
            .iter()
            .map(|task| format!("{}:{}", task["id"], task["title"].as_str().unwrap_or("")))
            .collect())
    }

    /// The payloads of the events in the journal of `my-app`, oldest first.
    fn event_payloads(&self) -> Result<Vec<Value>> {
        let path = self
            .sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("journal.db");
        let database = rusqlite::Connection::open(path)?;
        let mut statement = database.prepare("SELECT payload FROM events ORDER BY seq")?;
        let payloads = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        payloads
            .iter()
            .map(|payload| Ok(serde_json::from_str(payload)?))
            .collect()
    }
}

fn task(title: &str) -> Value {
    json!({ "title": title, "criteria": ["it works"] })
}

/// `listed`, reduced to the fields an import takes.
fn authored(listed: &[Value]) -> Vec<Value> {
    listed
        .iter()
        .map(|task| {
            json!({
                "title": task["title"],
                "body": task["body"],
                "criteria": task["criteria"],
                "kind": task["kind"],
                "links": task["links"],
            })
        })
        .collect()
}

fn assert_refused(outcome: &Outcome, naming: &[&str]) {
    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    for name in naming {
        assert!(outcome.stderr.contains(name), "{name}: {}", outcome.stderr);
    }
}

#[test]
fn a_valid_file_of_three_tasks_adds_all_three_in_order_and_prints_their_ids() -> Result<()> {
    let fixture = Fixture::new()?;
    let tasks = json!([
        {"title": "First", "criteria": ["one"]},
        {"title": "Second", "body": "with\na body", "criteria": ["a", "b"], "kind": "human",
         "links": ["github:owner/repo#7", "https://example.com/x"]},
        {"title": "Third", "criteria": ["three"]},
    ]);

    let outcome = fixture.import(&tasks, &[])?;

    assert_eq!(outcome.stdout, "1\n2\n3\n");
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let listed = fixture.listed()?;
    assert_eq!(
        authored(&listed),
        [
            json!({"title": "First", "body": "", "criteria": ["one"], "kind": "agent",
                   "links": []}),
            json!({"title": "Second", "body": "with\na body", "criteria": ["a", "b"],
                   "kind": "human", "links": ["github:owner/repo#7", "https://example.com/x"]}),
            json!({"title": "Third", "body": "", "criteria": ["three"], "kind": "agent",
                   "links": []}),
        ]
    );
    let ids: Vec<_> = listed
        .iter()
        .map(|t| (t["id"].clone(), t["position"].clone(), t["status"].clone()))
        .collect();
    assert_eq!(
        ids,
        [
            (json!(1), json!(1), json!("pending")),
            (json!(2), json!(2), json!("pending")),
            (json!(3), json!(3), json!("pending"))
        ]
    );
    let events = fixture.event_payloads()?;
    assert_eq!(events.len(), 3, "one event for each task");
    assert_eq!(events[1]["title"], "Second");
    Ok(())
}

#[test]
fn what_is_left_out_of_a_task_takes_its_default() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.import(&json!([task("Plain")]), &[])?;

    assert_eq!(outcome.stdout, "1\n");
    assert_eq!(
        authored(&fixture.listed()?),
        [
            json!({"title": "Plain", "body": "", "criteria": ["it works"], "kind": "agent",
                "links": []})
        ]
    );
    Ok(())
}

#[test]
fn an_empty_array_adds_nothing_prints_nothing_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;

    let outcome = fixture.import(&json!([]), &[])?;

    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.queue()?, ["1:Existing"]);
    Ok(())
}

#[test]
fn imported_tasks_get_the_next_ids_after_the_tasks_already_there() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;

    let outcome = fixture.import(&json!([task("a"), task("b")]), &[])?;

    assert_eq!(outcome.stdout, "2\n3\n");
    assert_eq!(fixture.queue()?, ["1:Existing", "2:a", "3:b"]);
    Ok(())
}

/// A queue of `a`, `b`, `c` (IDs 1 to 3), and the outcome of importing `x` and `y` with
/// `options`.
fn import_xy_into_abc(options: &[&str]) -> Result<(Fixture, Outcome)> {
    let fixture = Fixture::new()?;
    for title in ["a", "b", "c"] {
        fixture.add(title)?;
    }
    let outcome = fixture.import(&json!([task("x"), task("y")]), options)?;
    Ok((fixture, outcome))
}

#[test]
fn before_puts_the_whole_batch_immediately_before_the_task_named_in_order() -> Result<()> {
    for (id, expected) in [
        ("1", ["4:x", "5:y", "1:a", "2:b", "3:c"]),
        ("2", ["1:a", "4:x", "5:y", "2:b", "3:c"]),
        ("3", ["1:a", "2:b", "4:x", "5:y", "3:c"]),
    ] {
        let (fixture, outcome) = import_xy_into_abc(&["--before", id])?;
        assert_eq!(outcome.stdout, "4\n5\n", "--before {id}");
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        assert_eq!(fixture.queue()?, expected, "--before {id}");
    }
    Ok(())
}

#[test]
fn after_puts_the_whole_batch_immediately_after_the_task_named_in_order() -> Result<()> {
    for (id, expected) in [
        ("1", ["1:a", "4:x", "5:y", "2:b", "3:c"]),
        ("2", ["1:a", "2:b", "4:x", "5:y", "3:c"]),
        ("3", ["1:a", "2:b", "3:c", "4:x", "5:y"]),
    ] {
        let (fixture, outcome) = import_xy_into_abc(&["--after", id])?;
        assert_eq!(outcome.stdout, "4\n5\n", "--after {id}");
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        assert_eq!(fixture.queue()?, expected, "--after {id}");
    }
    Ok(())
}

#[test]
fn the_events_of_a_placed_batch_record_where_each_task_went() -> Result<()> {
    let (fixture, _) = import_xy_into_abc(&["--before", "2"])?;

    let events = fixture.event_payloads()?;

    assert_eq!(events.len(), 5);
    assert_eq!(
        (&events[3]["title"], &events[3]["before"]),
        (&json!("x"), &json!(2))
    );
    assert_eq!(
        (&events[4]["title"], &events[4]["after"]),
        (&json!("y"), &json!(4))
    );
    assert_eq!(events[4].get("before"), None);
    Ok(())
}

#[test]
fn a_place_that_does_not_exist_or_is_given_twice_exits_two_and_adds_nothing() -> Result<()> {
    for (options, naming) in [
        (&["--before", "9"][..], "there is no task 9"),
        (&["--after", "9"], "there is no task 9"),
        (&["--before", "0"], "there is no task 0"),
        (&["--before", "1", "--after", "2"], "cannot be used with"),
        (&["--before", "first"], "invalid value"),
    ] {
        let (fixture, outcome) = import_xy_into_abc(options)?;
        assert_refused(&outcome, &[naming]);
        assert_eq!(fixture.queue()?, ["1:a", "2:b", "3:c"], "{options:?}");
        assert_eq!(fixture.event_payloads()?.len(), 3, "{options:?}");
    }
    Ok(())
}

#[test]
fn a_file_with_one_invalid_task_adds_nothing_exits_two_and_names_the_task_and_problem() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;
    let tasks = json!([task("fine"), {"title": "  ", "criteria": ["c"]}, task("also fine")]);

    let outcome = fixture.import(&tasks, &[])?;

    assert_refused(
        &outcome,
        &["1 task is invalid", "task 2:", "the title is empty"],
    );
    assert!(!outcome.stderr.contains("task 1:"), "{}", outcome.stderr);
    assert!(!outcome.stderr.contains("task 3:"), "{}", outcome.stderr);
    assert_eq!(fixture.queue()?, ["1:Existing"]);
    assert_eq!(fixture.event_payloads()?.len(), 1, "no event was recorded");
    Ok(())
}

#[test]
fn every_invalid_task_is_named_by_its_index_with_every_problem_it_has() -> Result<()> {
    let fixture = Fixture::new()?;
    let tasks = json!([
        {"title": "no criteria"},
        task("fine"),
        {"title": "bad", "criteria": ["c", ""], "kind": "robot", "links": ["nonsense"]},
        {"title": "extra field", "criteria": ["c"], "status": "done"},
        {"title": 5, "criteria": ["c"]},
        "just text",
    ]);

    let outcome = fixture.import(&tasks, &[])?;

    assert_refused(
        &outcome,
        &[
            "5 tasks are invalid",
            "task 1: a task needs at least one acceptance criterion",
            "task 3: unknown kind \"robot\"",
            "task 3: an acceptance criterion is empty",
            "task 3: malformed link \"nonsense\"",
            "task 4: unknown field `status`",
            "task 5: invalid type: integer `5`, expected a string",
            "task 6: invalid type: string \"just text\"",
        ],
    );
    assert!(!outcome.stderr.contains("task 2:"), "{}", outcome.stderr);
    assert_eq!(fixture.listed()?, Vec::<Value>::new());
    assert_eq!(fixture.event_payloads()?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn a_placement_error_and_an_invalid_task_together_still_add_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;

    let outcome = fixture.import(
        &json!([task("ok"), {"title": "no criteria"}]),
        &["--after", "1"],
    )?;

    assert_refused(&outcome, &["task 2:"]);
    assert_eq!(fixture.queue()?, ["1:Existing"]);
    Ok(())
}

#[test]
fn malformed_json_exits_two_with_the_location_of_the_parse_error_and_adds_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let file = fixture.file(
        "broken.json",
        "[\n  {\"title\": \"a\",\n   \"criteria\": }\n]",
    )?;

    let outcome = fixture.run(&["import", &file])?;

    assert_refused(&outcome, &["not valid JSON", "line 3 column 16"]);
    assert_eq!(fixture.listed()?, Vec::<Value>::new());
    assert_eq!(fixture.event_payloads()?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn text_that_is_not_a_json_array_of_tasks_exits_two() -> Result<()> {
    let fixture = Fixture::new()?;
    for text in [
        "{\"title\": \"a\", \"criteria\": [\"c\"]}",
        "\"text\"",
        "7",
        "null",
    ] {
        let file = fixture.file("not-an-array.json", text)?;
        let outcome = fixture.run(&["import", &file])?;
        assert_refused(&outcome, &["expected a JSON array of tasks"]);
    }
    for text in ["", "   \n", "[", "[1,]", "[{}] trailing"] {
        let file = fixture.file("not-json.json", text)?;
        let outcome = fixture.run(&["import", &file])?;
        assert_refused(&outcome, &["not valid JSON", "line "]);
    }
    assert_eq!(fixture.listed()?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn a_dash_reads_the_tasks_from_standard_input() -> Result<()> {
    let fixture = Fixture::new()?;
    let tasks = json!([task("From stdin"), task("Also")]);

    let outcome = fixture.run_with_stdin(&["import", "-"], &tasks.to_string())?;

    assert_eq!(outcome.stdout, "1\n2\n");
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.queue()?, ["1:From stdin", "2:Also"]);
    Ok(())
}

#[test]
fn standard_input_that_is_invalid_is_refused_like_a_file() -> Result<()> {
    let fixture = Fixture::new()?;

    let broken = fixture.run_with_stdin(&["import", "-"], "[{")?;
    assert_refused(&broken, &["not valid JSON", "line 1 column 2"]);
    let empty = fixture.run_with_stdin(&["import", "-"], "")?;
    assert_refused(&empty, &["not valid JSON"]);
    let invalid = fixture.run_with_stdin(&["import", "-"], r#"[{"title": "x"}]"#)?;
    assert_refused(&invalid, &["task 1:"]);
    assert_eq!(fixture.listed()?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn a_file_that_cannot_be_read_exits_two_naming_it_and_registers_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let missing = fixture.work.join("missing.json");
    let missing = missing.to_string_lossy();

    let outcome = fixture.run(&["import", &missing])?;

    assert_refused(&outcome, &["cannot read", &missing]);
    assert_eq!(fixture.run(&["project", "list"])?.stdout, "");
    let binary = fixture.work.join("binary.json");
    std::fs::write(&binary, [0xff, 0xfe, 0x00])?;
    let outcome = fixture.run(&["import", &binary.to_string_lossy()])?;
    assert_refused(&outcome, &["cannot read"]);
    Ok(())
}

#[test]
fn import_without_a_file_is_a_usage_error() -> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = fixture.run(&["import"])?;
    assert_refused(&outcome, &["FILE"]);
    Ok(())
}

#[test]
fn the_output_of_list_json_reduced_to_authored_fields_imports_into_another_project() -> Result<()> {
    let fixture = Fixture::new()?;
    let full = fixture.run(&[
        "add",
        "--title",
        "Full",
        "--criterion",
        "one",
        "--criterion",
        "two",
        "--body",
        "Long\nbody",
        "--kind",
        "human",
        "--link",
        "github:o/r#1",
        "--link",
        "https://example.com",
    ])?;
    assert_eq!(full.code, Some(0), "{}", full.stderr);
    fixture.add("Plain")?;
    let listed = fixture.listed()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    let reduced = authored(&listed);

    let outcome = fixture.sandbox.run(
        &other,
        &[
            "import",
            &fixture.file("reduced.json", &json!(reduced).to_string())?,
        ],
    )?;

    assert_eq!(outcome.stdout, "1\n2\n");
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let imported = fixture.sandbox.run(&other, &["list", "--json"])?;
    let imported: Vec<Value> = serde_json::from_str(&imported.stdout)?;
    assert_eq!(authored(&imported), reduced);
    assert_eq!(imported[0]["kind"], "human");
    assert_eq!(imported[0]["body"], "Long\nbody");
    Ok(())
}

#[test]
fn the_unreduced_output_of_list_json_is_refused_naming_the_fields_that_are_not_authored()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Plain")?;
    let listed = fixture.run(&["list", "--json"])?;

    let outcome = fixture.run_with_stdin(&["import", "-"], &listed.stdout)?;

    assert_refused(
        &outcome,
        &["task 1: unknown field", "expected one of `title`, `body`"],
    );
    assert_eq!(fixture.queue()?, ["1:Plain"]);
    Ok(())
}

#[test]
fn project_selects_the_queue_the_tasks_are_imported_into() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    assert_eq!(
        fixture.sandbox.run(&other, &["project", "show"])?.code,
        Some(0)
    );

    let outcome = fixture.import(&json!([task("Elsewhere")]), &["--project", "other-app"])?;

    assert_eq!(outcome.stdout, "1\n");
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.listed()?, Vec::<Value>::new());
    let listed = fixture.sandbox.run(&other, &["list"])?;
    assert_eq!(listed.stdout, "1\t#1\tpending\tagent\tElsewhere\n");
    Ok(())
}

#[test]
fn the_import_help_describes_the_file_the_placement_and_the_fields() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["import", "--help"])?;
    assert_eq!(help.code, Some(0));
    for text in [
        "FILE",
        "-",
        "standard input",
        "--before",
        "--after",
        "--project",
        "criteria",
    ] {
        assert!(help.stdout.contains(text), "{text}: {}", help.stdout);
    }
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("import"), "{}", top.stdout);
    Ok(())
}
