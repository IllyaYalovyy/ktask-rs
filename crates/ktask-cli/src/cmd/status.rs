//! `ktask-rs status`: the headless dashboard (`docs/CONTRACT.md` section 3).
//!
//! Reads the queue and every task's current state straight from the
//! journal and prints them — one line per task, then a summary line of
//! counts by state — without ever recording anything: [`Journal::tasks`],
//! [`Journal::all_states`] and [`Journal::events_for`], the only journal
//! methods this module calls, are all read-only. `--json` emits the same
//! data as the `{project, tasks: [...], summary: {...}}` shape
//! `docs/CONTRACT.md` documents, through [`json::emit_json`] so it lands on
//! stdout as a single compact line.
//!
//! As in `cmd::doctor`, every function below [`run`] is pure: it takes
//! already-read tasks, states and events (or, for rendering, an explicit
//! `now`) and returns a value, so a test can hand it a fixture without
//! touching a real journal or the real clock. [`run`] alone opens the
//! journal and touches the clock.

use ktask_core::{
    Config, Event, EventKind, Journal, Phase, Project, RunOutcome, Task, TaskId, TaskState,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use time::OffsetDateTime;

use crate::{json, render};

/// One task's row in the dashboard, carrying its full [`TaskState`] (rather
/// than only the name [`RowData::to_json_row`] reduces it to) so
/// [`render_line`] can also decide whether to show elapsed time.
#[derive(Debug, Clone, PartialEq)]
struct RowData {
    id: TaskId,
    title: String,
    state: TaskState,
    protocol: String,
    phase: Option<Phase>,
    attempts: u32,
    started_at: Option<OffsetDateTime>,
    ended_at: Option<OffsetDateTime>,
}

impl RowData {
    /// Reduces this row to the shape `docs/CONTRACT.md` section 3 documents
    /// for one entry of `status --json`'s `tasks` array.
    fn to_json_row(&self) -> TaskRow {
        TaskRow {
            id: self.id,
            title: self.title.clone(),
            state: self.state.name().to_string(),
            protocol: self.protocol.clone(),
            phase: self.phase,
            attempts: self.attempts,
            started_at: self.started_at,
            ended_at: self.ended_at,
        }
    }
}

/// One task's row, exactly the shape `docs/CONTRACT.md` section 3 documents
/// for `status --json`'s `tasks` array.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TaskRow {
    id: TaskId,
    title: String,
    state: String,
    protocol: String,
    phase: Option<Phase>,
    attempts: u32,
    #[serde(with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    ended_at: Option<OffsetDateTime>,
}

/// The whole `status --json` payload: `docs/CONTRACT.md` section 3.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct StatusReport {
    project: String,
    tasks: Vec<TaskRow>,
    summary: BTreeMap<String, u32>,
}

/// Reads `project`'s queue and prints its dashboard, in human or `--json`
/// form, without recording anything. Always [`RunOutcome::Drained`] once the
/// journal has been read; a journal that cannot even be opened or read
/// (corrupt state, an unwritable path) is reported as
/// [`RunOutcome::CheckFailed`] rather than panicking or fabricating an empty
/// queue.
pub(crate) fn run(project: &Project, config: &Config, json_output: bool) -> RunOutcome {
    let journal = match Journal::open_for(project) {
        Ok(journal) => journal,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("status: could not open the journal: {err}"),
            };
        }
    };

    let rows = match collect_rows(&journal, config) {
        Ok(rows) => rows,
        Err(err) => {
            return RunOutcome::CheckFailed {
                detail: format!("status: could not read the journal: {err}"),
            };
        }
    };

    if json_output {
        let report = StatusReport {
            project: project.id.clone(),
            tasks: rows.iter().map(RowData::to_json_row).collect(),
            summary: summary_counts(&rows),
        };
        let _ = json::emit_json(&report);
    } else {
        let now = OffsetDateTime::now_utc();
        for row in &rows {
            render::out(format_args!("{}", render_line(row, now)));
        }
        render::out(format_args!("{}", summary_line(&summary_counts(&rows))));
    }

    RunOutcome::Drained
}

/// Reads every task in `project`'s queue, its current state, and the events
/// recorded against it, and reduces all three to one [`RowData`] per task,
/// in queue order.
///
/// A task with no entry in the journal's `task_state` projection has not
/// yet had a `TaskQueued` event recorded against it (matching
/// [`ktask_core::next_runnable`]'s own reading of a missing entry) but is
/// still queued, not started: it is displayed as [`TaskState::Queued`]
/// rather than causing an error or a blank row.
fn collect_rows(journal: &Journal, config: &Config) -> ktask_core::Result<Vec<RowData>> {
    let tasks = journal.tasks()?;
    let states = journal.all_states()?;

    let mut rows = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let state = states.get(&task.id).cloned().unwrap_or(TaskState::Queued);
        let events = journal.events_for(task.id)?;
        rows.push(build_row(task, state, &events, config));
    }
    Ok(rows)
}

/// Builds one task's [`RowData`] from its parsed `task`, its current
/// `state`, and every event recorded against it (`events`, already in `seq`
/// order per [`Journal::events_for`]).
///
/// `attempts` counts `AttemptStarted` events: each fresh attempt at the
/// task, including the one `ktask-rs retry` starts, gets exactly one
/// (`docs/CONTRACT.md`'s `retry`: "starts a fresh remediation attempt"),
/// while a remediation loop within one attempt does not (`state.rs`:
/// `VerifyFailed` moves `Verifying` to `Remediating` carrying the same
/// `attempt`). `started_at` is the earliest event recorded for the task, and
/// `ended_at` is the latest — but only once `state` is terminal, since a
/// task still in flight has no "end" yet, however recent its last event.
///
/// The task's protocol name is resolved the same way
/// [`ktask_core::for_task`] would (the task's own `**Protocol:**` section,
/// falling back to `config.default_protocol`), but without its validation:
/// a misconfigured default must not make the whole dashboard fail to
/// render, only make this one row name a protocol that is not actually
/// runnable — a problem `ktask-rs doctor` or `ktask-rs plan lint` surfaces,
/// not `status`.
fn build_row(task: &Task, state: TaskState, events: &[Event], config: &Config) -> RowData {
    let attempts = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::AttemptStarted { .. }))
        .count();
    let attempts = u32::try_from(attempts).unwrap_or(u32::MAX);
    let started_at = events.first().map(|event| event.ts);
    let ended_at = state
        .is_terminal()
        .then(|| events.last().map(|event| event.ts))
        .flatten();
    let phase = phase_of(&state);

    RowData {
        id: task.id,
        title: task.title().to_string(),
        protocol: task
            .protocol
            .clone()
            .unwrap_or_else(|| config.default_protocol.clone()),
        state,
        phase,
        attempts,
        started_at,
        ended_at,
    }
}

/// The phase `state` is currently in, if it carries one at all.
///
/// `Running` and `Remediating` carry a phase directly; `Verifying` and
/// `Publishing` are always exactly [`Phase::Verify`] and [`Phase::Publish`]
/// (every protocol's mandatory tail, per `docs/adr` and `protocol.rs`'s
/// `checked`). A `Paused` task's phase is whatever phase `resume_to` will
/// return it to, found by recursing — a task paused mid-`Remediating` is
/// still "in" that phase as far as the dashboard is concerned, it is just
/// not actively running right now. Every other state (`Queued`, `Preflight`,
/// `PublishedVerified`, and the terminal states) carries no phase.
fn phase_of(state: &TaskState) -> Option<Phase> {
    match state {
        TaskState::Running { phase, .. } | TaskState::Remediating { phase, .. } => Some(*phase),
        TaskState::Verifying { .. } => Some(Phase::Verify),
        TaskState::Publishing { .. } => Some(Phase::Publish),
        TaskState::Paused { resume_to, .. } => phase_of(resume_to),
        TaskState::Queued
        | TaskState::Preflight
        | TaskState::PublishedVerified { .. }
        | TaskState::Done
        | TaskState::Acknowledged { .. }
        | TaskState::Failed { .. }
        | TaskState::Cancelled => None,
    }
}

/// Renders `row`'s human-readable line: id, title, state, protocol, phase
/// (`-` if it carries none), attempt count, and — only while
/// [`TaskState::is_active`] and a start time is on record — elapsed time
/// measured against `now`.
fn render_line(row: &RowData, now: OffsetDateTime) -> String {
    let phase = row
        .phase
        .map_or_else(|| "-".to_string(), |p| format!("{p:?}"));
    let mut line = format!(
        "{} {} state={} protocol={} phase={} attempts={}",
        row.id,
        row.title,
        row.state.name(),
        row.protocol,
        phase,
        row.attempts,
    );

    if row.state.is_active()
        && let Some(started_at) = row.started_at
    {
        let _ = write!(line, " elapsed={}", format_elapsed(now - started_at));
    }

    line
}

/// Formats a duration as a compact human string: seconds alone below a
/// minute, minutes and seconds below an hour, hours/minutes/seconds beyond
/// that. A negative duration (clock skew between when an event was recorded
/// and `now`) renders as `0s` rather than a confusing negative number.
fn format_elapsed(duration: time::Duration) -> String {
    let total_seconds = u64::try_from(duration.whole_seconds()).unwrap_or(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Counts `rows` by [`TaskState::name`], the shared source for both the
/// human summary line and `--json`'s `summary` object — so the two forms
/// can never disagree about the counts.
fn summary_counts(rows: &[RowData]) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for row in rows {
        *counts.entry(row.state.name().to_string()).or_insert(0) += 1;
    }
    counts
}

/// Renders `summary`'s counts as the one-line, `docs/CONTRACT.md`-mandated
/// summary that ends the human-readable form of `status`.
fn summary_line(summary: &BTreeMap<String, u32>) -> String {
    if summary.is_empty() {
        return "summary: no tasks".to_string();
    }
    let parts: Vec<String> = summary
        .iter()
        .map(|(state, count)| format!("{state}={count}"))
        .collect();
    format!("summary: {}", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{AttemptId, EventSeq, FailureClass, PauseReason, TaskStatus, apply};
    use std::env;
    use std::process::Command;
    use time::macros::datetime;

    // -- phase_of ---------------------------------------------------------

    #[test]
    fn phase_of_reads_the_phase_carried_by_running() {
        let state = TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Green,
        };
        assert_eq!(phase_of(&state), Some(Phase::Green));
    }

    #[test]
    fn phase_of_reads_the_phase_carried_by_remediating() {
        let state = TaskState::Remediating {
            attempt: AttemptId::new(1),
            phase: Phase::Harden,
        };
        assert_eq!(phase_of(&state), Some(Phase::Harden));
    }

    #[test]
    fn phase_of_verifying_is_always_the_verify_phase() {
        let state = TaskState::Verifying {
            attempt: AttemptId::new(1),
        };
        assert_eq!(phase_of(&state), Some(Phase::Verify));
    }

    #[test]
    fn phase_of_publishing_is_always_the_publish_phase() {
        let state = TaskState::Publishing {
            attempt: AttemptId::new(1),
        };
        assert_eq!(phase_of(&state), Some(Phase::Publish));
    }

    #[test]
    fn phase_of_paused_recurses_into_resume_to() {
        let state = TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Red,
            }),
        };
        assert_eq!(phase_of(&state), Some(Phase::Red));
    }

    #[test]
    fn phase_of_a_gate_pause_recurses_to_no_phase() {
        let state = TaskState::Paused {
            reason: PauseReason::HumanGate,
            resume_to: Box::new(TaskState::Queued),
        };
        assert_eq!(phase_of(&state), None);
    }

    #[test]
    fn phase_of_states_without_a_phase_is_none() {
        for state in [
            TaskState::Queued,
            TaskState::Preflight,
            TaskState::PublishedVerified {
                commit: "abc123".to_string(),
            },
            TaskState::Done,
            TaskState::Acknowledged {
                by: "alice".to_string(),
                at: OffsetDateTime::UNIX_EPOCH,
            },
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "crashed".to_string(),
            },
            TaskState::Cancelled,
        ] {
            assert_eq!(phase_of(&state), None, "state: {state:?}");
        }
    }

    // -- format_elapsed -----------------------------------------------------

    #[test]
    fn format_elapsed_below_a_minute_is_seconds_only() {
        assert_eq!(format_elapsed(time::Duration::seconds(0)), "0s");
        assert_eq!(format_elapsed(time::Duration::seconds(45)), "45s");
    }

    #[test]
    fn format_elapsed_below_an_hour_is_minutes_and_seconds() {
        assert_eq!(format_elapsed(time::Duration::seconds(65)), "1m05s");
        assert_eq!(format_elapsed(time::Duration::seconds(3599)), "59m59s");
    }

    #[test]
    fn format_elapsed_at_and_beyond_an_hour_includes_hours() {
        assert_eq!(format_elapsed(time::Duration::seconds(3600)), "1h00m00s");
        assert_eq!(format_elapsed(time::Duration::seconds(3725)), "1h02m05s");
    }

    #[test]
    fn format_elapsed_clamps_a_negative_duration_to_zero() {
        assert_eq!(format_elapsed(time::Duration::seconds(-5)), "0s");
    }

    // -- render_line ----------------------------------------------------------

    fn row(state: TaskState) -> RowData {
        RowData {
            id: TaskId::new(3),
            title: "Fix the widget".to_string(),
            phase: phase_of(&state),
            state,
            protocol: "direct".to_string(),
            attempts: 2,
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn render_line_includes_id_title_state_protocol_phase_and_attempts() {
        let line = render_line(&row(TaskState::Queued), OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            line,
            "3 Fix the widget state=Queued protocol=direct phase=- attempts=2"
        );
    }

    #[test]
    fn render_line_shows_the_phase_name_when_one_is_carried() {
        let state = TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Green,
        };
        let line = render_line(&row(state), OffsetDateTime::UNIX_EPOCH);
        assert!(line.contains("phase=Green"), "{line:?}");
    }

    #[test]
    fn render_line_omits_elapsed_for_an_inactive_task_even_with_a_start_time() {
        let mut r = row(TaskState::Done);
        r.started_at = Some(datetime!(2024-01-01 00:00:00 UTC));
        let now = datetime!(2024-01-01 00:05:00 UTC);
        let line = render_line(&r, now);
        assert!(!line.contains("elapsed="), "{line:?}");
    }

    #[test]
    fn render_line_omits_elapsed_for_an_active_task_with_no_recorded_start() {
        let state = TaskState::Preflight;
        let r = row(state);
        assert!(r.started_at.is_none(), "sanity: no start time on record");
        let line = render_line(&r, OffsetDateTime::UNIX_EPOCH);
        assert!(!line.contains("elapsed="), "{line:?}");
    }

    #[test]
    fn render_line_shows_elapsed_for_an_active_task_with_a_recorded_start() {
        let mut r = row(TaskState::Preflight);
        r.started_at = Some(datetime!(2024-01-01 00:00:00 UTC));
        let now = datetime!(2024-01-01 00:01:05 UTC);
        let line = render_line(&r, now);
        assert!(line.ends_with("elapsed=1m05s"), "{line:?}");
    }

    // -- summary_counts / summary_line ----------------------------------------

    #[test]
    fn summary_counts_tallies_rows_by_state_name() {
        let rows = vec![
            row(TaskState::Queued),
            row(TaskState::Queued),
            row(TaskState::Done),
        ];
        let counts = summary_counts(&rows);
        assert_eq!(counts.get("Queued"), Some(&2));
        assert_eq!(counts.get("Done"), Some(&1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn summary_line_of_an_empty_queue_says_so() {
        assert_eq!(summary_line(&BTreeMap::new()), "summary: no tasks");
    }

    #[test]
    fn summary_line_lists_every_present_state_and_its_count() {
        let mut counts = BTreeMap::new();
        counts.insert("Done".to_string(), 2);
        counts.insert("Queued".to_string(), 1);
        assert_eq!(summary_line(&counts), "summary: Done=2 Queued=1");
    }

    // -- build_row / collect_rows ---------------------------------------------

    fn sample_task(id: u32, protocol: Option<&str>) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
            protocol: protocol.map(str::to_string),
        }
    }

    fn attempt_started_event(seq: u64, ts: OffsetDateTime, task_id: TaskId) -> Event {
        Event {
            seq: EventSeq::new(seq),
            ts,
            task_id: Some(task_id),
            kind: EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 1234,
                base_sha: "abc".to_string(),
            },
        }
    }

    #[test]
    fn build_row_counts_attempt_started_events_and_ignores_others() {
        let task = sample_task(1, None);
        let events = vec![
            attempt_started_event(1, datetime!(2024-01-01 00:00:00 UTC), task.id),
            Event {
                seq: EventSeq::new(2),
                ts: datetime!(2024-01-01 00:00:01 UTC),
                task_id: Some(task.id),
                kind: EventKind::PhaseEntered {
                    attempt: AttemptId::new(1),
                    phase: Phase::Green,
                },
            },
            attempt_started_event(3, datetime!(2024-01-01 00:00:02 UTC), task.id),
        ];
        let row = build_row(
            &task,
            TaskState::Running {
                attempt: AttemptId::new(2),
                phase: Phase::Green,
            },
            &events,
            &Config::default(),
        );
        assert_eq!(row.attempts, 2);
    }

    #[test]
    fn build_row_started_at_is_the_first_event_and_ended_at_is_none_while_not_terminal() {
        let task = sample_task(1, None);
        let events = vec![attempt_started_event(
            1,
            datetime!(2024-01-01 00:00:00 UTC),
            task.id,
        )];
        let row = build_row(
            &task,
            TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            },
            &events,
            &Config::default(),
        );
        assert_eq!(row.started_at, Some(datetime!(2024-01-01 00:00:00 UTC)));
        assert_eq!(row.ended_at, None);
    }

    #[test]
    fn build_row_ended_at_is_the_last_event_once_the_state_is_terminal() {
        let task = sample_task(1, None);
        let events = vec![
            attempt_started_event(1, datetime!(2024-01-01 00:00:00 UTC), task.id),
            Event {
                seq: EventSeq::new(2),
                ts: datetime!(2024-01-01 00:05:00 UTC),
                task_id: Some(task.id),
                kind: EventKind::TaskDone {
                    commit: "def456".to_string(),
                },
            },
        ];
        let row = build_row(&task, TaskState::Done, &events, &Config::default());
        assert_eq!(row.started_at, Some(datetime!(2024-01-01 00:00:00 UTC)));
        assert_eq!(row.ended_at, Some(datetime!(2024-01-01 00:05:00 UTC)));
    }

    #[test]
    fn build_row_with_no_events_has_no_start_or_end_time_and_zero_attempts() {
        let task = sample_task(1, None);
        let row = build_row(&task, TaskState::Queued, &[], &Config::default());
        assert_eq!(row.attempts, 0);
        assert_eq!(row.started_at, None);
        assert_eq!(row.ended_at, None);
    }

    #[test]
    fn build_row_uses_the_tasks_own_protocol_when_named() {
        let task = sample_task(1, Some("tdd"));
        let row = build_row(&task, TaskState::Queued, &[], &Config::default());
        assert_eq!(row.protocol, "tdd");
    }

    #[test]
    fn build_row_falls_back_to_the_configured_default_protocol_when_the_task_names_none() {
        let task = sample_task(1, None);
        let mut config = Config::default();
        config.default_protocol = "tdd".to_string();
        let row = build_row(&task, TaskState::Queued, &[], &config);
        assert_eq!(row.protocol, "tdd");
    }

    fn journal_project(state_dir: &std::path::Path) -> Project {
        Project {
            root: std::path::PathBuf::from("/repo"),
            id: "status-fixture".to_string(),
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn collect_rows_uses_the_configured_default_protocol_when_a_task_names_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal
            .put_tasks(&[sample_task(1, None)])
            .expect("put_tasks");

        let mut config = Config::default();
        config.default_protocol = "tdd".to_string();

        let rows = collect_rows(&journal, &config).expect("collect_rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].protocol, "tdd");
    }

    #[test]
    fn collect_rows_defaults_an_unrecorded_task_to_queued() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal
            .put_tasks(&[sample_task(1, None)])
            .expect("put_tasks");

        let rows = collect_rows(&journal, &Config::default()).expect("collect_rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, TaskState::Queued);
    }

    #[test]
    fn collect_rows_returns_tasks_in_queue_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = journal_project(dir.path());
        let mut journal = Journal::open_for(&project).expect("open journal");
        journal
            .put_tasks(&[sample_task(1, None), sample_task(2, None)])
            .expect("put_tasks");

        let rows = collect_rows(&journal, &Config::default()).expect("collect_rows");
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![TaskId::new(1), TaskId::new(2)]
        );
    }

    /// Runs the whole `TaskQueued -> ... -> TaskDone` happy path against a
    /// real journal for one task, exactly as a live runner would: append
    /// each event, fold it through `apply`, and persist the resulting state.
    /// Reused by both the `run` and the `--json` fixture tests below so
    /// their expected output stays anchored to one real event trail rather
    /// than two hand-maintained ones.
    fn seed_completed_task(journal: &mut Journal, task_id: TaskId) {
        let attempt = AttemptId::new(1);
        let mut state = TaskState::Queued;

        let events: Vec<EventKind> = vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 4242,
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Verify,
            },
            EventKind::VerifyPassed { attempt },
            EventKind::PublishStarted {
                attempt,
                candidate_sha: "def456".to_string(),
            },
            EventKind::PublishVerified {
                commit: "def456".to_string(),
                remote_sha: "def456".to_string(),
            },
            EventKind::TaskDone {
                commit: "def456".to_string(),
            },
        ];

        for event in &events {
            journal.append(Some(task_id), event).expect("append");
            state = apply(&state, event).expect("apply");
        }
        journal.put_state(task_id, &state).expect("put_state");
    }

    fn seed_running_task(journal: &mut Journal, task_id: TaskId) {
        let attempt = AttemptId::new(1);
        let mut state = TaskState::Queued;

        let events: Vec<EventKind> = vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "base".to_string(),
            },
            EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 4242,
                base_sha: "base".to_string(),
            },
            EventKind::PhaseEntered {
                attempt,
                phase: Phase::Implement,
            },
        ];

        for event in &events {
            journal.append(Some(task_id), event).expect("append");
            state = apply(&state, event).expect("apply");
        }
        journal.put_state(task_id, &state).expect("put_state");
    }

    fn plan_with_three_tasks() -> String {
        "\
## First task

**Outcome:** the first thing happens.

**Done-when:** it happened.

**Verify:** `true`

**Refs:** none

## Second task

**Outcome:** the second thing happens.

**Done-when:** it happened too.

**Verify:** `true`

**Refs:** none

## Third task

**Outcome:** the third thing happens.

**Done-when:** it happened three.

**Verify:** `true`

**Refs:** none
"
        .to_string()
    }

    /// Builds a project (under `dir`) whose queue has three tasks: task 1
    /// completed to `Done`, task 2 mid-run (`Running`, active), and task 3
    /// left with no recorded state at all (defaults to `Queued`).
    fn build_fixture_project(dir: &std::path::Path) -> Project {
        let project = journal_project(dir);
        let plan = plan_with_three_tasks();
        let tasks = ktask_core::parse_plan(&plan).expect("parse_plan");

        let mut journal = Journal::open_for(&project).expect("open journal");
        journal.put_tasks(&tasks).expect("put_tasks");
        seed_completed_task(&mut journal, TaskId::new(1));
        seed_running_task(&mut journal, TaskId::new(2));
        // Task 3 is left with no recorded events or state at all.

        project
    }

    #[test]
    fn run_reports_drained_for_a_healthy_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = build_fixture_project(dir.path());
        let outcome = run(&project, &Config::default(), false);
        assert_eq!(outcome, RunOutcome::Drained);
    }

    #[test]
    fn run_reports_check_failed_when_the_journal_cannot_be_opened() {
        let project = journal_project(std::path::Path::new(
            "/nonexistent/ktask-status-run-fixture/state",
        ));
        let outcome = run(&project, &Config::default(), false);
        assert!(
            matches!(outcome, RunOutcome::CheckFailed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn run_never_writes_to_the_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = build_fixture_project(dir.path());
        let journal_file = ktask_core::journal_path(&project.state_dir);
        let before = std::fs::read(&journal_file).expect("read journal before");

        run(&project, &Config::default(), false);
        run(&project, &Config::default(), true);

        let after = std::fs::read(&journal_file).expect("read journal after");
        assert_eq!(before, after, "status must never write to the journal");
    }

    /// Spawns this test binary re-executed as `child_name`, the pattern
    /// `cmd::doctor`'s own tests use to observe real, separate stdout and
    /// stderr for a process boundary this crate has no library target to
    /// unit test against directly.
    fn run_child(child_name: &str) -> (String, String) {
        let exe = env::current_exe().expect("current test exe");
        let output = Command::new(exe)
            .args(["--exact", "--ignored", "--nocapture", child_name])
            .output()
            .expect("spawn child");
        (
            String::from_utf8(output.stdout).expect("stdout is utf8"),
            String::from_utf8(output.stderr).expect("stderr is utf8"),
        )
    }

    #[test]
    fn run_prints_one_line_per_task_then_a_summary_line_to_stdout_only() {
        let (stdout, stderr) = run_child("cmd::status::tests::emit_fixture_status_run");

        // The child is libtest's own runner invoked for one `#[ignore]`d
        // test, so stdout also carries its "running 1 test" / "test ...
        // ok" narration around whatever `run` itself wrote; `status`'s own
        // lines are the ones starting with a task id or `summary:`.
        let lines: Vec<&str> = stdout
            .lines()
            .filter(|line| {
                line.starts_with("summary:") || line.chars().next().is_some_and(char::is_numeric)
            })
            .collect();
        assert_eq!(lines.len(), 4, "3 tasks + 1 summary line: {stdout:?}");
        assert!(
            lines[0].starts_with("1 First task state=Done protocol=direct phase=- attempts=1"),
            "{:?}",
            lines[0]
        );
        assert!(
            lines[1].starts_with(
                "2 Second task state=Running protocol=direct phase=Implement attempts=1"
            ),
            "{:?}",
            lines[1]
        );
        assert!(lines[1].contains("elapsed="), "{:?}", lines[1]);
        assert_eq!(
            lines[2],
            "3 Third task state=Queued protocol=direct phase=- attempts=0"
        );
        assert_eq!(lines[3], "summary: Done=1 Queued=1 Running=1");

        assert!(
            !stderr.contains("state="),
            "status lines leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_prints_one_line_per_task_then_a_summary_line_to_stdout_only"]
    fn emit_fixture_status_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = build_fixture_project(dir.path());
        run(&project, &Config::default(), false);
    }

    #[test]
    fn run_with_json_emits_one_object_to_stdout_with_the_documented_shape() {
        let (stdout, stderr) = run_child("cmd::status::tests::emit_fixture_status_run_json");

        let json_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with('{')).collect();
        assert_eq!(json_lines.len(), 1, "expected one JSON line: {stdout:?}");

        let parsed: serde_json::Value =
            serde_json::from_str(json_lines[0]).expect("valid JSON object");
        assert_eq!(parsed["project"], "status-fixture");

        let tasks = parsed["tasks"].as_array().expect("tasks is an array");
        assert_eq!(tasks.len(), 3);
        for task in tasks {
            for field in [
                "id",
                "title",
                "state",
                "protocol",
                "phase",
                "attempts",
                "started_at",
                "ended_at",
            ] {
                assert!(
                    task.as_object()
                        .expect("task is an object")
                        .contains_key(field),
                    "missing field {field}: {task:?}"
                );
            }
        }

        assert_eq!(tasks[0]["id"], 1);
        assert_eq!(tasks[0]["state"], "Done");
        assert_eq!(tasks[0]["phase"], serde_json::Value::Null);
        assert_eq!(tasks[0]["attempts"], 1);
        assert!(tasks[0]["started_at"].is_string());
        assert!(tasks[0]["ended_at"].is_string());

        assert_eq!(tasks[1]["id"], 2);
        assert_eq!(tasks[1]["state"], "Running");
        assert_eq!(tasks[1]["phase"], "Implement");
        assert_eq!(tasks[1]["ended_at"], serde_json::Value::Null);

        assert_eq!(tasks[2]["id"], 3);
        assert_eq!(tasks[2]["state"], "Queued");
        assert_eq!(tasks[2]["started_at"], serde_json::Value::Null);

        assert_eq!(parsed["summary"]["Done"], 1);
        assert_eq!(parsed["summary"]["Running"], 1);
        assert_eq!(parsed["summary"]["Queued"], 1);

        assert!(
            !stderr.contains('{'),
            "json output leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_with_json_emits_one_object_to_stdout_with_the_documented_shape"]
    fn emit_fixture_status_run_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = build_fixture_project(dir.path());
        run(&project, &Config::default(), true);
    }
}
