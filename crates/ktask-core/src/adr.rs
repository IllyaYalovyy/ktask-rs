//! Recording the answer to a `waiting_input` question: the `DecisionResolved`
//! event and the ADR that goes with it.
//!
//! `ktask-rs resolve` and the input inbox of the interface both answer a
//! question, and `docs/CONTRACT.md` section 4 says the interface writes "the
//! same ADR as `resolve`". They can only be the same if there is one
//! implementation, so it is here, below both: [`resolve_decision`] is the
//! whole recording, and each caller only decides where the answer comes from.
//!
//! `VISION.md` §6: every transition is journaled before its side effect. The
//! journal is written first and the event carries the whole answer, so an ADR
//! that then fails to be written costs a file, not the decision
//! (`docs/adr/0009-*.md`). Nothing here commits the ADR.

use crate::{DecisionRequest, EventKind, Journal, Project, TaskId, TaskState, apply, redact};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::Date;

/// The ADR directory, relative to the repository root.
pub const ADR_DIR: &str = "docs/adr";

/// The longest slug in an ADR's filename, in characters.
const SLUG_LIMIT: usize = 50;

/// The longest title in an ADR's heading, in characters, before it is cut.
const TITLE_LIMIT: usize = 72;

/// Why a resolution was not recorded, or not completely.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The ADR directory could not be read to number the new record. Nothing
    /// has been journaled.
    #[error("could not read {}: {source}", dir.display())]
    Scan {
        /// The directory that could not be read.
        dir: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },

    /// The task's history could not be read or replayed. Nothing has been
    /// journaled.
    #[error("could not read the state of task {task}: {source}")]
    State {
        /// The task whose history was needed.
        task: TaskId,
        /// Why it could not be read.
        source: crate::Error,
    },

    /// The task is not in a state that accepts an answer. Nothing has been
    /// journaled.
    #[error("task {task} cannot be resolved: {source}")]
    Rejected {
        /// The task that was to be resolved.
        task: TaskId,
        /// The transition table's refusal.
        source: crate::Error,
    },

    /// The resolution could not be appended to the journal. No ADR has been
    /// written.
    #[error("could not record the resolution of task {task}: {source}")]
    Record {
        /// The task that was resolved.
        task: TaskId,
        /// Why the journal refused it.
        source: crate::Error,
    },

    /// The resolution is journaled, but its ADR could not be written.
    #[error(
        "the answer to task {task} is journaled, but its ADR {} could not be written: \
         {source}; the answer was: {answer}", path.display()
    )]
    Write {
        /// The task that was resolved.
        task: TaskId,
        /// The ADR's path, relative to the repository root.
        path: PathBuf,
        /// The answer that is journaled but has no file.
        answer: String,
        /// What the operating system said.
        source: std::io::Error,
    },
}

/// Records `answer` to the question `request` that `task` is waiting on: it
/// journals `DecisionResolved`, which returns the task to the queue, then
/// writes the next ADR under `docs/adr` in the repository. Returns the ADR's
/// path relative to the repository root.
///
/// `answer` is recorded as given; a caller that wants it trimmed or
/// non-empty makes it so first. `today` dates the record.
///
/// # Errors
///
/// A [`ResolveError`] naming which step failed, and so whether the resolution
/// is journaled: only [`ResolveError::Write`] leaves it so.
pub fn resolve_decision(
    project: &Project,
    task: TaskId,
    request: &DecisionRequest,
    answer: &str,
    today: Date,
) -> Result<PathBuf, ResolveError> {
    let state_of = |source| ResolveError::State { task, source };
    let mut journal = Journal::open_for(project).map_err(state_of)?;
    let current = journal
        .events_for(task)
        .and_then(|events| {
            events
                .into_iter()
                .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))
        })
        .map_err(state_of)?;

    let adr_dir = project.root.join(ADR_DIR);
    let number = next_number(&adr_dir).map_err(|source| ResolveError::Scan {
        dir: adr_dir,
        source,
    })?;
    let title = title(&request.question);
    let relative = PathBuf::from(ADR_DIR).join(format!("{number:04}-{}.md", slug(&title)));

    let event = EventKind::DecisionResolved {
        adr_path: relative.clone(),
        answer: answer.to_string(),
    };
    apply(&current, &event).map_err(|source| ResolveError::Rejected { task, source })?;
    journal
        .append(Some(task), &event)
        .map_err(|source| ResolveError::Record { task, source })?;

    let adr = render(number, &title, today, task, request, answer);
    write(&project.root.join(&relative), &adr).map_err(|source| ResolveError::Write {
        task,
        path: relative.clone(),
        answer: answer.to_string(),
        source,
    })?;
    Ok(relative)
}

/// The question `task` most recently raised: the last `DecisionRaised` in
/// its journaled history.
///
/// # Errors
///
/// Returns an error if the journal cannot be read.
pub fn raised_request(project: &Project, task: TaskId) -> crate::Result<Option<DecisionRequest>> {
    let events = Journal::open_for(project)?.events_for(task)?;
    Ok(events.into_iter().rev().find_map(|event| match event.kind {
        EventKind::DecisionRaised { request } => Some(request),
        _ => None,
    }))
}

/// `items` as a Markdown bullet list, one per line.
#[must_use]
pub fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The number the next ADR takes: one more than the highest `NNNN-*.md`
/// under `dir`, or 1 when there is none (0000 is the template). Files that
/// do not open with four digits and a hyphen are not ADRs and are ignored.
fn next_number(dir: &Path) -> std::io::Result<u32> {
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

/// The ADR for a resolved decision, in the shape of
/// `docs/adr/0000-template.md`. Redacted as a whole: unlike the journal and
/// the state directory, this file goes into the repository.
fn render(
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
fn write(path: &Path, adr: &str) -> std::io::Result<()> {
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
    use crate::{AttemptId, PauseReason, Task, TaskStatus};
    use time::Month;

    fn today() -> Date {
        Date::from_calendar_date(2026, Month::September, 23).expect("a real date")
    }

    fn task() -> Task {
        Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "Task 1".to_string(),
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

    /// A project whose task 1 has raised `request` and is waiting for input.
    fn waiting_project(dir: &tempfile::TempDir) -> Project {
        let project = Project {
            root: dir.path().join("repo"),
            id: "adr-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        std::fs::create_dir_all(&project.root).expect("create repo root");
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal.put_tasks(&[task()]).expect("put tasks");
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
            EventKind::DecisionRaised { request: request() },
        ] {
            journal.append(Some(TaskId::new(1)), &kind).expect("append");
        }
        project
    }

    fn state_of(project: &Project) -> TaskState {
        Journal::open_for(project)
            .expect("open journal")
            .events_for(TaskId::new(1))
            .expect("events")
            .into_iter()
            .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))
            .expect("replay")
    }

    /// What answering the fixture question with "Use SQLite." writes, spelled
    /// out independently of [`render`].
    const GOLDEN: &str = "# 0001. Postgres or SQLite for the journal\n\
\n\
- **Status:** accepted\n\
- **Date:** 2026-09-23\n\
\n\
## Context\n\
\n\
Task 1 paused for a decision (`waiting_input`):\n\
\n\
Postgres or SQLite for the journal?\n\
\n\
Trade-offs: Postgres scales; SQLite is one file.\n\
\n\
Recommended by the agent: SQLite\n\
\n\
## Decision\n\
\n\
Use SQLite.\n\
\n\
## Alternatives considered\n\
\n\
The options the agent put forward:\n\
\n\
- Postgres\n\
- SQLite\n\
\n\
## Consequences\n\
\n\
Journal durability and operational overhead.\n\
\n\
Recorded by `ktask-rs resolve`; task 1 runs again with this decision in its context.\n";

    #[test]
    fn a_resolution_writes_exactly_the_adr_the_contract_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let path = resolve_decision(&project, TaskId::new(1), &request(), "Use SQLite.", today())
            .expect("resolve");

        assert_eq!(
            path,
            PathBuf::from("docs/adr/0001-postgres-or-sqlite-for-the-journal.md")
        );
        let written = std::fs::read_to_string(project.root.join(&path)).expect("read adr");
        assert_eq!(written, GOLDEN);
    }

    #[test]
    fn a_resolution_is_journaled_with_the_whole_answer_and_requeues_the_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        assert!(matches!(
            state_of(&project),
            TaskState::Paused {
                reason: PauseReason::Input,
                ..
            }
        ));

        let path = resolve_decision(&project, TaskId::new(1), &request(), "Use SQLite.", today())
            .expect("resolve");

        let last = Journal::open_for(&project)
            .expect("open journal")
            .events_for(TaskId::new(1))
            .expect("events")
            .pop()
            .expect("an event");
        assert_eq!(
            last.kind,
            EventKind::DecisionResolved {
                adr_path: path,
                answer: "Use SQLite.".to_string(),
            }
        );
        assert_eq!(state_of(&project), TaskState::Queued);
    }

    #[test]
    fn a_task_not_waiting_for_input_is_rejected_and_nothing_is_written_or_journaled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        resolve_decision(&project, TaskId::new(1), &request(), "Use SQLite.", today())
            .expect("resolve once");
        let before = Journal::open_for(&project)
            .expect("open journal")
            .events()
            .expect("events")
            .len();

        let again = resolve_decision(&project, TaskId::new(1), &request(), "Again.", today());

        assert!(
            matches!(again, Err(ResolveError::Rejected { .. })),
            "{again:?}"
        );
        let after = Journal::open_for(&project)
            .expect("open journal")
            .events()
            .expect("events")
            .len();
        assert_eq!(after, before);
        let adrs = std::fs::read_dir(project.root.join(ADR_DIR))
            .expect("adr dir")
            .count();
        assert_eq!(adrs, 1);
    }

    #[test]
    fn an_unreadable_adr_directory_stops_before_anything_is_journaled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        // `docs` is a file, so `docs/adr` is not a directory beneath it.
        std::fs::write(project.root.join("docs"), "").expect("write blocker");

        let outcome = resolve_decision(&project, TaskId::new(1), &request(), "x", today());

        assert!(
            matches!(outcome, Err(ResolveError::Scan { .. })),
            "{outcome:?}"
        );
        assert!(matches!(state_of(&project), TaskState::Paused { .. }));
    }

    #[test]
    fn a_failed_write_is_reported_with_the_answer_after_the_journal_holds_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        let adrs = project.root.join(ADR_DIR);
        std::fs::create_dir_all(&adrs).expect("mkdir");
        std::fs::set_permissions(&adrs, std::fs::Permissions::from_mode(0o555))
            .expect("make the ADR directory read-only");

        let outcome =
            resolve_decision(&project, TaskId::new(1), &request(), "Use SQLite.", today());

        let Err(ResolveError::Write { answer, .. }) = &outcome else {
            panic!("expected a write failure, got {outcome:?}");
        };
        assert_eq!(answer, "Use SQLite.");
        let text = outcome.expect_err("failed").to_string();
        assert!(text.contains("journaled"), "{text}");
        assert!(text.contains("Use SQLite."), "{text}");
        assert_eq!(state_of(&project), TaskState::Queued);
    }

    #[test]
    fn a_credential_in_the_answer_is_redacted_before_it_reaches_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);

        let path = resolve_decision(
            &project,
            TaskId::new(1),
            &request(),
            "api_key = \"abcdefgh12345678\"",
            today(),
        )
        .expect("resolve");

        let adr = std::fs::read_to_string(project.root.join(path)).expect("read adr");
        assert!(!adr.contains("abcdefgh12345678"), "{adr}");
        assert!(adr.contains("[redacted]"), "{adr}");
    }

    #[test]
    fn the_raised_request_is_the_last_question_the_task_asked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = waiting_project(&dir);
        assert_eq!(
            raised_request(&project, TaskId::new(1)).expect("read"),
            Some(request())
        );
        assert_eq!(
            raised_request(&project, TaskId::new(9)).expect("read"),
            None
        );
    }

    #[test]
    fn the_next_number_follows_the_highest_one_present_and_ignores_other_files() {
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

        assert_eq!(next_number(&adrs).expect("scan"), 13);
    }

    #[test]
    fn the_first_adr_of_a_repository_with_no_adr_directory_is_number_one() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(next_number(&dir.path().join("missing")).expect("scan"), 1);
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
}
