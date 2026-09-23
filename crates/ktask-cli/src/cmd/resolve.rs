//! `ktask-rs resolve`: answers a `waiting_input` question
//! (`docs/CONTRACT.md` section 3).
//!
//! `VISION.md` §3 invariant 8: "design decisions belong to the human ... its
//! resolution is recorded as a decision record (ADR) available to future
//! tasks." The answer comes from `--note`, or from `$EDITOR` opened on the
//! question. It is journaled as a `DecisionResolved` event, which returns the
//! task to the queue, and written as the next ADR in the repository's
//! `docs/adr/` in the shape of `docs/adr/0000-template.md`. The next task's
//! context picks it up from there ([`ktask_core::collect_adrs`]).
//!
//! The journal is written first: every transition is journaled before its
//! side effect (`VISION.md` §6), and the event carries the whole answer, so
//! an ADR that then fails to be written costs a file, not the decision.
//!
//! Nothing here commits the ADR; see `docs/adr/0009-*.md`.

use ktask_core::{
    Config, DecisionRequest, EventKind, Journal, PauseReason, Project, RunOutcome, TaskId,
    TaskState, apply, redact,
};
use std::env::{self, VarError};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::{Date, OffsetDateTime};

use super::editor;
use crate::cmd::run::read_queue;
use crate::render;

/// The ADR directory, relative to the repository root.
const ADR_DIR: &str = "docs/adr";

/// The scratch file `$EDITOR` opens, within the project's state directory.
const SCRATCH_FILE_NAME: &str = "resolve-answer.md";

/// The longest slug in an ADR's filename, in characters.
const SLUG_LIMIT: usize = 50;

/// The longest title in an ADR's heading, in characters, before it is cut.
const TITLE_LIMIT: usize = 72;

/// Answers the question `task` is waiting on with `note`, or with what
/// `$EDITOR` leaves behind when `note` is omitted. Exits 0 once the answer is
/// journaled and its ADR written; exits 2 when `task` is not waiting for
/// input or the answer is empty.
pub(crate) fn run(
    project: &Project,
    _config: &Config,
    task: TaskId,
    note: Option<&str>,
) -> RunOutcome {
    resolve(
        project,
        task,
        note,
        &|key| env::var(key),
        OffsetDateTime::now_utc().date(),
    )
}

/// [`run`] with the environment and the date passed in, so a test needs
/// neither the real process environment nor the real clock.
fn resolve(
    project: &Project,
    task: TaskId,
    note: Option<&str>,
    env_var: &dyn Fn(&str) -> Result<String, VarError>,
    today: Date,
) -> RunOutcome {
    let check_failed = |detail: String| {
        render::progress(format_args!("error: {detail}"));
        RunOutcome::CheckFailed { detail }
    };

    let (tasks, states) = match read_queue(project) {
        Ok(queue) => queue,
        Err(err) => return check_failed(format!("resolve: could not read the queue: {err}")),
    };
    if !tasks.iter().any(|candidate| candidate.id == task) {
        return RunOutcome::Usage {
            detail: format!("no task {task} in the queue"),
        };
    }
    let state = states.get(&task).unwrap_or(&TaskState::Queued);
    if !matches!(
        state,
        TaskState::Paused {
            reason: PauseReason::Input,
            ..
        }
    ) {
        return RunOutcome::Usage {
            detail: format!(
                "resolve: task {task} is {}, not waiting for input",
                state.name().to_lowercase()
            ),
        };
    }

    let request = match raised_request(project, task) {
        Ok(Some(request)) => request,
        Ok(None) => {
            return check_failed(format!(
                "resolve: task {task} is waiting for input but the journal holds no question for it"
            ));
        }
        Err(err) => {
            return check_failed(format!("resolve: could not read the journal: {err}"));
        }
    };

    let answer = match answer_for(project, &request, note, env_var) {
        Ok(answer) => answer,
        Err(detail) => return RunOutcome::Usage { detail },
    };

    let adr_dir = project.root.join(ADR_DIR);
    let number = match next_adr_number(&adr_dir) {
        Ok(number) => number,
        Err(err) => {
            return check_failed(format!(
                "resolve: could not read {}: {err}",
                adr_dir.display()
            ));
        }
    };
    let title = title(&request.question);
    let relative = PathBuf::from(ADR_DIR).join(format!("{number:04}-{}.md", slug(&title)));

    let event = EventKind::DecisionResolved {
        adr_path: relative.clone(),
        answer: answer.clone(),
    };
    // `read_queue` only lets an input pause get this far, the one state that
    // accepts this event; checking anyway keeps that a fact `apply` states
    // rather than one this function assumes.
    if let Err(err) = apply(state, &event) {
        return RunOutcome::Usage {
            detail: format!("resolve: task {task} cannot be resolved: {err}"),
        };
    }
    let appended =
        Journal::open_for(project).and_then(|mut journal| journal.append(Some(task), &event));
    if let Err(err) = appended {
        return check_failed(format!(
            "resolve: could not record the resolution of task {task}: {err}"
        ));
    }

    let adr = render_adr(number, &title, today, task, &request, &answer);
    if let Err(err) = write_adr(&project.root.join(&relative), &adr) {
        return check_failed(format!(
            "resolve: the answer to task {task} is journaled, but its ADR {} could not be \
             written: {err}; the answer was: {answer}",
            relative.display()
        ));
    }

    render::out(format_args!("task {task} resolved: {}", relative.display()));
    render::progress(format_args!(
        "resolve: commit the ADR before `ktask-rs resume` runs task {task} again; preflight \
         refuses a working tree with uncommitted files"
    ));
    RunOutcome::Drained
}

/// The question `task` most recently raised: the last `DecisionRaised` in
/// its journaled history.
fn raised_request(project: &Project, task: TaskId) -> ktask_core::Result<Option<DecisionRequest>> {
    let events = Journal::open_for(project)?.events_for(task)?;
    Ok(events.into_iter().rev().find_map(|event| match event.kind {
        EventKind::DecisionRaised { request } => Some(request),
        _ => None,
    }))
}

/// The answer to `request`: `note` if given, else what `$EDITOR` leaves
/// after the question. Trimmed; never empty.
///
/// # Errors
///
/// Returns the usage-error text when `$EDITOR` is needed and cannot be run,
/// or when the answer is empty.
fn answer_for(
    project: &Project,
    request: &DecisionRequest,
    note: Option<&str>,
    env_var: &dyn Fn(&str) -> Result<String, VarError>,
) -> Result<String, String> {
    let text = if let Some(note) = note {
        note.to_string()
    } else {
        let edited = editor::edit(
            "resolve",
            "--note",
            SCRATCH_FILE_NAME,
            &editor_template(request),
            &project.state_dir,
            env_var,
        )?;
        strip_comment(&edited).to_string()
    };
    let answer = text.trim();
    if answer.is_empty() {
        return Err("resolve: the answer is empty".to_string());
    }
    Ok(answer.to_string())
}

/// What `$EDITOR` opens: the question, its options and trade-offs in an
/// HTML comment (which [`strip_comment`] removes again), then room to answer.
fn editor_template(request: &DecisionRequest) -> String {
    let options = bullets(&request.options);
    let recommended = request
        .recommended
        .as_ref()
        .map_or_else(String::new, |recommended| {
            format!("\nRecommended: {recommended}\n")
        });
    format!(
        "<!--\nAnswer below this comment; the comment is not kept.\n\n\
         Question: {}\n\nOptions:\n{options}\n\nTrade-offs: {}\n\nImpact: {}\n{recommended}-->\n\n",
        request.question, request.tradeoffs, request.impact
    )
}

/// `text` without a leading HTML comment, if it starts with one that is
/// closed. A comment anywhere else, or one never closed, is part of the
/// answer.
fn strip_comment(text: &str) -> &str {
    let trimmed = text.trim_start();
    trimmed
        .strip_prefix("<!--")
        .and_then(|rest| rest.split_once("-->"))
        .map_or(text, |(_, after)| after.trim())
}

/// The number the next ADR takes: one more than the highest `NNNN-*.md`
/// under `dir`, or 1 when there is none (0000 is the template). Files that
/// do not open with four digits and a hyphen are not ADRs and are ignored.
fn next_adr_number(dir: &Path) -> std::io::Result<u32> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(err) => return Err(err),
    };
    let mut highest = 0;
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let number = name
            .split_once('-')
            .filter(|(digits, _)| digits.len() == 4)
            .and_then(|(digits, _)| digits.parse::<u32>().ok());
        highest = highest.max(number.unwrap_or(0));
    }
    Ok(highest + 1)
}

/// The ADR heading for `question`: its first non-empty line without closing
/// punctuation, cut to [`TITLE_LIMIT`] characters with an ellipsis.
fn title(question: &str) -> String {
    let line = question
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .trim_end_matches(['?', '.', '!', ':'])
        .trim_end();
    if line.is_empty() {
        return "Decision".to_string();
    }
    if line.chars().count() <= TITLE_LIMIT {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_LIMIT).collect();
    format!("{}…", cut.trim_end())
}

/// A filename-safe rendering of `text`: lowercase ASCII letters and digits
/// in words joined by single hyphens, at most [`SLUG_LIMIT`] characters, or
/// `decision` when nothing is left.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let cut: String = out.trim_matches('-').chars().take(SLUG_LIMIT).collect();
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "decision".to_string()
    } else {
        cut.to_string()
    }
}

/// `items` as a Markdown bullet list, one per line.
fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ADR for a resolved decision, in the shape of
/// `docs/adr/0000-template.md`. Redacted as a whole: unlike the journal and
/// the state directory, this file goes into the repository.
fn render_adr(
    number: u32,
    title: &str,
    date: Date,
    task: TaskId,
    request: &DecisionRequest,
    answer: &str,
) -> String {
    let recommended = request
        .recommended
        .as_ref()
        .map_or_else(String::new, |recommended| {
            format!("\n\nRecommended by the agent: {recommended}")
        });
    let context = format!(
        "Task {task} paused for a decision (`waiting_input`):\n\n{}\n\nTrade-offs: {}{recommended}",
        request.question, request.tradeoffs
    );
    let alternatives = bullets(&request.options);

    let adr = format!(
        "# {number:04}. {title}\n\n\
         - **Status:** accepted\n\
         - **Date:** {:04}-{:02}-{:02}\n\n\
         ## Context\n\n{context}\n\n\
         ## Decision\n\n{answer}\n\n\
         ## Alternatives considered\n\nThe options the agent put forward:\n\n{alternatives}\n\n\
         ## Consequences\n\n{}\n\n\
         Recorded by `ktask-rs resolve`; task {task} runs again with this decision in its \
         context.\n",
        date.year(),
        u8::from(date.month()),
        date.day(),
        request.impact,
    );
    redact(&adr, &[])
}

/// Writes `adr` to `path`, creating `docs/adr` if it is not there yet.
/// Refuses to overwrite: an ADR is a record, and a number already taken is
/// somebody else's.
fn write_adr(path: &Path, adr: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(adr.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::run::read_queue;
    use ktask_core::{
        AttemptId, DecisionRequest, EventKind, Journal, PauseReason, Task, TaskState, TaskStatus,
    };
    use std::env::VarError;
    use std::path::Path;
    use time::{Date, Month};

    fn today() -> Date {
        Date::from_calendar_date(2026, Month::September, 23).expect("a real date")
    }

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
            protocol: None,
        }
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            question: "Postgres or SQLite for the journal?".to_string(),
            options: vec!["Postgres".to_string(), "SQLite".to_string()],
            tradeoffs: "Postgres scales; SQLite is one file.".to_string(),
            impact: "Journal durability and operational overhead.".to_string(),
            recommended: Some("SQLite".to_string()),
        }
    }

    fn no_env(_: &str) -> Result<String, VarError> {
        Err(VarError::NotPresent)
    }

    fn editor_env(editor: String) -> impl Fn(&str) -> Result<String, VarError> {
        move |key| {
            if key == "EDITOR" {
                Ok(editor.clone())
            } else {
                Err(VarError::NotPresent)
            }
        }
    }

    /// A project (repository root and state directory both under `dir`)
    /// whose task 1 has raised `request` and is now waiting for input.
    fn waiting_project(dir: &tempfile::TempDir, request: DecisionRequest) -> Project {
        let project = Project {
            root: dir.path().join("repo"),
            id: "resolve-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        std::fs::create_dir_all(&project.root).expect("create repo root");
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal.put_tasks(&[task(1), task(2)]).expect("put tasks");
        let id = Some(TaskId::new(1));
        for kind in [
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 1,
                base_sha: "abc".to_string(),
            },
            EventKind::DecisionRaised { request },
        ] {
            journal.append(id, &kind).expect("append");
        }
        project
    }

    fn event_count(project: &Project) -> usize {
        Journal::open_for(project)
            .expect("open journal")
            .events()
            .expect("events")
            .len()
    }

    fn state_of(project: &Project, id: u32) -> TaskState {
        read_queue(project).expect("read queue").1[&TaskId::new(id)].clone()
    }

    fn adr_dir(project: &Project) -> PathBuf {
        project.root.join("docs/adr")
    }

    fn adr_names(project: &Project) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(adr_dir(project))
            .map(|entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("entry")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The structure of an ADR: its `#` heading marker, its `- **Field:**`
    /// labels and its `##` section headings, with every free-text value
    /// removed — what a document must share with the template to be "the
    /// template's shape".
    fn skeleton(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| {
                if line.starts_with("## ") {
                    Some(line.to_string())
                } else if line.starts_with("# ") {
                    Some("#".to_string())
                } else if line.starts_with("- **") {
                    line.find(":**").map(|end| line[..end + 3].to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn template() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/adr/0000-template.md");
        std::fs::read_to_string(&path).expect("the ADR template exists")
    }

    // -- resolve: the happy path ---------------------------------------------

    #[test]
    fn a_note_is_written_as_the_next_adr_and_journaled_as_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());

        let outcome = resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        assert_eq!(outcome, RunOutcome::Drained);
        let names = adr_names(&project);
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(names[0], "0001-postgres-or-sqlite-for-the-journal.md");

        let journal = Journal::open_for(&project).expect("open journal");
        let last = journal
            .events_for(TaskId::new(1))
            .expect("events")
            .pop()
            .expect("an event");
        assert_eq!(
            last.kind,
            EventKind::DecisionResolved {
                adr_path: "docs/adr/0001-postgres-or-sqlite-for-the-journal.md".into(),
                answer: "Use SQLite.".to_string(),
            }
        );
    }

    #[test]
    fn resolving_returns_the_task_to_the_queue_and_touches_no_other_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        assert_eq!(
            state_of(&project, 1),
            TaskState::Paused {
                reason: PauseReason::Input,
                resume_to: Box::new(TaskState::Running {
                    attempt: AttemptId::new(1),
                    phase: ktask_core::Phase::Implement,
                }),
            }
        );

        resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        assert_eq!(state_of(&project, 1), TaskState::Queued);
        assert_eq!(state_of(&project, 2), TaskState::Queued);
    }

    #[test]
    fn the_adr_carries_the_question_the_answer_and_what_the_agent_put_forward() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());

        resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        let adr = std::fs::read_to_string(
            adr_dir(&project).join("0001-postgres-or-sqlite-for-the-journal.md"),
        )
        .expect("read adr");
        let decision = adr.split("## Decision").nth(1).expect("a Decision section");
        let decision = decision
            .split("## Alternatives considered")
            .next()
            .expect("section");
        assert_eq!(decision.trim(), "Use SQLite.");
        for expected in [
            "Postgres or SQLite for the journal?",
            "Postgres scales; SQLite is one file.",
            "- Postgres",
            "- SQLite",
            "Journal durability and operational overhead.",
            "task 1",
        ] {
            assert!(adr.contains(expected), "missing {expected:?} in:\n{adr}");
        }
        assert!(adr.contains("Recommended by the agent: SQLite"), "{adr}");
    }

    #[test]
    fn an_answer_that_looks_like_a_credential_is_redacted_before_it_reaches_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let secret = "api_key = \"abcdefgh12345678\"";

        resolve(&project, TaskId::new(1), Some(secret), &no_env, today());

        let names = adr_names(&project);
        let adr = std::fs::read_to_string(adr_dir(&project).join(&names[0])).expect("read adr");
        assert!(!adr.contains("abcdefgh12345678"), "{adr}");
        assert!(adr.contains("[redacted]"), "{adr}");
    }

    // -- the ADR's shape -------------------------------------------------------

    #[test]
    fn the_adr_has_the_templates_structure_and_filled_front_matter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());

        resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        let adr = std::fs::read_to_string(
            adr_dir(&project).join("0001-postgres-or-sqlite-for-the-journal.md"),
        )
        .expect("read adr");
        assert_eq!(skeleton(&adr), skeleton(&template()));
        assert_eq!(
            adr.lines().next(),
            Some("# 0001. Postgres or SQLite for the journal")
        );
        assert!(adr.contains("\n- **Status:** accepted\n"), "{adr}");
        assert!(adr.contains("\n- **Date:** 2026-09-23\n"), "{adr}");
    }

    #[test]
    fn the_next_adr_number_follows_the_highest_one_present_and_ignores_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adrs = dir.path().join("adr");
        std::fs::create_dir_all(&adrs).expect("mkdir");
        for name in [
            "0000-template.md",
            "0008-retry.md",
            "0012-something.md",
            "README.md",
            "notes.txt",
            "12-short.md",
        ] {
            std::fs::write(adrs.join(name), "").expect("write");
        }

        assert_eq!(next_adr_number(&adrs).expect("scan"), 13);
    }

    #[test]
    fn the_first_adr_of_a_repository_with_no_adr_directory_is_number_one() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(
            next_adr_number(&dir.path().join("missing")).expect("scan"),
            1
        );
    }

    #[test]
    fn a_second_resolution_takes_the_next_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );
        // Task 1 runs again and asks another question.
        let mut journal = Journal::open_for(&project).expect("open journal");
        for kind in [
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(2),
                protocol: "direct".to_string(),
                pid: 1,
                base_sha: "abc".to_string(),
            },
            EventKind::DecisionRaised {
                request: DecisionRequest {
                    question: "Which port?".to_string(),
                    ..request()
                },
            },
        ] {
            journal.append(Some(TaskId::new(1)), &kind).expect("append");
        }

        let outcome = resolve(&project, TaskId::new(1), Some("8080"), &no_env, today());

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(
            adr_names(&project),
            vec![
                "0001-postgres-or-sqlite-for-the-journal.md".to_string(),
                "0002-which-port.md".to_string(),
            ]
        );
    }

    #[test]
    fn slugs_are_lowercase_ascii_words_joined_by_single_hyphens() {
        assert_eq!(slug("Postgres or SQLite?"), "postgres-or-sqlite");
        assert_eq!(
            slug("  --Use  `tokio`, not async-std!! "),
            "use-tokio-not-async-std"
        );
        assert_eq!(slug("Übergröße"), "bergr-e");
        assert_eq!(slug("???"), "decision");
        assert_eq!(slug(""), "decision");
    }

    #[test]
    fn a_long_slug_is_cut_without_leaving_a_trailing_hyphen() {
        let long = format!("{} {}", "a".repeat(49), "b".repeat(30));

        let cut = slug(&long);

        assert_eq!(cut, "a".repeat(49));
        assert!(cut.len() <= SLUG_LIMIT);
    }

    #[test]
    fn a_title_is_the_first_line_of_the_question_without_its_closing_punctuation() {
        assert_eq!(title("Which database?\nMore detail."), "Which database");
        assert_eq!(title("\n  Which database?  "), "Which database");
        assert_eq!(title(""), "Decision");
    }

    #[test]
    fn a_long_title_is_shortened_with_an_ellipsis() {
        let shown = title(&"word ".repeat(40));

        assert_eq!(shown.chars().count(), TITLE_LIMIT + 1);
        assert!(shown.ends_with('…'), "{shown}");
    }

    // -- refusals ----------------------------------------------------------------

    #[test]
    fn a_task_that_is_not_waiting_for_input_is_a_usage_error_and_nothing_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let before = event_count(&project);

        let outcome = resolve(&project, TaskId::new(2), Some("x"), &no_env, today());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "resolve: task 2 is queued, not waiting for input".to_string()
            }
        );
        assert_eq!(event_count(&project), before);
        assert!(adr_names(&project).is_empty());
    }

    #[test]
    fn a_task_paused_at_a_human_gate_is_not_waiting_for_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        Journal::open_for(&project)
            .expect("open journal")
            .append(
                Some(TaskId::new(2)),
                &EventKind::Paused {
                    reason: PauseReason::HumanGate,
                },
            )
            .expect("append");

        let outcome = resolve(&project, TaskId::new(2), Some("x"), &no_env, today());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "resolve: task 2 is paused, not waiting for input".to_string()
            }
        );
    }

    #[test]
    fn a_task_already_resolved_cannot_be_resolved_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );
        let before = event_count(&project);

        let outcome = resolve(
            &project,
            TaskId::new(1),
            Some("Use Postgres."),
            &no_env,
            today(),
        );

        assert!(matches!(outcome, RunOutcome::Usage { .. }), "{outcome:?}");
        assert_eq!(event_count(&project), before);
        assert_eq!(adr_names(&project).len(), 1);
    }

    #[test]
    fn a_task_not_in_the_queue_is_a_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());

        let outcome = resolve(&project, TaskId::new(9), Some("x"), &no_env, today());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "no task 9 in the queue".to_string()
            }
        );
    }

    #[test]
    fn a_blank_note_is_a_usage_error_and_nothing_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let before = event_count(&project);

        let outcome = resolve(&project, TaskId::new(1), Some("  \n "), &no_env, today());

        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "resolve: the answer is empty".to_string()
            }
        );
        assert_eq!(event_count(&project), before);
        assert!(adr_names(&project).is_empty());
    }

    #[test]
    fn an_unreadable_queue_is_a_check_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").expect("write blocker");
        let project = Project {
            root: dir.path().join("repo"),
            id: "resolve-broken".to_string(),
            state_dir: blocker,
        };

        let outcome = resolve(&project, TaskId::new(1), Some("x"), &no_env, today());

        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_waiting_task_with_no_recorded_question_is_a_check_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        // Task 2 paused for input without ever raising a decision: a state
        // no runner produces, but the journal is the source of truth.
        let mut journal = Journal::open_for(&project).expect("open journal");
        for kind in [
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::Paused {
                reason: PauseReason::Input,
            },
        ] {
            journal.append(Some(TaskId::new(2)), &kind).expect("append");
        }
        let before = event_count(&project);

        let outcome = resolve(&project, TaskId::new(2), Some("x"), &no_env, today());

        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
        assert_eq!(event_count(&project), before);
    }

    // -- when the ADR cannot be written or found -------------------------------------

    #[test]
    fn an_unreadable_adr_directory_is_a_check_failure_and_nothing_is_journaled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        // `docs` is a file, so `docs/adr` is not a directory beneath it.
        std::fs::write(project.root.join("docs"), "").expect("write blocker");
        let before = event_count(&project);

        let outcome = resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
        assert_eq!(event_count(&project), before);
    }

    #[test]
    fn a_failed_adr_write_is_reported_after_the_resolution_is_already_journaled() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        std::fs::create_dir_all(adr_dir(&project)).expect("mkdir");
        std::fs::set_permissions(adr_dir(&project), std::fs::Permissions::from_mode(0o555))
            .expect("make the ADR directory read-only");

        let outcome = resolve(
            &project,
            TaskId::new(1),
            Some("Use SQLite."),
            &no_env,
            today(),
        );

        // The journal is written first (every transition is journaled before
        // its side effect), and it carries the whole answer, so the decision
        // is not lost with the file.
        let RunOutcome::CheckFailed { detail } = outcome else {
            panic!("expected CheckFailed, got {outcome:?}");
        };
        assert!(detail.contains("journaled"), "{detail}");
        assert!(detail.contains("Use SQLite."), "{detail}");
        assert_eq!(state_of(&project, 1), TaskState::Queued);
        assert!(adr_names(&project).is_empty());
    }

    // -- the editor -------------------------------------------------------------------

    fn fake_editor(dir: &Path, body: &str) -> String {
        let script = dir.join("fake-editor.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{body}EOF\n"),
        )
        .expect("write fake editor");
        format!("sh {}", script.display())
    }

    #[test]
    fn without_a_note_the_answer_is_what_the_editor_leaves_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let editor = fake_editor(dir.path(), "Use SQLite.\n\nIt is one file.\n");

        let outcome = resolve(&project, TaskId::new(1), None, &editor_env(editor), today());

        assert_eq!(outcome, RunOutcome::Drained);
        let journal = Journal::open_for(&project).expect("open journal");
        let last = journal
            .events_for(TaskId::new(1))
            .expect("events")
            .pop()
            .expect("event");
        let EventKind::DecisionResolved { answer, .. } = last.kind else {
            panic!("expected DecisionResolved, got {:?}", last.kind);
        };
        assert_eq!(answer, "Use SQLite.\n\nIt is one file.");
    }

    #[test]
    fn the_editor_is_shown_the_question_and_the_options_in_a_comment_that_is_not_the_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let seen = dir.path().join("seen.txt");
        let script = dir.path().join("spy.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncp \"$1\" {}\n", seen.display()),
        )
        .expect("write spy");

        let outcome = resolve(
            &project,
            TaskId::new(1),
            None,
            &editor_env(format!("sh {}", script.display())),
            today(),
        );

        // The spy leaves the template untouched, so the answer is empty…
        assert_eq!(
            outcome,
            RunOutcome::Usage {
                detail: "resolve: the answer is empty".to_string()
            }
        );
        // …and what it was shown names the question, every option and the
        // trade-offs.
        let shown = std::fs::read_to_string(&seen).expect("the editor was opened");
        for expected in [
            "Postgres or SQLite for the journal?",
            "Postgres",
            "SQLite",
            "Postgres scales; SQLite is one file.",
        ] {
            assert!(
                shown.contains(expected),
                "missing {expected:?} in:\n{shown}"
            );
        }
    }

    #[test]
    fn an_answer_written_below_the_comment_is_kept_and_the_comment_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let editor = fake_editor(dir.path(), "<!-- the question -->\n\nUse SQLite.\n");

        resolve(&project, TaskId::new(1), None, &editor_env(editor), today());

        let names = adr_names(&project);
        let adr = std::fs::read_to_string(adr_dir(&project).join(&names[0])).expect("read adr");
        assert!(adr.contains("Use SQLite."), "{adr}");
        assert!(!adr.contains("the question"), "{adr}");
    }

    #[test]
    fn without_a_note_and_without_an_editor_the_error_names_the_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir, request());
        let before = event_count(&project);

        let outcome = resolve(&project, TaskId::new(1), None, &no_env, today());

        let RunOutcome::Usage { detail } = outcome else {
            panic!("expected Usage, got {outcome:?}");
        };
        assert!(detail.contains("--note"), "{detail}");
        assert_eq!(event_count(&project), before);
    }

    #[test]
    fn a_comment_block_is_removed_only_from_the_start_of_the_text() {
        assert_eq!(strip_comment("<!-- a -->\nanswer"), "answer");
        assert_eq!(
            strip_comment("  <!--\nmulti\nline\n-->  answer  "),
            "answer"
        );
        assert_eq!(
            strip_comment("answer <!-- keep -->"),
            "answer <!-- keep -->"
        );
        assert_eq!(
            strip_comment("<!-- never closed\nanswer"),
            "<!-- never closed\nanswer"
        );
        assert_eq!(strip_comment("plain"), "plain");
    }
}
