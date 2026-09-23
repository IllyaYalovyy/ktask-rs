//! `ktask-rs plan lint`: validates the whole queue without running anything.
//!
//! `docs/CONTRACT.md` section 3: reports every task with a missing required
//! section, an unparsable `**Verify:**` command, or an id shared with
//! another task — all of them, in one pass, never stopping at the first
//! problem found. Exit 0 when the queue is clean, 2
//! ([`RunOutcome::Usage`]) when any task is malformed; [`RunOutcome::CheckFailed`]
//! only if the queue itself cannot even be read (VISION.md section 4: `add`
//! and `plan lint` apply the same validation, applied again here so state
//! that reached the journal by some other path is still caught).

use std::collections::BTreeMap;

use ktask_core::{Config, Project, RunOutcome, Task, TaskId, load, validate};

use crate::cli::PlanCommand;
use crate::render;

/// Routes a `plan` subcommand to its behavior. `Lint` is currently the only
/// one [`crate::cli::PlanCommand`] has.
pub(crate) fn run(project: &Project, _config: &Config, command: &PlanCommand) -> RunOutcome {
    match command {
        PlanCommand::Lint => lint_run(project),
    }
}

/// Reads `project`'s queue, prints one line per problem found, and reports
/// whether it was clean.
///
/// A queue that cannot even be opened or read is a [`RunOutcome::CheckFailed`]
/// (the same distinction `cmd::status` draws): that is an environment or
/// state problem, not a verdict about any task's content. Once the queue is
/// read, every problem [`lint`] finds is printed and the run reports
/// [`RunOutcome::Usage`] (exit 2, `docs/CONTRACT.md` section 1) if there was
/// at least one, [`RunOutcome::Drained`] (exit 0) if the queue was clean.
fn lint_run(project: &Project) -> RunOutcome {
    let tasks = match load(project) {
        Ok(tasks) => tasks,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("plan lint: could not read the queue: {err}"),
            };
        }
    };

    let problems = lint(&tasks);
    for problem in &problems {
        render::out(format_args!("{problem}"));
    }

    if problems.is_empty() {
        RunOutcome::Drained
    } else {
        RunOutcome::Usage {
            detail: format!("plan lint: {} problem(s) found", problems.len()),
        }
    }
}

/// Checks every task in `tasks`, in document order, and returns one
/// human-readable line per problem.
///
/// Three kinds of problem are reported: a task [`validate`] rejects (a
/// missing required section, or an unknown named protocol — the same check
/// `add` runs at import time); a `**Verify:**` field that does not parse
/// into a shell command ([`parse_verify_command`]); and an id shared with
/// another task in `tasks`. A task can appear in more than one problem line
/// if it has more than one issue, and every task is checked regardless of
/// what earlier ones turned up — an early problem never shortens the scan.
fn lint(tasks: &[Task]) -> Vec<String> {
    let duplicate_counts = duplicate_id_counts(tasks);
    let mut problems = Vec::new();

    for task in tasks {
        if let Err(err) = validate(task) {
            problems.push(format!("task {}: {err}", task.id));
        }

        if !task.verify.trim().is_empty()
            && let Err(err) = parse_verify_command(&task.verify)
        {
            problems.push(format!(
                "task {}: unparsable Verify command: {err}",
                task.id
            ));
        }

        let count = duplicate_counts.get(&task.id).copied().unwrap_or(0);
        if count > 1 {
            problems.push(format!(
                "task {}: duplicate id (shared by {count} tasks)",
                task.id
            ));
        }
    }

    problems
}

/// How many times each id in `tasks` occurs.
fn duplicate_id_counts(tasks: &[Task]) -> BTreeMap<TaskId, u32> {
    let mut counts: BTreeMap<TaskId, u32> = BTreeMap::new();
    for task in tasks {
        *counts.entry(task.id).or_insert(0) += 1;
    }
    counts
}

/// Parses a `**Verify:**` field's content into a shell-style argv.
///
/// Every example in `.ktask/README.md` and `docs/CONTRACT.md` wraps the
/// command in a single backtick-quoted span (e.g. `` `cargo test` ``), so
/// that is what this requires: nothing before or after the backticks, and
/// the text inside splitting into words the way a POSIX shell would
/// (`'single'` and `"double"` quoting understood, `\` escaping the next
/// character outside quotes). [`ktask_core::run_gate`] never runs a task's
/// `Verify` command through a shell — it always spawns from an argv — so
/// this exists only to catch a human authoring mistake (an unbalanced quote
/// that would silently break at run time) before it ever reaches a gate,
/// not to reproduce full shell semantics such as globbing or substitution.
///
/// # Errors
///
/// A short message naming what is wrong: not wrapped in a single pair of
/// backticks, an unterminated quote, a trailing backslash, or a command
/// that parses to no words at all.
fn parse_verify_command(field: &str) -> Result<Vec<String>, String> {
    let trimmed = field.trim();
    let inner = trimmed
        .strip_prefix('`')
        .and_then(|rest| rest.strip_suffix('`'))
        .ok_or_else(|| "must be a single backtick-quoted command".to_string())?;

    let words = shell_words(inner)?;
    if words.is_empty() {
        return Err("command is empty".to_string());
    }
    Ok(words)
}

/// Whether [`shell_words`] is currently inside a quoted span, and which
/// kind — needed so a `'` inside `"..."` (and vice versa) is treated as
/// ordinary text rather than closing the span.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// A minimal POSIX-style shell word split, used only to judge whether a
/// `**Verify:**` command is well-formed (see [`parse_verify_command`]).
///
/// Whitespace separates words outside quotes; `'...'` is taken literally;
/// `"..."` allows `\"`, `\\` and `\$` as escapes (any other backslash
/// inside double quotes is kept as-is, matching `sh`); and `\` outside
/// quotes escapes the single character after it. Adjacent quoted and
/// unquoted segments with no space between them join into one word, also
/// matching shell behavior.
///
/// # Errors
///
/// Names an unterminated `'` or `"`, or a `\` with nothing after it.
fn shell_words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote = Quote::None;
    let mut chars = text.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '\\' | '$')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => {
                        return Err("trailing backslash inside a double-quoted string".to_string());
                    }
                },
                other => current.push(other),
            },
            Quote::None => match ch {
                ' ' | '\t' => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                '\'' => {
                    quote = Quote::Single;
                    in_word = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_word = true;
                }
                '\\' => {
                    in_word = true;
                    match chars.next() {
                        Some(next) => current.push(next),
                        None => return Err("trailing backslash".to_string()),
                    }
                }
                other => {
                    in_word = true;
                    current.push(other);
                }
            },
        }
    }

    match quote {
        Quote::Single => return Err("unterminated single-quoted string".to_string()),
        Quote::Double => return Err("unterminated double-quoted string".to_string()),
        Quote::None => {}
    }

    if in_word {
        words.push(current);
    }

    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{Journal, TaskStatus};
    use std::path::PathBuf;

    fn task(id: u32, verify: &str) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "it happens.".to_string(),
            done_when: "it happened.".to_string(),
            verify: verify.to_string(),
            refs: "none".to_string(),
            protocol: None,
        }
    }

    // -- shell_words ----------------------------------------------------------

    #[test]
    fn shell_words_splits_on_whitespace() {
        assert_eq!(
            shell_words("cargo test --all").unwrap(),
            vec!["cargo", "test", "--all"]
        );
    }

    #[test]
    fn shell_words_treats_single_quotes_literally() {
        assert_eq!(
            shell_words("cargo -E 'test(/cmd::plan/)'").unwrap(),
            vec!["cargo", "-E", "test(/cmd::plan/)"]
        );
    }

    #[test]
    fn shell_words_understands_double_quote_escapes() {
        assert_eq!(
            shell_words(r#"echo "a \"quoted\" word""#).unwrap(),
            vec!["echo", "a \"quoted\" word"]
        );
    }

    #[test]
    fn shell_words_joins_adjacent_quoted_and_unquoted_segments() {
        assert_eq!(shell_words("foo'bar'baz").unwrap(), vec!["foobarbaz"]);
    }

    #[test]
    fn shell_words_rejects_an_unterminated_single_quote() {
        let err = shell_words("cargo -E 'test(/foo").unwrap_err();
        assert!(err.contains("single"), "{err}");
    }

    #[test]
    fn shell_words_rejects_an_unterminated_double_quote() {
        let err = shell_words(r#"echo "unterminated"#).unwrap_err();
        assert!(err.contains("double"), "{err}");
    }

    #[test]
    fn shell_words_rejects_a_trailing_backslash() {
        let err = shell_words(r"cargo test\").unwrap_err();
        assert!(err.contains("backslash"), "{err}");
    }

    #[test]
    fn shell_words_of_an_empty_string_is_no_words() {
        assert_eq!(shell_words("").unwrap(), Vec::<String>::new());
    }

    // -- parse_verify_command --------------------------------------------------

    #[test]
    fn parse_verify_command_accepts_a_simple_backtick_command() {
        assert_eq!(parse_verify_command("`true`").unwrap(), vec!["true"]);
    }

    #[test]
    fn parse_verify_command_accepts_a_quoted_nextest_filter() {
        assert_eq!(
            parse_verify_command("`cargo nextest run -p ktask-cli -E 'test(/cmd::plan/)'`")
                .unwrap(),
            vec![
                "cargo",
                "nextest",
                "run",
                "-p",
                "ktask-cli",
                "-E",
                "test(/cmd::plan/)"
            ]
        );
    }

    #[test]
    fn parse_verify_command_rejects_text_not_wrapped_in_backticks() {
        let err = parse_verify_command("cargo test").unwrap_err();
        assert!(err.contains("backtick"), "{err}");
    }

    #[test]
    fn parse_verify_command_rejects_a_trailing_sentence_outside_the_backticks() {
        let err = parse_verify_command("`cargo test` and check manually").unwrap_err();
        assert!(err.contains("backtick"), "{err}");
    }

    #[test]
    fn parse_verify_command_rejects_an_unbalanced_quote_inside_the_backticks() {
        let err = parse_verify_command("`cargo test -E 'unterminated`").unwrap_err();
        assert!(err.contains("single"), "{err}");
    }

    #[test]
    fn parse_verify_command_rejects_an_empty_backtick_span() {
        let err = parse_verify_command("``").unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    // -- lint -------------------------------------------------------------------

    #[test]
    fn lint_of_a_clean_queue_is_empty() {
        let tasks = vec![task(1, "`true`"), task(2, "`cargo test`")];
        assert!(lint(&tasks).is_empty());
    }

    #[test]
    fn lint_reports_a_task_missing_a_required_section() {
        let mut broken = task(1, "`true`");
        broken.outcome = String::new();
        let tasks = vec![broken];

        let problems = lint(&tasks);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("task 1"), "{problems:?}");
        assert!(problems[0].contains("Outcome"), "{problems:?}");
    }

    #[test]
    fn lint_reports_a_task_with_an_unparsable_verify_command() {
        let tasks = vec![task(1, "cargo test")];

        let problems = lint(&tasks);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("task 1"), "{problems:?}");
        assert!(
            problems[0].contains("unparsable Verify command"),
            "{problems:?}"
        );
    }

    #[test]
    fn lint_does_not_double_report_a_verify_field_that_is_already_missing() {
        let tasks = vec![task(1, "")];

        let problems = lint(&tasks);
        // Only the missing-section problem, not also an "unparsable Verify
        // command" problem for the same empty field.
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("Verify"), "{problems:?}");
    }

    #[test]
    fn lint_reports_every_task_sharing_a_duplicate_id() {
        let mut second = task(2, "`true`");
        second.id = TaskId::new(1);
        let tasks = vec![task(1, "`true`"), second];

        let problems = lint(&tasks);
        assert_eq!(problems.len(), 2);
        assert!(problems.iter().all(|p| p.contains("duplicate id")));
    }

    #[test]
    fn lint_reports_every_problem_in_one_pass_not_just_the_first() {
        let mut missing_section = task(1, "`true`");
        missing_section.outcome = String::new();
        let unparsable_verify = task(2, "cargo test");
        let duplicate_a = task(3, "`true`");
        let duplicate_b = task(3, "`true`");

        let tasks = vec![missing_section, unparsable_verify, duplicate_a, duplicate_b];

        let problems = lint(&tasks);
        assert_eq!(
            problems.len(),
            4,
            "one missing-section, one unparsable-verify, two duplicate-id: {problems:?}"
        );
        assert!(problems.iter().any(|p| p.contains("task 1")));
        assert!(problems.iter().any(|p| p.contains("task 2")));
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.contains("task 3") && p.contains("duplicate id"))
                .count(),
            2
        );
    }

    #[test]
    fn lint_reports_a_task_with_an_unknown_protocol() {
        let mut broken = task(1, "`true`");
        broken.protocol = Some("waterfall".to_string());
        let tasks = vec![broken];

        let problems = lint(&tasks);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("waterfall"), "{problems:?}");
    }

    // -- lint_run / run -----------------------------------------------------

    fn journal_project(state_dir: &std::path::Path) -> Project {
        Project {
            root: PathBuf::from("/repo"),
            id: "plan-lint-fixture".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn lint_run_reports_drained_for_a_clean_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let plan = "\
## Do the thing

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none
";
        let parsed = ktask_core::parse_plan(plan).expect("parse_plan");
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal.put_tasks(&parsed).expect("put_tasks");

        assert_eq!(lint_run(&project), RunOutcome::Drained);
    }

    #[test]
    fn lint_run_reports_usage_and_prints_problems_for_a_malformed_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let plan = "\
## Bad verify

**Outcome:** it happens.

**Done-when:** it happened.

**Verify:** cargo test

**Refs:** none
";
        let parsed = ktask_core::parse_plan(plan).expect("parse_plan");
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal.put_tasks(&parsed).expect("put_tasks");

        let outcome = lint_run(&project);
        assert!(matches!(outcome, RunOutcome::Usage { .. }), "{outcome:?}");
    }

    #[test]
    fn lint_run_reports_drained_for_an_empty_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        Journal::open_for(&project).expect("open journal");

        assert_eq!(lint_run(&project), RunOutcome::Drained);
    }

    #[test]
    fn lint_run_reports_check_failed_when_the_journal_cannot_be_opened() {
        let project = Project {
            root: PathBuf::from("/nonexistent/ktask-plan-lint-fixture/root"),
            id: "plan-lint-broken-fixture".to_string(),
            state_dir: PathBuf::from("/nonexistent/ktask-plan-lint-fixture/state"),
        };

        let outcome = lint_run(&project);
        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn run_dispatches_lint_to_lint_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        Journal::open_for(&project).expect("open journal");

        let outcome = run(&project, &Config::default(), &PlanCommand::Lint);
        assert_eq!(outcome, RunOutcome::Drained);
    }
}
