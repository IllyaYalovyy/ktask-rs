//! Translating a [`RunOutcome`] into the process exit status documented in
//! `docs/CONTRACT.md` section 1.
//!
//! The mapping lives here, alone, so nothing else in this crate has to
//! reason about which integer means what: it asks [`code_for`] and exits
//! with the answer.

use ktask_core::RunOutcome;

/// The process exit code for `outcome`, exactly as `docs/CONTRACT.md`
/// section 1 documents it.
///
/// Codes 3, 4 and 5 are pauses, not failures, and — like 130 — must never
/// collapse to 1: only [`RunOutcome::TaskFailed`] and
/// [`RunOutcome::CheckFailed`] do, the two shapes "the command completed and
/// found something wrong" takes.
#[must_use]
pub(crate) fn code_for(outcome: &RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Drained => 0,
        RunOutcome::TaskFailed { .. } | RunOutcome::CheckFailed { .. } => 1,
        RunOutcome::Usage { .. } => 2,
        RunOutcome::ProviderLimit { .. } => 3,
        RunOutcome::HumanGate { .. } => 4,
        RunOutcome::NeedsInput { .. } => 5,
        RunOutcome::Interrupted => 130,
    }
}

#[cfg(test)]
mod tests {
    use super::code_for;
    use ktask_core::{RunOutcome, TaskId};

    /// One row per outcome `docs/CONTRACT.md` section 1 documents, so a
    /// variant added to `RunOutcome` without a row here is caught by
    /// [`every_documented_outcome_is_covered`] rather than silently
    /// defaulting somewhere.
    fn table() -> Vec<(RunOutcome, i32)> {
        vec![
            (RunOutcome::Drained, 0),
            (
                RunOutcome::TaskFailed {
                    task: TaskId::new(1),
                },
                1,
            ),
            (
                RunOutcome::CheckFailed {
                    detail: "git: not runnable".to_string(),
                },
                1,
            ),
            (
                RunOutcome::Usage {
                    detail: "no registered project".to_string(),
                },
                2,
            ),
            (RunOutcome::ProviderLimit { until: None }, 3),
            (
                RunOutcome::HumanGate {
                    task: TaskId::new(1),
                },
                4,
            ),
            (
                RunOutcome::NeedsInput {
                    task: TaskId::new(1),
                },
                5,
            ),
            (RunOutcome::Interrupted, 130),
        ]
    }

    #[test]
    fn exit_code_matches_the_contract_for_every_outcome() {
        for (outcome, expected) in table() {
            assert_eq!(
                code_for(&outcome),
                expected,
                "{outcome:?} must map to exit code {expected}"
            );
        }
    }

    #[test]
    fn every_documented_outcome_is_covered() {
        // Exhaustive, wildcard-free match: a variant added to `RunOutcome`
        // without a row in `table()` fails to compile here instead of
        // silently passing untested.
        for (outcome, _) in table() {
            match outcome {
                RunOutcome::Drained
                | RunOutcome::TaskFailed { .. }
                | RunOutcome::CheckFailed { .. }
                | RunOutcome::Usage { .. }
                | RunOutcome::ProviderLimit { .. }
                | RunOutcome::HumanGate { .. }
                | RunOutcome::NeedsInput { .. }
                | RunOutcome::Interrupted => {}
            }
        }
        assert_eq!(table().len(), 8, "one row per RunOutcome variant");
    }

    #[test]
    fn pauses_never_map_to_the_failure_code() {
        let pauses = [
            RunOutcome::ProviderLimit { until: None },
            RunOutcome::HumanGate {
                task: TaskId::new(7),
            },
            RunOutcome::NeedsInput {
                task: TaskId::new(7),
            },
            RunOutcome::Interrupted,
        ];
        for pause in pauses {
            assert_ne!(
                code_for(&pause),
                1,
                "{pause:?} is a pause, not a failure, and must not exit 1"
            );
        }
    }

    #[test]
    fn only_task_failed_and_check_failed_map_to_the_failure_code() {
        for (outcome, code) in table() {
            let is_a_failure = matches!(
                outcome,
                RunOutcome::TaskFailed { .. } | RunOutcome::CheckFailed { .. }
            );
            assert_eq!(
                code == 1,
                is_a_failure,
                "exit code 1 must be reserved for TaskFailed and CheckFailed, \
                 got {outcome:?} -> {code}"
            );
        }
    }
}
