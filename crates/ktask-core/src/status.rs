//! The status use case: reads journal history into the facts an interface needs to show it.

use std::time::Duration;

use crate::{AttemptOutput, Clock, Journal, JournalError, RunLock, list_all_tasks};

mod build;
mod facts;
mod lines;
mod outcome;

pub use facts::{AttemptLine, DoneMark, OutputActivity, StatusEntry, StepLine, Wait};
pub use outcome::AttemptOutcome;

/// Reads every task with recorded status history, in queue order.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when the run lock cannot be used.
pub fn status(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
) -> Result<Vec<StatusEntry>, JournalError> {
    status_inner(journal, clock, lock, None, Duration::MAX)
}

/// Reads status history and the live provider-output facts for running attempts.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when the run lock cannot be used.
pub fn status_with_output(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: &impl AttemptOutput,
    silent_after: Duration,
) -> Result<Vec<StatusEntry>, JournalError> {
    status_inner(journal, clock, lock, Some(output), silent_after)
}

/// The journal read shared by the status views, repeated until the journal held still across
/// it: whether a run is alive is judged from the lock at one instant, so it only says anything
/// about the entries built from a journal that did not change meanwhile. A run that starts in
/// between would otherwise be read as one that is not alive, and shown `interrupted`.
fn status_inner(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: Option<&dyn AttemptOutput>,
    silent_after: Duration,
) -> Result<Vec<StatusEntry>, JournalError> {
    loop {
        let events_before = journal.events()?.len();
        let entries = read_entries(journal, clock, lock, output, silent_after)?;
        if journal.events()?.len() == events_before {
            return Ok(entries);
        }
    }
}

fn read_entries(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    output: Option<&dyn AttemptOutput>,
    silent_after: Duration,
) -> Result<Vec<StatusEntry>, JournalError> {
    let run_alive = match crate::attempt::running(journal)? {
        Some(_) => lock
            .in_progress()
            .map_err(|error| JournalError::new(error.to_string()))?,
        None => false,
    };
    let mut entries = Vec::new();
    for task in list_all_tasks(journal)? {
        if let Some(entry) =
            build::entry_for_task(journal, task, clock, run_alive, output, silent_after)?
        {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::fakes::{FakeClock, FakeJournal, at, draft};
    use crate::{
        AttemptOutcome, Event, Placement, RunLock, RunLockError, TaskId, add_task, status,
    };

    use super::*;

    /// A journal that, right after status has looked for a running attempt and found none,
    /// has a run start on its task: the run takes the lock and only then records the attempt,
    /// as `ktask-rs run` does.
    struct RunStartsAfterFirstRead<'a> {
        inner: &'a FakeJournal,
        clock: &'a FakeClock,
        reads: Cell<u32>,
        lock_taken: &'a Cell<bool>,
    }

    impl Journal for RunStartsAfterFirstRead<'_> {
        fn events(&self) -> Result<Vec<Event>, JournalError> {
            let events = self.inner.events();
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                self.lock_taken.set(true);
                crate::attempt::begin_attempt_running(
                    self.inner,
                    self.clock,
                    TaskId(1),
                    "echo",
                    None,
                )
                .unwrap();
            }
            events
        }

        fn append_events(
            &self,
            events: &[Event],
            read: usize,
        ) -> Result<(), crate::journal::AppendConflict> {
            self.inner.append_events(events, read)
        }
    }

    struct LockTaken<'a>(&'a Cell<bool>);

    impl RunLock for LockTaken<'_> {
        fn acquire(&self) -> Result<(), RunLockError> {
            unreachable!("status never takes the lock")
        }

        fn in_progress(&self) -> Result<bool, RunLockError> {
            Ok(self.0.get())
        }
    }

    #[test]
    fn a_run_that_starts_while_status_reads_is_shown_running_not_interrupted() {
        let inner = FakeJournal::default();
        let clock = FakeClock(at(0));
        add_task(&inner, &clock, &draft("a"), Placement::End).unwrap();
        let lock_taken = Cell::new(false);
        let journal = RunStartsAfterFirstRead {
            inner: &inner,
            clock: &clock,
            reads: Cell::new(0),
            lock_taken: &lock_taken,
        };

        let entries = status(&journal, &clock, &LockTaken(&lock_taken)).unwrap();

        assert_eq!(entries[0].attempt.outcome, AttemptOutcome::Running);
    }
}
