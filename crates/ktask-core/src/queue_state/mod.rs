//! The queue's state, and the pure rules for adding, placing and cancelling tasks, and for
//! beginning, running and ending their attempts.
//!
//! This is the one place a command (add, add at a place, cancel, begin an attempt, report,
//! mark running, end an attempt) is turned into events or a refusal, and the one place events
//! are folded into state. No I/O: [`crate::Journal`] only stores and returns the events this
//! module decides.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::journal::{AttemptRun, Event, LimitWait, WaitReason};
use crate::{
    AcknowledgeError, AnswerError, AppendConflict, AppendError, Attempt, AttemptEnd,
    BeginAttemptError, CancelError, DoneError, Journal, JournalError, Outcome, Placement,
    RecordReportError, RetryError, Step, Task, TaskDraft, TaskId, TaskStatus,
};

mod apply;
mod decide;
mod query;

/// One step of an attempt, as folded from its events: [`AttemptFold::steps`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepFold {
    name: String,
    provider: Option<String>,
    model: Option<String>,
    started_at: SystemTime,
    ended: Option<AttemptEnd>,
}

/// One task's attempt, as folded from its events: the state [`QueueState::attempt_of`] and
/// [`QueueState::running`] read from, keyed by task in [`QueueState::attempts`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptFold {
    number: u32,
    started_at: SystemTime,
    start_commit: Option<String>,
    provider: Option<String>,
    session: Option<String>,
    /// The step name and time of the most recent [`Event::AttemptWaiting`] not yet superseded
    /// by a later event for this attempt: [`QueueState::attempt_from_fold`].
    waiting: Option<(String, SystemTime, WaitReason)>,
    ended: Option<AttemptEnd>,
    steps: Vec<StepFold>,
}

/// The queue's state: every task ever added, in queue order, with the cancelled ones marked
/// but kept in place, and every attempt ever begun for each task, oldest first — folded from
/// the events recorded for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueueState {
    tasks: Vec<Task>,
    attempts: HashMap<TaskId, Vec<AttemptFold>>,
    reports: HashMap<(TaskId, u32), (Outcome, Option<String>)>,
    step_reports: HashMap<(TaskId, u32, String), (Outcome, Option<String>)>,
    /// The model the resolver named for an attempt's own retry decision, when it named one:
    /// [`QueueState::retry_model_of`].
    retry_models: HashMap<(TaskId, u32), String>,
    /// Whether the resolver's retry decision for an attempt asked to resume its own session:
    /// [`QueueState::retry_same_session_of`].
    retry_same_sessions: HashMap<(TaskId, u32), bool>,
    /// Whether the resolver's retry decision for an attempt asked for the working tree to be
    /// reset before the task's next attempt begins: [`QueueState::retry_reset_tree_of`].
    retry_reset_trees: HashMap<(TaskId, u32), bool>,
    /// The most recent gate stop recorded for each task, cleared once a later attempt for it
    /// actually begins: [`QueueState::gate_stop_of`].
    gate_stops: HashMap<TaskId, (String, String)>,
    /// The answer recorded for each attempt that was answered: [`QueueState::answer_of`].
    answers: HashMap<(TaskId, u32), String>,
    /// The reason and when each task was marked done by the user's own hand:
    /// [`QueueState::done_mark_of`].
    done_marks: HashMap<TaskId, (String, SystemTime)>,
}

impl QueueState {
    /// Folds `events`, in the order they were recorded, into the state they produced.
    #[must_use]
    pub(crate) fn fold(events: &[Event]) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    /// `(state, event) -> state`: the one place an event changes the queue's state.
    fn apply(&mut self, event: &Event) {
        match event {
            Event::TaskAdded { .. } => self.apply_task_added(event),
            Event::TaskCancelled { .. } => self.apply_task_cancelled(event),
            Event::AttemptStarted { .. } => self.apply_attempt_started(event),
            Event::AttemptRunning { .. } => self.apply_attempt_running(event),
            Event::AttemptWaiting { .. } => self.apply_attempt_waiting(event),
            Event::AttemptReported { .. } => self.apply_attempt_reported(event),
            Event::AttemptSessionRecorded { .. } => self.apply_attempt_session_recorded(event),
            Event::AttemptEnded { .. } => self.apply_attempt_ended(event),
            Event::StepStarted { .. } => self.apply_step_started(event),
            Event::StepEnded { .. } => self.apply_step_ended(event),
            Event::GateFailed { .. } => self.apply_gate_failed(event),
            Event::TaskRetried { .. } => self.apply_task_retried(event),
            Event::TaskAnswered { .. } => self.apply_task_answered(event),
            Event::TaskDoneByUser { .. } => self.apply_task_done_by_user(event),
            Event::TaskAcknowledged { .. } => self.apply_task_acknowledged(event),
        }
    }

    /// The attempt numbered `number` of task `id`, when its fold is still held. Shared by the
    /// handful of events (running, reported, ended, a step) that update an attempt already
    /// begun, found by number rather than assumed to be the last one, so an event addressed to
    /// an earlier attempt — never produced by any use case, but not this fold's job to rule
    /// out — never lands on the wrong one.
    fn attempt_mut(&mut self, id: TaskId, number: u32) -> Option<&mut AttemptFold> {
        self.attempts
            .get_mut(&id)?
            .iter_mut()
            .find(|attempt| attempt.number == number)
    }

    /// The most recently begun attempt of task `id`, when it has one.
    fn current_attempt(&self, id: TaskId) -> Option<&AttemptFold> {
        self.attempts.get(&id).and_then(|attempts| attempts.last())
    }
}

/// Reads the queue's events, decides `build`'s events against the state they fold to, and
/// appends them — retrying, from a fresh read, for as long as [`Journal::append_events`]
/// reports that the journal moved on since. `build` does not check any rule outside the
/// queue's own state (a draft's title, criteria, links, and so on): the caller has done that
/// before looping.
///
/// # Errors
///
/// Fails, appending nothing, with whatever `build` itself refuses with, or when the journal
/// cannot be read or written.
pub(crate) fn decide_and_append<T, E>(
    journal: &dyn Journal,
    build: impl Fn(&QueueState) -> Result<(Vec<Event>, T), E>,
) -> Result<T, E>
where
    E: From<JournalError>,
{
    loop {
        let events = journal.events().map_err(E::from)?;
        let state = QueueState::fold(&events);
        let (new_events, result) = build(&state)?;
        match journal.append_events(&new_events, events.len()) {
            Ok(()) => return Ok(result),
            Err(AppendConflict::Conflict) => {}
            Err(AppendConflict::Journal(error)) => return Err(E::from(error)),
        }
    }
}

/// Reads the queue's events and returns what `query` computes from the state they fold to.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn read_and_query<T>(
    journal: &dyn Journal,
    query: impl FnOnce(&QueueState) -> T,
) -> Result<T, JournalError> {
    let events = journal.events()?;
    Ok(query(&QueueState::fold(&events)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{AddError, TaskKind, add_task, remove_task};

    use super::*;

    /// A small, deterministic pseudo-random generator (xorshift32), so the property test
    /// below is reproducible without an external dependency.
    struct Rng(u32);

    impl Rng {
        fn new(seed: u32) -> Self {
            Self(seed | 1)
        }

        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() as usize) % bound
        }
    }

    /// What the test independently expects the queue to hold: every task ever added, in
    /// order, with its current status — `pending`, `cancelled`, or whatever its one possible
    /// attempt (begun, then reported and ended, or not) left it at — and whether that one
    /// attempt was ever begun, which `status` alone does not say: a task cancelled straight
    /// from pending never had one, even though its status then matches a task cancelled after
    /// its attempt ended.
    #[derive(Debug, Clone, Default)]
    struct Oracle {
        entries: Vec<(TaskId, String, TaskStatus, bool)>,
    }

    impl Oracle {
        fn index_of(&self, id: TaskId) -> Option<usize> {
            self.entries.iter().position(|(entry, ..)| *entry == id)
        }

        fn status_of(&self, id: TaskId) -> TaskStatus {
            self.index_of(id)
                .map_or(TaskStatus::Pending, |index| self.entries[index].2)
        }

        fn attempted(&self, id: TaskId) -> bool {
            self.index_of(id).is_some_and(|index| self.entries[index].3)
        }

        fn is_cancelled(&self, id: TaskId) -> bool {
            self.status_of(id) == TaskStatus::Cancelled
        }

        fn add(&mut self, id: TaskId, title: &str, placement: Placement) {
            let index = match placement {
                Placement::End => self.entries.len(),
                Placement::Before(anchor) => self.index_of(anchor).unwrap_or(self.entries.len()),
                Placement::After(anchor) => {
                    self.index_of(anchor).map_or(self.entries.len(), |i| i + 1)
                }
            };
            self.entries
                .insert(index, (id, title.to_owned(), TaskStatus::Pending, false));
        }

        fn set_status(&mut self, id: TaskId, status: TaskStatus) {
            if let Some(index) = self.index_of(id) {
                self.entries[index].2 = status;
            }
        }

        fn mark_attempted(&mut self, id: TaskId) {
            if let Some(index) = self.index_of(id) {
                self.entries[index].3 = true;
            }
        }
    }

    /// The outcome `record_report` and `end_attempt` are refused with for task `id`'s attempt
    /// `number`, given whether it was ever begun and, if so, its current `status` — `id`'s one
    /// possible attempt is always numbered 1, since ending it leaves the task no longer
    /// pending, so it can never begin another.
    fn expected_attempt_refusal(
        id: TaskId,
        number: u32,
        attempted: bool,
        status: TaskStatus,
    ) -> RecordReportError {
        if !attempted || status == TaskStatus::Pending {
            RecordReportError::UnknownAttempt { task: id, number }
        } else {
            RecordReportError::AttemptEnded { task: id, number }
        }
    }

    /// One randomly generated command against a queue that already holds `existing` ids,
    /// applied to both `journal` (through the real use cases) and `oracle` (independently),
    /// asserting the use case's own outcome matches what the oracle expects.
    #[allow(clippy::too_many_lines)]
    fn apply_random_command(
        rng: &mut Rng,
        journal: &FakeJournal,
        clock: &FakeClock,
        oracle: &mut Oracle,
        existing: &mut Vec<TaskId>,
        next_title: &mut u32,
    ) {
        if !existing.is_empty() {
            match rng.below(10) {
                // One command in ten is a cancel.
                0 => {
                    let id = existing[rng.below(existing.len())];
                    let status = oracle.status_of(id);
                    let result = remove_task(journal, clock, id);
                    match status {
                        TaskStatus::Cancelled => {
                            assert_eq!(result, Err(CancelError::AlreadyCancelled(id)));
                        }
                        TaskStatus::Running => {
                            assert_eq!(result, Err(CancelError::Running(id)));
                        }
                        _ => {
                            assert_eq!(result, Ok(()));
                            oracle.set_status(id, TaskStatus::Cancelled);
                        }
                    }
                    return;
                }
                // One in ten begins the attempt a pending task does not have yet.
                1 => {
                    let id = existing[rng.below(existing.len())];
                    let status = oracle.status_of(id);
                    let result = crate::attempt::begin_attempt(journal, clock, id);
                    if status == TaskStatus::Pending {
                        assert_eq!(result, Ok(1));
                        oracle.set_status(id, TaskStatus::Running);
                        oracle.mark_attempted(id);
                    } else {
                        assert_eq!(result, Err(BeginAttemptError::NotPending(id)));
                    }
                    return;
                }
                // One in ten reports an outcome — recorded only while the attempt runs, never
                // changing the task's status itself.
                2 => {
                    let id = existing[rng.below(existing.len())];
                    let status = oracle.status_of(id);
                    let outcome = [
                        Outcome::Done,
                        Outcome::Failed,
                        Outcome::NeedsInput,
                        Outcome::TooLarge,
                    ][rng.below(4)];
                    let result = crate::attempt::record_report(
                        journal,
                        clock,
                        id,
                        1,
                        outcome,
                        Some("why"),
                        None,
                        false,
                        false,
                    );
                    if status == TaskStatus::Running {
                        assert_eq!(result, Ok(()));
                    } else {
                        let attempted = oracle.attempted(id);
                        assert_eq!(
                            result,
                            Err(expected_attempt_refusal(id, 1, attempted, status))
                        );
                    }
                    return;
                }
                // One in ten ends the attempt, settling the task at the outcome given.
                3 => {
                    let id = existing[rng.below(existing.len())];
                    let status = oracle.status_of(id);
                    let ends_at = [
                        TaskStatus::Done,
                        TaskStatus::Failed,
                        TaskStatus::Blocked,
                        TaskStatus::FailedUnknown,
                    ][rng.below(4)];
                    let run = AttemptRun {
                        duration: Duration::ZERO,
                        exit_code: Some(0),
                        status: ends_at,
                        reason: None,
                    };
                    let result = crate::attempt::end_attempt(journal, id, 1, run, clock.0);
                    if status == TaskStatus::Running {
                        assert_eq!(result, Ok(()));
                        oracle.set_status(id, ends_at);
                    } else {
                        let attempted = oracle.attempted(id);
                        assert_eq!(
                            result,
                            Err(expected_attempt_refusal(id, 1, attempted, status))
                        );
                    }
                    return;
                }
                _ => {}
            }
        }
        let title = format!("t{next_title}");
        *next_title += 1;
        let placement = if existing.is_empty() || rng.below(3) == 0 {
            Placement::End
        } else {
            let anchor = existing[rng.below(existing.len())];
            if rng.below(2) == 0 {
                Placement::Before(anchor)
            } else {
                Placement::After(anchor)
            }
        };
        let anchor_cancelled = match placement {
            Placement::Before(anchor) | Placement::After(anchor) => oracle.is_cancelled(anchor),
            Placement::End => false,
        };
        let result = add_task(journal, clock, &draft(&title), placement);
        if anchor_cancelled {
            let (Placement::Before(anchor) | Placement::After(anchor)) = placement else {
                unreachable!("checked above")
            };
            assert_eq!(result, Err(vec![AddError::CancelledTask(anchor)]));
        } else {
            let added = result.unwrap();
            oracle.add(added.id, &title, placement);
            existing.push(added.id);
        }
    }

    #[test]
    fn folding_the_recorded_events_gives_the_same_queue_the_commands_produced() {
        for seed in 0..200_u32 {
            let mut rng = Rng::new(seed.wrapping_mul(2_654_435_761).wrapping_add(1));
            let journal = FakeJournal::default();
            let clock = FakeClock(at(0));
            let mut oracle = Oracle::default();
            let mut existing = Vec::new();
            let mut next_title = 0;
            let steps = 1 + rng.below(40);
            for _ in 0..steps {
                apply_random_command(
                    &mut rng,
                    &journal,
                    &clock,
                    &mut oracle,
                    &mut existing,
                    &mut next_title,
                );
            }

            let events = journal.events().unwrap();
            let folded = QueueState::fold(&events).into_tasks();

            let from_events: Vec<(TaskId, String, TaskStatus)> = folded
                .iter()
                .map(|task| (task.id, task.title.clone(), task.status))
                .collect();
            let expected: Vec<(TaskId, String, TaskStatus)> = oracle
                .entries
                .iter()
                .map(|(id, title, status, _)| (*id, title.clone(), *status))
                .collect();
            assert_eq!(from_events, expected, "seed {seed}");
            let positions: Vec<usize> = folded.iter().map(|task| task.position).collect();
            let expected_positions: Vec<usize> = (1..=folded.len()).collect();
            assert_eq!(positions, expected_positions, "seed {seed}");
        }
    }

    #[test]
    fn folding_from_scratch_matches_a_batch_added_together() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        crate::import_tasks(
            &journal,
            &clock,
            r#"[{"title":"x","criteria":["c"]},{"title":"y","criteria":["c"]}]"#,
            Placement::End,
        )
        .unwrap();
        add_task(
            &journal,
            &clock,
            &TaskDraft {
                kind: TaskKind::Human,
                ..draft("b")
            },
            Placement::Before(TaskId(1)),
        )
        .unwrap();
        remove_task(&journal, &clock, TaskId(2)).unwrap();

        let events = journal.events().unwrap();
        let folded = QueueState::fold(&events).into_tasks();
        let expected = crate::list_all_tasks(&journal).unwrap();
        assert_eq!(folded, expected);
    }

    #[test]
    fn a_gate_failure_folds_to_the_tasks_current_gate_stop() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        journal
            .append_events(
                &[Event::GateFailed {
                    id: TaskId(1),
                    step: "sync".to_owned(),
                    reason: "uncommitted changes".to_owned(),
                    at: at(1),
                }],
                journal.events().unwrap().len(),
            )
            .unwrap();

        let state = QueueState::fold(&journal.events().unwrap());
        assert_eq!(
            state.gate_stop_of(TaskId(1)),
            Some(("sync".to_owned(), "uncommitted changes".to_owned()))
        );
        // The task itself stays pending: a gate stop is not an attempt.
        assert_eq!(state.into_tasks()[0].status, TaskStatus::Pending);
    }

    #[test]
    fn a_later_attempt_beginning_clears_the_tasks_gate_stop() {
        let journal = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&journal, &clock, &draft("a"), Placement::End).unwrap();
        journal
            .append_events(
                &[Event::GateFailed {
                    id: TaskId(1),
                    step: "health check".to_owned(),
                    reason: "exited with code 1".to_owned(),
                    at: at(1),
                }],
                journal.events().unwrap().len(),
            )
            .unwrap();
        crate::attempt::begin_attempt(&journal, &clock, TaskId(1)).unwrap();

        let state = QueueState::fold(&journal.events().unwrap());
        assert_eq!(state.gate_stop_of(TaskId(1)), None);
    }

    #[test]
    fn a_task_never_stopped_by_a_gate_has_none() {
        let journal = FakeJournal::default();
        add_task(&journal, &FakeClock(at(0)), &draft("a"), Placement::End).unwrap();
        let state = QueueState::fold(&journal.events().unwrap());
        assert_eq!(state.gate_stop_of(TaskId(1)), None);
    }
}
