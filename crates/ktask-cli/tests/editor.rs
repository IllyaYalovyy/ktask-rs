//! `ktask-rs add` with no content options, writing the task in `$EDITOR`, on the real binary.
//!
//! The editor is a small shell script that plays the part of a person: it copies the
//! template it was opened on to a file the test can read, replaces the file with prepared
//! text, and exits with a chosen status.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use repo::{git_repository, scratch};
use serde_json::{Value, json};
use support::{Outcome, Result, Sandbox};

const FAKE_EDITOR: &str = "\
#!/bin/sh
printf '%s\\n' \"$1\" > \"$FAKE_EDITOR_FILE_NAME\"
cp \"$1\" \"$FAKE_EDITOR_SAW\"
if [ -n \"$FAKE_EDITOR_WRITES\" ]; then cp \"$FAKE_EDITOR_WRITES\" \"$1\"; fi
exit \"${FAKE_EDITOR_EXIT:-0}\"
";

/// A sandbox with a git repository called `my-app` and a fake editor.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    work: PathBuf,
    editor: PathBuf,
    _keep: tempfile::TempDir,
}

/// What the fake editor does when it is opened.
#[derive(Debug, Default)]
struct Edit {
    /// The text it leaves in the file; the file is left as it was when `None`.
    writes: Option<String>,
    /// The status it exits with.
    exit: u8,
}

impl Edit {
    /// Leaves `text` in the file and exits successfully.
    fn writing(text: impl Into<String>) -> Self {
        Self {
            writes: Some(text.into()),
            exit: 0,
        }
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let editor = work.join("fake-editor");
        std::fs::write(&editor, FAKE_EDITOR)?;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            sandbox,
            repository,
            work,
            editor,
            _keep: keep,
        })
    }

    fn scratch_file(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }

    /// The template the editor was last opened on.
    fn template_seen(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.scratch_file("saw"))?)
    }

    /// The path the editor was last given.
    fn file_name_seen(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(
            std::fs::read_to_string(self.scratch_file("file-name"))?.trim_end(),
        ))
    }

    fn was_opened(&self) -> bool {
        self.scratch_file("saw").exists()
    }

    /// Runs `ktask-rs add` with `args` in `cwd`, `$EDITOR` being the fake editor doing `edit`.
    fn add_in(&self, cwd: &Path, args: &[&str], edit: Edit) -> Result<Outcome> {
        let writes = self.scratch_file("writes");
        let Edit { writes: text, exit } = edit;
        if let Some(text) = &text {
            std::fs::write(&writes, text)?;
        }
        let mut all = vec!["add"];
        all.extend(args);
        self.sandbox.run_with(cwd, &all, |command| {
            command
                .env("EDITOR", &self.editor)
                .env("FAKE_EDITOR_SAW", self.scratch_file("saw"))
                .env("FAKE_EDITOR_FILE_NAME", self.scratch_file("file-name"))
                .env("FAKE_EDITOR_EXIT", exit.to_string());
            if text.is_some() {
                command.env("FAKE_EDITOR_WRITES", &writes);
            }
        })
    }

    fn add(&self, args: &[&str], edit: Edit) -> Result<Outcome> {
        self.add_in(&self.repository, args, edit)
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// The queue as `list --json` shows it.
    fn tasks(&self) -> Result<Vec<Value>> {
        let listed = self.run(&["list", "--json"])?;
        assert_eq!(listed.code, Some(0), "{}", listed.stderr);
        match serde_json::from_str(&listed.stdout)? {
            Value::Array(tasks) => Ok(tasks),
            other => Err(format!("list --json is not an array: {other}").into()),
        }
    }

    /// The queue as `id:title` pairs in order.
    fn queue(&self) -> Result<Vec<String>> {
        Ok(self
            .tasks()?
            .iter()
            .map(|task| format!("{}:{}", task["id"], task["title"].as_str().unwrap_or("")))
            .collect())
    }

    /// The number of events in the journal of `my-app`.
    fn events(&self) -> Result<i64> {
        let path = self
            .sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("journal.db");
        let database = rusqlite::Connection::open(path)?;
        Ok(database.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
    }

    /// Asserts that nothing was added: the queue is empty and no event was recorded.
    fn assert_nothing_added(&self) -> Result<()> {
        assert_eq!(self.tasks()?, Vec::<Value>::new(), "a task was added");
        assert_eq!(self.events()?, 0, "an event was recorded");
        Ok(())
    }

    /// Adds `a`, `b` and `c` with the options, numbered 1, 2, 3.
    fn abc(&self) -> Result<()> {
        for title in ["a", "b", "c"] {
            let added = self.run(&["add", "--title", title, "--criterion", "it works"])?;
            assert_eq!(added.code, Some(0), "{}", added.stderr);
        }
        Ok(())
    }
}

/// A template filled in with a task that has the given title and one criterion.
fn task_text(title: &str) -> String {
    format!("Title: {title}\nCriteria:\n- it works\n")
}

const FULL: &str = "\
# Anything above the title is ignored.
Title: Write the parser
Kind: human
Links:
- github:owner/repo#12
- https://example.com/design
Body:
It reads the input.

  Then it stops.
Criteria:
- it reads a file
- it reports the line of an error
";

#[test]
fn a_filled_in_template_adds_the_task_with_every_field_as_written() -> Result<()> {
    let fixture = Fixture::new()?;

    let added = fixture.add(&[], Edit::writing(FULL))?;

    assert_eq!(added.stdout, "1\n", "{}", added.stderr);
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let tasks = fixture.tasks()?;
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task["id"], 1);
    assert_eq!(task["position"], 1);
    assert_eq!(task["title"], "Write the parser");
    assert_eq!(task["kind"], "human");
    assert_eq!(task["status"], "pending");
    assert_eq!(
        task["links"],
        json!(["github:owner/repo#12", "https://example.com/design"])
    );
    assert_eq!(task["body"], "It reads the input.\n\n  Then it stops.");
    assert_eq!(
        task["criteria"],
        json!(["it reads a file", "it reports the line of an error"])
    );
    let listed = fixture.run(&["list"])?;
    assert_eq!(listed.stdout, "1\t#1\tpending\thuman\tWrite the parser\n");
    Ok(())
}

#[test]
fn a_template_with_only_a_title_and_a_criterion_gets_kind_agent_no_links_and_no_body() -> Result<()>
{
    let fixture = Fixture::new()?;
    let added = fixture.add(&[], Edit::writing(task_text("Plain")))?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let tasks = fixture.tasks()?;
    assert_eq!(tasks[0]["kind"], "agent");
    assert_eq!(tasks[0]["links"], json!([]));
    assert_eq!(tasks[0]["body"], "");
    assert_eq!(tasks[0]["criteria"], json!(["it works"]));
    Ok(())
}

#[test]
fn the_editor_is_opened_on_a_template_with_every_field_in_a_file_in_tmpdir_that_is_removed()
-> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.add(&[], Edit::writing(task_text("t")))?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let template = fixture.template_seen()?;
    for header in ["Title:", "Kind: agent", "Links:", "Body:", "Criteria:"] {
        assert!(
            template.lines().any(|line| line == header),
            "{header}: {template}"
        );
    }
    let file = fixture.file_name_seen()?;
    assert!(
        file.starts_with(fixture.sandbox.tmpdir()),
        "{} is not in the sandbox's TMPDIR",
        file.display()
    );
    assert!(!file.exists(), "{} was left behind", file.display());
    Ok(())
}

#[test]
fn an_unchanged_template_adds_nothing_says_so_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.add(&[], Edit::default())?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome
            .stderr
            .contains("nothing added: the template was left unchanged"),
        "{}",
        outcome.stderr
    );
    assert!(fixture.was_opened());
    fixture.assert_nothing_added()
}

#[test]
fn an_emptied_template_adds_nothing_says_so_and_exits_zero() -> Result<()> {
    let fixture = Fixture::new()?;

    for text in ["", "\n\n  \n"] {
        let outcome = fixture.add(&[], Edit::writing(text))?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome
                .stderr
                .contains("nothing added: the template was emptied"),
            "{}",
            outcome.stderr
        );
    }
    fixture.assert_nothing_added()
}

#[test]
fn a_template_without_a_title_or_a_criterion_exits_two_naming_both_and_adds_nothing() -> Result<()>
{
    let fixture = Fixture::new()?;

    let outcome = fixture.add(
        &[],
        Edit::writing("Title:\nKind: human\nBody: only a body\nCriteria:\n"),
    )?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("title is empty"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("at least one acceptance criterion"),
        "{}",
        outcome.stderr
    );
    fixture.assert_nothing_added()
}

#[test]
fn a_missing_title_alone_and_a_missing_criterion_alone_are_each_named() -> Result<()> {
    let fixture = Fixture::new()?;
    let edit = |text| Edit::writing(text);

    let no_title = fixture.add(&[], edit("Title:\nCriteria:\n- c\n"))?;
    assert_eq!(no_title.code, Some(2), "{}", no_title.stderr);
    assert!(no_title.stderr.contains("title is empty"));
    assert!(
        !no_title.stderr.contains("criterion"),
        "{}",
        no_title.stderr
    );

    let no_criterion = fixture.add(&[], edit("Title: t\nCriteria:\n-\n"))?;
    assert_eq!(no_criterion.code, Some(2), "{}", no_criterion.stderr);
    assert!(
        no_criterion
            .stderr
            .contains("at least one acceptance criterion")
    );
    assert!(
        !no_criterion.stderr.contains("title"),
        "{}",
        no_criterion.stderr
    );
    fixture.assert_nothing_added()
}

#[test]
fn every_problem_in_the_template_is_named_at_once() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.add(
        &[],
        Edit::writing(
                "Title:\nKind: robot\nLinks:\n- not a link\n- https://fine.example\n- ftp://x\nCriteria:\n",
            ),
    )?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    for named in [
        "title is empty",
        "at least one acceptance criterion",
        "unknown kind \"robot\"",
        "malformed link \"not a link\"",
        "malformed link \"ftp://x\"",
    ] {
        assert!(
            outcome.stderr.contains(named),
            "{named}: {}",
            outcome.stderr
        );
    }
    assert!(
        !outcome.stderr.contains("fine.example"),
        "{}",
        outcome.stderr
    );
    fixture.assert_nothing_added()
}

#[test]
fn an_editor_that_exits_non_zero_adds_nothing_and_exits_two() -> Result<()> {
    let fixture = Fixture::new()?;

    // Even when it left a perfectly good task behind.
    let outcome = fixture.add(
        &[],
        Edit {
            exit: 3,
            ..Edit::writing(task_text("t"))
        },
    )?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("editor"), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("exit status: 3"),
        "{}",
        outcome.stderr
    );
    fixture.assert_nothing_added()
}

#[test]
fn an_editor_that_does_not_exist_adds_nothing_and_exits_two() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["add"], |command| {
            command.env("EDITOR", "no-such-editor-anywhere");
        })?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("no-such-editor-anywhere"),
        "{}",
        outcome.stderr
    );
    fixture.assert_nothing_added()
}

#[test]
fn an_unset_or_empty_editor_exits_two_saying_how_to_set_it_or_use_the_options() -> Result<()> {
    let fixture = Fixture::new()?;

    let unset = fixture.run(&["add"])?;
    let empty = fixture
        .sandbox
        .run_with(&fixture.repository, &["add"], |command| {
            command.env("EDITOR", "");
        })?;

    for outcome in [unset, empty] {
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert_eq!(outcome.stdout, "");
        for named in ["$EDITOR is not set", "EDITOR=vi", "--title", "--criterion"] {
            assert!(
                outcome.stderr.contains(named),
                "{named}: {}",
                outcome.stderr
            );
        }
    }
    assert!(!fixture.was_opened());
    fixture.assert_nothing_added()
}

#[test]
fn before_and_after_place_the_task_written_in_the_editor() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.abc()?;
    let write = |title: &str| Edit::writing(task_text(title));

    let before = fixture.add(&["--before", "2"], write("before b"))?;
    assert_eq!(before.stdout, "4\n", "{}", before.stderr);
    assert_eq!(fixture.queue()?, ["1:a", "4:before b", "2:b", "3:c"]);

    let after = fixture.add(&["--after", "3"], write("after c"))?;
    assert_eq!(after.stdout, "5\n", "{}", after.stderr);
    let first = fixture.add(&["--before", "1"], write("first"))?;
    assert_eq!(first.stdout, "6\n", "{}", first.stderr);
    assert_eq!(
        fixture.queue()?,
        ["6:first", "1:a", "4:before b", "2:b", "3:c", "5:after c"]
    );
    Ok(())
}

#[test]
fn a_place_that_does_not_exist_exits_two_before_the_editor_opens() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.abc()?;

    for flag in ["--before", "--after"] {
        let outcome = fixture.add(&[flag, "9"], Edit::writing(task_text("t")))?;
        assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
        assert!(outcome.stderr.contains("no task 9"), "{}", outcome.stderr);
        assert!(!fixture.was_opened(), "the editor was opened for {flag} 9");
    }
    assert_eq!(fixture.queue()?, ["1:a", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn before_and_after_together_are_a_usage_error_and_the_editor_is_not_opened() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.abc()?;

    let outcome = fixture.add(&["--before", "1", "--after", "2"], Edit::default())?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("--before"), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("--after"), "{}", outcome.stderr);
    assert!(!fixture.was_opened());
    Ok(())
}

#[test]
fn an_invalid_template_placed_before_a_task_adds_nothing_and_moves_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.abc()?;

    let outcome = fixture.add(&["--before", "2"], Edit::writing("Title:\nCriteria:\n"))?;

    assert_eq!(outcome.code, Some(2), "{}", outcome.stderr);
    assert_eq!(fixture.queue()?, ["1:a", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn project_selects_the_queue_the_editor_task_is_added_to() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.abc()?;
    let other = git_repository(&fixture.sandbox, &fixture.work, "other-app")?;
    let shown = fixture.sandbox.run(&other, &["project", "show"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);

    let added = fixture.add_in(
        &other,
        &["--project", "my-app", "--after", "1"],
        Edit::writing(task_text("remote")),
    )?;

    assert_eq!(added.stdout, "4\n", "{}", added.stderr);
    assert_eq!(fixture.queue()?, ["1:a", "4:remote", "2:b", "3:c"]);
    Ok(())
}

#[test]
fn content_options_add_the_task_without_opening_the_editor() -> Result<()> {
    let fixture = Fixture::new()?;

    let added = fixture.add(
        &["--title", "From options", "--criterion", "c"],
        Edit {
            exit: 1,
            ..Edit::writing("Title:\n")
        },
    )?;

    assert_eq!(added.stdout, "1\n", "{}", added.stderr);
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert!(!fixture.was_opened());
    assert_eq!(fixture.queue()?, ["1:From options"]);
    Ok(())
}

#[test]
fn a_content_option_without_a_title_is_a_usage_error_naming_the_title_and_no_editor_opens()
-> Result<()> {
    let fixture = Fixture::new()?;
    for option in [
        &["--criterion", "c"][..],
        &["--body", "b"],
        &["--kind", "human"],
        &["--link", "https://example.com"],
    ] {
        let outcome = fixture.add(option, Edit::default())?;
        assert_eq!(outcome.code, Some(2), "{option:?}: {}", outcome.stderr);
        assert!(outcome.stderr.contains("--title"), "{}", outcome.stderr);
    }
    let no_criterion = fixture.add(&["--title", "t"], Edit::default())?;
    assert_eq!(no_criterion.code, Some(2), "{}", no_criterion.stderr);
    assert!(no_criterion.stderr.contains("--criterion"));
    assert!(!fixture.was_opened());
    fixture.assert_nothing_added()
}

#[test]
fn the_add_help_says_the_editor_is_used_without_content_options() -> Result<()> {
    let fixture = Fixture::new()?;
    let help = fixture.run(&["add", "--help"])?;
    assert_eq!(help.code, Some(0));
    assert!(help.stdout.contains("$EDITOR"), "{}", help.stdout);
    Ok(())
}

#[test]
fn an_editor_command_with_arguments_is_started_the_way_git_starts_it() -> Result<()> {
    let fixture = Fixture::new()?;
    // This editor takes the file as its last argument, as real editors do.
    let script = fixture.scratch_file("arg-editor");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' \"$1\" > \"$FAKE_EDITOR_SAW\"\nshift\nprintf 'Title: t\\nCriteria:\\n- c\\n' > \"$1\"\n",
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

    let outcome = fixture
        .sandbox
        .run_with(&fixture.repository, &["add"], |command| {
            command
                .env("EDITOR", format!("{} first-argument", script.display()))
                .env("FAKE_EDITOR_SAW", fixture.scratch_file("saw"));
        })?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.template_seen()?, "first-argument");
    assert_eq!(fixture.queue()?, ["1:t"]);
    Ok(())
}
