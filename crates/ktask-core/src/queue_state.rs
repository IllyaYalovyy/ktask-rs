//! The queue's state, and the pure rules for adding, placing and cancelling tasks, and for
//! beginning, running and ending their attempts.
//!
//! This is the one place a command (add, add at a place, cancel, begin an attempt, report,
//! mark running, end an attempt) is turned into events or a refusal, and the one place events
//! are folded into state. No I/O: [`crate::Journal`] only stores and returns the events this
//! module decides.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::journal::{AttemptRun, Event};
use crate::{
    AppendConflict, AppendError, Attempt, AttemptEnd, BeginAttemptError, CancelError, Journal,
    JournalError, Outcome, Placement, RecordReportError, Task, TaskDraft, TaskId, TaskStatus,
};

/// One task's attempt, as folded from its events: the state [`QueueState::attempt_of`] and
/// [`QueueState::running`] read from, keyed by task in [`QueueState::attempts`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptFold {
    number: u32,
    started_at: SystemTime,
    provider: Option<String>,
    ended: Option<AttemptEnd>,
}

/// The queue's state: every task ever added, in queue order, with the cancelled ones marked
/// but kept in place, and every task's most recent attempt — folded from the events recorded
/// for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueueState {
    tasks: Vec<Task>,
    attempts: HashMap<TaskId, AttemptFold>,
    reports: HashMap<(TaskId, u32), (Outcome, Option<String>)>,
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
            Event::TaskAdded {
                id,
                draft,
                placement,
                at,
            } => {
                let index = self.insertion_index(*placement);
                self.tasks.insert(
                    index,
                    Task {
                        id: *id,
                        position: 0,
                        title: draft.title.clone(),
                        body: draft.body.clone(),
                        criteria: draft.criteria.clone(),
                        kind: draft.kind,
                        links: draft.links.clone(),
                        status: TaskStatus::Pending,
                        created_at: *at,
                    },
                );
                self.renumber();
            }
            Event::TaskCancelled { id, .. } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
                    task.status = TaskStatus::Cancelled;
                }
            }
            Event::AttemptStarted { id, number, at } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
                    task.status = TaskStatus::Running;
                }
                self.attempts.insert(
                    *id,
                    AttemptFold {
                        number: *number,
                        started_at: *at,
                        provider: None,
                        ended: None,
                    },
                );
            }
            Event::AttemptRunning {
                id,
                number,
                provider,
                ..
            } => {
                if let Some(attempt) = self.attempts.get_mut(id)
                    && attempt.number == *number
                {
                    attempt.provider = Some(provider.clone());
                }
            }
            Event::AttemptReported {
                id,
                number,
                outcome,
                reason,
                ..
            } => {
                self.reports
                    .insert((*id, *number), (*outcome, reason.clone()));
            }
            Event::AttemptEnded {
                id,
                number,
                duration,
                status,
                reason,
                ..
            } => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
                    task.status = *status;
                }
                if let Some(attempt) = self.attempts.get_mut(id)
                    && attempt.number == *number
                {
                    attempt.ended = Some(AttemptEnd {
                        duration: *duration,
                        status: *status,
                        reason: reason.clone(),
                    });
                }
            }
        }
    }

    /// Where a task placed at `placement` goes, if `placement` names a task — the caller
    /// checks first that it exists and is not cancelled; a missing anchor here falls back to
    /// the end, so folding never panics on a foreign or corrupt event.
    fn insertion_index(&self, placement: Placement) -> usize {
        match placement {
            Placement::End => self.tasks.len(),
            Placement::Before(anchor) => self.index_of(anchor).unwrap_or(self.tasks.len()),
            Placement::After(anchor) => self.index_of(anchor).map_or(self.tasks.len(), |i| i + 1),
        }
    }

    fn index_of(&self, id: TaskId) -> Option<usize> {
        self.tasks.iter().position(|task| task.id == id)
    }

    fn renumber(&mut self) {
        for (index, task) in self.tasks.iter_mut().enumerate() {
            task.position = index + 1;
        }
    }

    /// The number the next task added to this state would get: one past the highest ever
    /// used, cancelled tasks included, so a number is never reused.
    fn next_id(&self) -> TaskId {
        TaskId(self.tasks.iter().map(|task| task.id.0).max().unwrap_or(0) + 1)
    }

    /// Checks that `placement` names a task that exists and is not cancelled, when it names
    /// one at all.
    fn check_placement(&self, placement: Placement) -> Result<(), AppendError> {
        let anchor = match placement {
            Placement::End => return Ok(()),
            Placement::Before(anchor) | Placement::After(anchor) => anchor,
        };
        match self.tasks.iter().find(|task| task.id == anchor) {
            None => Err(AppendError::UnknownTask(anchor)),
            Some(task) if task.status == TaskStatus::Cancelled => {
                Err(AppendError::CancelledTask(anchor))
            }
            Some(_) => Ok(()),
        }
    }

    /// The command "add `drafts`, together, at `placement`": the events it produces and the
    /// tasks they add, or the reason `placement` is invalid. Does not check `drafts` against
    /// the queue's own rules (title, criteria, links) — the caller has done that.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when `placement` names a task that does not exist or was
    /// cancelled.
    pub(crate) fn decide_add(
        &self,
        drafts: &[TaskDraft],
        placement: Placement,
        at: SystemTime,
    ) -> Result<(Vec<Event>, Vec<Task>), AppendError> {
        self.check_placement(placement)?;
        let mut state = self.clone();
        let mut events = Vec::with_capacity(drafts.len());
        let mut placed_at = placement;
        for draft in drafts {
            let id = state.next_id();
            let event = Event::TaskAdded {
                id,
                draft: draft.clone(),
                placement: placed_at,
                at,
            };
            state.apply(&event);
            placed_at = placed_at.then_after(id);
            events.push(event);
        }
        let added = events
            .iter()
            .filter_map(|event| match event {
                Event::TaskAdded { id, .. } => {
                    state.tasks.iter().find(|task| task.id == *id).cloned()
                }
                _ => None,
            })
            .collect();
        Ok((events, added))
    }

    /// The command "cancel `id`": the event it produces, or the reason it cannot be.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task or it is cancelled already.
    pub(crate) fn decide_cancel(&self, id: TaskId, at: SystemTime) -> Result<Event, CancelError> {
        match self.tasks.iter().find(|task| task.id == id) {
            None => Err(CancelError::UnknownTask(id)),
            Some(task) if task.status == TaskStatus::Cancelled => {
                Err(CancelError::AlreadyCancelled(id))
            }
            Some(_) => Ok(Event::TaskCancelled { id, at }),
        }
    }

    /// Every task, cancelled ones included, in queue order.
    pub(crate) fn into_tasks(self) -> Vec<Task> {
        self.tasks
    }

    /// The task numbered `id`'s current attempt number: `0` when it was never attempted.
    fn current_attempt_number(&self, id: TaskId) -> u32 {
        self.attempts.get(&id).map_or(0, |attempt| attempt.number)
    }

    /// The command "begin the next attempt at `id`": the event it produces and the attempt's
    /// number, or the reason it cannot start.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task or it is not pending.
    pub(crate) fn decide_begin_attempt(
        &self,
        id: TaskId,
        at: SystemTime,
    ) -> Result<(Vec<Event>, u32), BeginAttemptError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(BeginAttemptError::UnknownTask(id))?;
        if task.status != TaskStatus::Pending {
            return Err(BeginAttemptError::NotPending(id));
        }
        let number = self.current_attempt_number(id) + 1;
        Ok((vec![Event::AttemptStarted { id, number, at }], number))
    }

    /// Checks that attempt `number` of task `id` is the one currently running: the caller of
    /// [`QueueState::decide_record_report`] and [`QueueState::decide_end_attempt`] shares this
    /// rule.
    fn check_attempt_running(&self, id: TaskId, number: u32) -> Result<(), RecordReportError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(RecordReportError::UnknownAttempt { task: id, number })?;
        if self.current_attempt_number(id) != number {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        }
        if task.status != TaskStatus::Running {
            return Err(RecordReportError::AttemptEnded { task: id, number });
        }
        Ok(())
    }

    /// The command "record `outcome` (and `reason`) for attempt `number` of task `id`": the
    /// event it produces, or the reason it cannot be recorded.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` was started for this task, or
    /// when it was but has since ended.
    pub(crate) fn decide_record_report(
        &self,
        id: TaskId,
        number: u32,
        outcome: Outcome,
        reason: Option<&str>,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptReported {
            id,
            number,
            outcome,
            reason: reason.map(str::to_owned),
            at,
        })
    }

    /// The command "end attempt `number` of task `id` with `run`": the event it produces, or
    /// the reason it cannot end.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    pub(crate) fn decide_end_attempt(
        &self,
        id: TaskId,
        number: u32,
        run: AttemptRun<'_>,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptEnded {
            id,
            number,
            duration: run.duration,
            exit_code: run.exit_code,
            status: run.status,
            reason: run.reason.map(str::to_owned),
            at,
        })
    }

    /// The task currently running, and its current attempt's number — a project has at most
    /// one at a time. `None` when none is running.
    pub(crate) fn running(&self) -> Option<(TaskId, u32)> {
        self.tasks
            .iter()
            .find(|task| task.status == TaskStatus::Running)
            .map(|task| (task.id, self.current_attempt_number(task.id)))
    }

    /// The most recent attempt at task `id`. `None` when it was never attempted.
    pub(crate) fn attempt_of(&self, id: TaskId) -> Option<Attempt> {
        self.attempts.get(&id).map(|attempt| Attempt {
            number: attempt.number,
            started_at: attempt.started_at,
            provider: attempt.provider.clone(),
            ended: attempt.ended.clone(),
        })
    }

    /// The most recent outcome and reason the agent itself reported for attempt `number` of
    /// task `id`. `None` when it reported nothing.
    pub(crate) fn report_of(&self, id: TaskId, number: u32) -> Option<(Outcome, Option<String>)> {
        self.reports.get(&(id, number)).cloned()
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
    journal: &impl Journal,
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
    journal: &impl Journal,
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
                    let already = oracle.is_cancelled(id);
                    let result = remove_task(journal, clock, id);
                    if already {
                        assert_eq!(result, Err(CancelError::AlreadyCancelled(id)));
                    } else {
                        assert_eq!(result, Ok(()));
                        oracle.set_status(id, TaskStatus::Cancelled);
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
                    let result =
                        crate::attempt::record_report(journal, clock, id, 1, outcome, Some("why"));
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
            assert_eq!(result, Err(AddError::CancelledTask(anchor)));
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
}
