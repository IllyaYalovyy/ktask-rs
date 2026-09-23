//! `ktask-rs add`: opens `$EDITOR` with a task template, or reads `--file`.
//!
//! `docs/CONTRACT.md` section 3: the submitted content is parsed the same
//! way a plan document is (`ktask_core::parse_plan`), every resulting task
//! is validated (`ktask_core::validate`) before any of them touch the
//! journal, and only if every one is valid are they inserted, in one
//! [`Journal::put_tasks`] call — so a malformed submission never partially
//! enters the queue. `docs/DESIGN.md`'s database schema section documents
//! `add --file <plan.md>` as importing a plan's blocks "once"; that is
//! [`Journal::put_tasks`]'s own rule, not something enforced again here: it
//! refuses to insert into a non-empty queue, and that refusal surfaces
//! through the same [`RunOutcome::Usage`] path as a malformed task.

use ktask_core::{Config, Journal, Project, RunOutcome, Task, parse_plan, validate};
use std::env;
use std::path::Path;

use super::editor;
use crate::render;

/// The template written into the scratch file `$EDITOR` opens: one task
/// heading and the four required sections, empty and ready to fill in.
const TASK_TEMPLATE: &str = "\
## One-line summary of the task

**Outcome:**

**Done-when:**

**Verify:**

**Refs:**
";

/// The scratch file's name within the project's state directory.
const SCRATCH_FILE_NAME: &str = "add-draft.md";

/// Reads the task (or tasks) to add from `file` if given, or from `$EDITOR`
/// otherwise; validates every one; and, only if all are valid, inserts them
/// into `project`'s queue in one [`Journal::put_tasks`] call, printing each
/// assigned id.
///
/// A malformed submission, a missing `$EDITOR`, an editor that exits
/// nonzero, or a queue that already has tasks in it (`Journal::put_tasks`'s
/// own "no merge" rule) are all reported as [`RunOutcome::Usage`] — the
/// queue is left exactly as it was, since nothing is written to the journal
/// until every submitted task has already passed [`validate`].
pub(crate) fn run(project: &Project, _config: &Config, file: Option<&Path>) -> RunOutcome {
    let content = match file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) => {
                return RunOutcome::Usage {
                    detail: format!("add: could not read {}: {err}", path.display()),
                };
            }
        },
        None => match edit_via_editor(&project.state_dir, &|key| env::var(key)) {
            Ok(content) => content,
            Err(detail) => return RunOutcome::Usage { detail },
        },
    };

    match add_content(project, &content) {
        Ok(tasks) => {
            for task in &tasks {
                render::out(format_args!("task {} added: {}", task.id, task.title()));
            }
            RunOutcome::Drained
        }
        Err(detail) => RunOutcome::Usage { detail },
    }
}

/// Parses `content`, validates every task it yields, and — only if every one
/// is valid — inserts them into `project`'s queue. Returns the inserted
/// tasks, in the order [`Journal::put_tasks`] recorded them, so the caller
/// can report each assigned id.
fn add_content(project: &Project, content: &str) -> Result<Vec<Task>, String> {
    let tasks = parse_plan(content).map_err(|err| format!("add: {err}"))?;

    if tasks.is_empty() {
        return Err(
            "add: no task found in the submitted content (expected a `## ` heading)".to_string(),
        );
    }

    let mut problems = Vec::new();
    for task in &tasks {
        if let Err(err) = validate(task) {
            problems.push(format!("task {:?}: {err}", task.title()));
        }
    }
    if !problems.is_empty() {
        return Err(format!("add: {}", problems.join("; ")));
    }

    let mut journal = Journal::open_for(project)
        .map_err(|err| format!("add: could not open the journal: {err}"))?;
    journal
        .put_tasks(&tasks)
        .map_err(|err| format!("add: {err}"))?;

    Ok(tasks)
}

/// Opens [`TASK_TEMPLATE`] in `$EDITOR` (read through `env_var`, so a test
/// can inject one without touching the real process environment) and returns
/// what the editor left behind once it exits successfully.
///
/// # Errors
///
/// A clear message, not a panic, when `$EDITOR` is unset, cannot be
/// spawned, or exits with a non-success status, and when the scratch file
/// cannot be written to or read back.
fn edit_via_editor(
    state_dir: &Path,
    env_var: &dyn Fn(&str) -> Result<String, env::VarError>,
) -> Result<String, String> {
    editor::edit(
        "add",
        "--file",
        SCRATCH_FILE_NAME,
        TASK_TEMPLATE,
        state_dir,
        env_var,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::TaskStatus;
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    fn journal_project(state_dir: &Path) -> Project {
        Project {
            root: PathBuf::from("/repo"),
            id: "add-fixture".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn complete_task_markdown(title: &str) -> String {
        format!(
            "\
## {title}

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
"
        )
    }

    // -- add_content --------------------------------------------------------

    #[test]
    fn add_content_inserts_a_valid_single_task_and_returns_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());

        let tasks = add_content(&project, &complete_task_markdown("Do the thing"))
            .expect("valid content is accepted");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title(), "Do the thing");
        assert_eq!(tasks[0].status, TaskStatus::Pending);

        let journal = Journal::open_for(&project).expect("open journal");
        let stored = journal.tasks().expect("tasks");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].title(), "Do the thing");
    }

    #[test]
    fn add_content_rejects_a_task_missing_sections_and_names_every_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());

        let malformed = "\
## Incomplete task

**Outcome:** it happens.
";
        let err = add_content(&project, malformed).expect_err("malformed content is rejected");
        assert!(err.contains("Done-when"), "{err}");
        assert!(err.contains("Verify"), "{err}");
        assert!(err.contains("Refs"), "{err}");
        assert!(!err.contains("Outcome"), "{err}");

        let journal = Journal::open_for(&project).expect("open journal");
        assert!(
            journal.tasks().expect("tasks").is_empty(),
            "queue must be unchanged after a rejected task"
        );
    }

    #[test]
    fn add_content_rejects_content_with_no_task_heading_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());

        let err = add_content(&project, "just some prose, no heading")
            .expect_err("headless content is rejected");
        assert!(err.contains("no task found"), "{err}");

        let journal = Journal::open_for(&project).expect("open journal");
        assert!(journal.tasks().expect("tasks").is_empty());
    }

    #[test]
    fn add_content_rejects_a_second_submission_into_an_already_populated_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());

        add_content(&project, &complete_task_markdown("First task")).expect("first add succeeds");

        let err = add_content(&project, &complete_task_markdown("Second task"))
            .expect_err("a second add into a non-empty queue is refused");
        assert!(err.contains("add:"), "{err}");

        let journal = Journal::open_for(&project).expect("open journal");
        let stored = journal.tasks().expect("tasks");
        assert_eq!(stored.len(), 1, "the second, rejected task must not appear");
        assert_eq!(stored[0].title(), "First task");
    }

    #[test]
    fn add_content_leaves_no_task_with_an_unknown_protocol_in_the_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());

        let plan = "\
## Bad protocol

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none

**Protocol:** waterfall
";
        let err = add_content(&project, plan).expect_err("unknown protocol is rejected");
        assert!(err.contains("waterfall"), "{err}");

        let journal = Journal::open_for(&project).expect("open journal");
        assert!(journal.tasks().expect("tasks").is_empty());
    }

    // -- run (--file) ---------------------------------------------------------

    #[test]
    fn run_with_file_inserts_a_valid_task_and_prints_its_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let file_path = dir.path().join("task.md");
        std::fs::write(&file_path, complete_task_markdown("From a file")).expect("write task file");

        let outcome = run(&project, &Config::default(), Some(&file_path));
        assert_eq!(outcome, RunOutcome::Drained);

        let journal = Journal::open_for(&project).expect("open journal");
        assert_eq!(journal.tasks().expect("tasks").len(), 1);
    }

    #[test]
    fn run_with_file_reports_usage_for_a_malformed_task_and_leaves_the_queue_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let file_path = dir.path().join("task.md");
        std::fs::write(&file_path, "## Bare\n\nno sections here\n").expect("write task file");

        let outcome = run(&project, &Config::default(), Some(&file_path));
        assert!(matches!(outcome, RunOutcome::Usage { .. }), "{outcome:?}");
        if let RunOutcome::Usage { detail } = &outcome {
            for label in ["Outcome", "Done-when", "Verify", "Refs"] {
                assert!(detail.contains(label), "missing {label} in {detail:?}");
            }
        }

        let journal = Journal::open_for(&project).expect("open journal");
        assert!(journal.tasks().expect("tasks").is_empty());
    }

    #[test]
    fn run_with_file_reports_usage_when_the_file_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let missing = dir.path().join("nonexistent.md");

        let outcome = run(&project, &Config::default(), Some(&missing));
        assert!(matches!(outcome, RunOutcome::Usage { .. }), "{outcome:?}");
    }

    // -- edit_via_editor --------------------------------------------------------

    #[test]
    fn edit_via_editor_reports_a_clear_error_when_editor_is_unset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = edit_via_editor(dir.path(), &|_| Err(env::VarError::NotPresent))
            .expect_err("missing $EDITOR is an error");
        assert!(err.contains("EDITOR"), "{err}");
    }

    #[test]
    fn edit_via_editor_writes_the_template_and_returns_what_the_editor_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `cat` with no arguments beyond the scratch path just leaves the
        // file as-is; this proves the template itself is what a real editor
        // would see, and that its content round-trips back out.
        let content = edit_via_editor(dir.path(), &|key| {
            if key == "EDITOR" {
                Ok("true".to_string())
            } else {
                Err(env::VarError::NotPresent)
            }
        })
        .expect("editor runs successfully");
        assert_eq!(content, TASK_TEMPLATE);
    }

    #[test]
    fn edit_via_editor_removes_the_scratch_file_after_a_successful_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        edit_via_editor(dir.path(), &|key| {
            if key == "EDITOR" {
                Ok("true".to_string())
            } else {
                Err(env::VarError::NotPresent)
            }
        })
        .expect("editor runs successfully");
        assert!(!dir.path().join(SCRATCH_FILE_NAME).exists());
    }

    #[test]
    fn edit_via_editor_removes_the_scratch_file_even_when_the_editor_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = edit_via_editor(dir.path(), &|key| {
            if key == "EDITOR" {
                Ok("false".to_string())
            } else {
                Err(env::VarError::NotPresent)
            }
        });
        assert!(!dir.path().join(SCRATCH_FILE_NAME).exists());
    }

    #[test]
    fn edit_via_editor_reports_a_clear_error_when_the_editor_exits_nonzero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = edit_via_editor(dir.path(), &|key| {
            if key == "EDITOR" {
                Ok("false".to_string())
            } else {
                Err(env::VarError::NotPresent)
            }
        })
        .expect_err("a failing editor is an error");
        assert!(err.contains("EDITOR"), "{err}");
    }

    #[test]
    fn edit_via_editor_returns_edited_content_from_a_real_editor_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A multi-word `$EDITOR` (interpreter plus a script), exercising the
        // `sh -c '<editor> "$0"'` indirection that lets an `$EDITOR` value
        // carry its own arguments.
        let edited = complete_task_markdown("Edited via a fake editor");
        let script_path = dir.path().join("fake-editor.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{edited}EOF\n"),
        )
        .expect("write fake editor script");
        let editor = format!("sh {}", script_path.display());

        let content = edit_via_editor(dir.path(), &|key| {
            if key == "EDITOR" {
                Ok(editor.clone())
            } else {
                Err(env::VarError::NotPresent)
            }
        })
        .expect("fake editor runs successfully");
        assert_eq!(content, edited);
    }

    // -- run (`$EDITOR`, process-boundary) ---------------------------------

    /// Spawns this test binary re-executed as `child_name` with `EDITOR` set
    /// (or, if `None`, explicitly removed) in the child's own environment —
    /// the same isolated-process pattern `cmd::doctor` and `cmd::init` use,
    /// applied here so no test mutates this process's real environment
    /// (unsafe to do from multiple threads under Rust 2024) and no two
    /// `$EDITOR`-dependent tests can race each other over it.
    fn run_child(child_name: &str, editor: Option<&str>) -> (String, String) {
        let exe = env::current_exe().expect("current test exe");
        let mut command = Command::new(exe);
        command.args(["--exact", "--ignored", "--nocapture", child_name]);
        match editor {
            Some(value) => {
                command.env("EDITOR", value);
            }
            None => {
                command.env_remove("EDITOR");
            }
        }
        let output = command.output().expect("spawn child");
        (
            String::from_utf8(output.stdout).expect("stdout is utf8"),
            String::from_utf8(output.stderr).expect("stderr is utf8"),
        )
    }

    #[test]
    fn run_reports_usage_naming_editor_when_editor_is_unset() {
        let (_stdout, stderr) = run_child("cmd::add::tests::emit_fixture_run_without_editor", None);
        assert!(stderr.contains("EDITOR"), "{stderr:?}");
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_reports_usage_naming_editor_when_editor_is_unset"]
    fn emit_fixture_run_without_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let outcome = run(&project, &Config::default(), None);
        assert!(matches!(outcome, RunOutcome::Usage { .. }));
        // `main.rs` alone prints `RunOutcome::Usage`'s detail (through
        // `render::progress`, to stderr); `cmd::add::run` itself never
        // writes on the error path, so this reproduces that here for
        // `run_child`'s parent to observe, through the same sanctioned
        // call site `render.rs` documents rather than a raw `eprintln!`.
        if let RunOutcome::Usage { detail } = outcome {
            render::progress(format_args!("error: {detail}"));
        }
    }
}
