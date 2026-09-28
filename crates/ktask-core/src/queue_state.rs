//! The queue's state, and the pure rules for adding, placing and cancelling tasks.
//!
//! This is the one place a command (add, add at a place, cancel) is turned into events or a
//! refusal, and the one place events are folded into state. No I/O: [`crate::Journal`] only
//! stores and returns the events this module decides.

use std::time::SystemTime;

use crate::journal::Event;
use crate::{AppendError, CancelError, Placement, Task, TaskDraft, TaskId, TaskStatus};

/// The queue's state: every task ever added, in queue order, with the cancelled ones marked
/// but kept in place — folded from the events recorded for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueueState {
    tasks: Vec<Task>,
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
                Event::TaskCancelled { .. } => None,
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
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{AddError, Journal as _, TaskKind, add_task, remove_task};

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
    /// order, with a flag for whether it was cancelled.
    #[derive(Debug, Clone, Default)]
    struct Oracle {
        entries: Vec<(TaskId, String, bool)>,
    }

    impl Oracle {
        fn index_of(&self, id: TaskId) -> Option<usize> {
            self.entries.iter().position(|(entry, ..)| *entry == id)
        }

        fn is_cancelled(&self, id: TaskId) -> bool {
            self.index_of(id).is_some_and(|index| self.entries[index].2)
        }

        fn add(&mut self, id: TaskId, title: &str, placement: Placement) {
            let index = match placement {
                Placement::End => self.entries.len(),
                Placement::Before(anchor) => self.index_of(anchor).unwrap_or(self.entries.len()),
                Placement::After(anchor) => {
                    self.index_of(anchor).map_or(self.entries.len(), |i| i + 1)
                }
            };
            self.entries.insert(index, (id, title.to_owned(), false));
        }

        fn cancel(&mut self, id: TaskId) {
            if let Some(index) = self.index_of(id) {
                self.entries[index].2 = true;
            }
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
        // One command in four is a cancel, when there is anything left to cancel.
        if !existing.is_empty() && rng.below(4) == 0 {
            let id = existing[rng.below(existing.len())];
            let already = oracle.is_cancelled(id);
            let result = remove_task(journal, clock, id);
            if already {
                assert_eq!(result, Err(CancelError::AlreadyCancelled(id)));
            } else {
                assert_eq!(result, Ok(()));
                oracle.cancel(id);
            }
            return;
        }
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
            let steps = 1 + rng.below(25);
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

            let from_events: Vec<(TaskId, String, bool)> = folded
                .iter()
                .map(|task| {
                    (
                        task.id,
                        task.title.clone(),
                        task.status == TaskStatus::Cancelled,
                    )
                })
                .collect();
            assert_eq!(from_events, oracle.entries, "seed {seed}");
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
