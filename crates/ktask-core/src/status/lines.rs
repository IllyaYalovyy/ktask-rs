//! Labelling one step's own outcome and reason, and the still-running line for a step that
//! has not ended yet — [`super::status`]'s own lowest-level work, pulled out of it so that
//! file stays within the workspace's function-length limit.

use crate::{AttemptEnd, Clock, Outcome, TaskStatus};

use super::{AttemptOutcome, IMPLEMENTATION, RESOLVE_STEP, REVIEW_STEP, StepLine, TEST_STEP};

/// The outcome and reason shown for the implementation step ended at `end`, given what the
/// agent itself reported for it, when it reported anything at all; `answer`, when the report
/// was `needs-input` and this attempt was since given one, is appended to the question it
/// asked.
fn ended_outcome(
    end: &AttemptEnd,
    reported: Option<(Outcome, Option<String>)>,
    answer: Option<&str>,
) -> (AttemptOutcome, Option<String>) {
    match reported {
        Some((Outcome::NeedsInput, reason)) => (
            AttemptOutcome::Reported(Outcome::NeedsInput),
            crate::attempt::with_answer(reason, answer),
        ),
        Some((outcome, reason)) => (AttemptOutcome::Reported(outcome), reason),
        None => (AttemptOutcome::Unreported, end.reason.clone()),
    }
}

/// The outcome and reason shown for a step named `name` that ended at `end`: the
/// implementation, review and test steps are judged by what the agent itself reported, when
/// it reported anything — see [`ended_outcome`] for `answer`; every other step — the sync,
/// the health check, the commit step and the push step, today — is a command-kind step the
/// tool itself ran, shown [`AttemptOutcome::Passed`] when `end.status` is `done`, or
/// [`AttemptOutcome::Failed`] with why, for the steps that can still end an attempt badly
/// because they run inside the attempt itself: the commit step and the push step.
pub(super) fn step_outcome(
    name: &str,
    end: &AttemptEnd,
    reported: Option<(Outcome, Option<String>)>,
    answer: Option<&str>,
) -> (AttemptOutcome, Option<String>) {
    if name == IMPLEMENTATION || name == REVIEW_STEP || name == TEST_STEP || name == RESOLVE_STEP {
        ended_outcome(end, reported, answer)
    } else if end.status == TaskStatus::Done {
        (AttemptOutcome::Passed, end.reason.clone())
    } else {
        (AttemptOutcome::Failed, end.reason.clone())
    }
}

/// The provider named for a step called `name`, given the provider the attempt ran with, when
/// one is known: the implementation, review and test steps are run by an agent, and name it;
/// every other step — the sync, health check, commit and push steps, today — is run by the
/// tool itself, and names none.
pub(super) fn step_provider(name: &str, provider: Option<&str>) -> Option<String> {
    if name == IMPLEMENTATION || name == REVIEW_STEP || name == TEST_STEP || name == RESOLVE_STEP {
        provider.map(str::to_owned)
    } else {
        None
    }
}

/// The still-running step line for a step named `name`, started at `started_at`: its elapsed
/// time so far, and whether it shows `running` or `interrupted` depending on `run_alive`.
pub(super) fn running_step(
    name: &str,
    provider: Option<&str>,
    model: Option<&str>,
    started_at: std::time::SystemTime,
    clock: &impl Clock,
    run_alive: bool,
) -> StepLine {
    let elapsed = clock.now().duration_since(started_at).unwrap_or_default();
    let outcome = if run_alive {
        AttemptOutcome::Running
    } else {
        AttemptOutcome::Interrupted
    };
    StepLine {
        step: name.to_owned(),
        provider: step_provider(name, provider),
        model: model.map(str::to_owned),
        time_spent: elapsed,
        outcome,
        reason: None,
    }
}
