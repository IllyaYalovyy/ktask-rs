//! How many of a task's attempts count against `max-attempts`.

use crate::{Journal, JournalError, TaskId, TaskStatus};

/// How many of task `id`'s attempts, up to and including `number`, are weighed against
/// `max_attempts` — every one of them except an attempt that ended `pending`, a stop verdict's
/// own ending: the environment stopped it, not the task, so it is never counted as one of the
/// task's attempts. `number` itself is always counted as one, since the caller only asks once
/// it knows this attempt did not end that way itself.
///
/// # Errors
///
/// Fails when the journal cannot be read.
pub(crate) fn real_attempt_count(
    journal: &dyn Journal,
    id: TaskId,
    number: u32,
) -> Result<u32, JournalError> {
    let earlier_real = crate::attempt::all_attempts(journal, id)?
        .into_iter()
        .filter(|attempt| attempt.number < number)
        .filter(|attempt| {
            !matches!(
                attempt.ended.as_ref().map(|end| end.status),
                Some(TaskStatus::Pending)
            )
        })
        .count();
    Ok(u32::try_from(earlier_real)
        .unwrap_or(u32::MAX)
        .saturating_add(1))
}
