//! Exit code mapping from outcomes to process exit status.
//!
//! Maps `RunOutcome` to exit codes per docs/CONTRACT.md section 1:
//! - 0: success (queue drained)
//! - 1: a task failed after its remediation budget
//! - 2: usage error (bad arguments, malformed input)
//! - 3: provider limit reached (work paused, not failed)
//! - 4: stopped at a human gate
//! - 5: stopped needing input on a decision
//! - 130: interrupted (SIGINT)

use ktask_core::RunOutcome;

/// Maps a `RunOutcome` to its corresponding process exit code.
///
/// Exit codes 3, 4, and 5 are pauses, not failures. They must never mark a
/// task as failed; the queue is suspended and can be resumed.
pub(crate) fn code_for(outcome: &RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Drained => 0,
        RunOutcome::TaskFailed { .. } => 1,
        RunOutcome::Usage { .. } => 2,
        RunOutcome::ProviderLimit { .. } => 3,
        RunOutcome::HumanGate { .. } => 4,
        RunOutcome::NeedsInput { .. } => 5,
        RunOutcome::Interrupted => 130,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::TaskId;

    #[test]
    fn exit_codes_match_contract() {
        // Table-driven test covering all documented outcomes per docs/CONTRACT.md section 1.
        let test_cases = vec![
            (RunOutcome::Drained, 0, "success: queue drained"),
            (
                RunOutcome::TaskFailed {
                    task: TaskId::new(1),
                },
                1,
                "task failed after remediation budget",
            ),
            (
                RunOutcome::Usage {
                    detail: "bad arguments".to_string(),
                },
                2,
                "usage error: bad arguments, malformed input",
            ),
            (
                RunOutcome::ProviderLimit { until: None },
                3,
                "provider limit reached (paused, not failed)",
            ),
            (
                RunOutcome::HumanGate {
                    task: TaskId::new(2),
                },
                4,
                "stopped at human gate",
            ),
            (
                RunOutcome::NeedsInput {
                    task: TaskId::new(3),
                },
                5,
                "stopped needing input on decision",
            ),
            (RunOutcome::Interrupted, 130, "interrupted by SIGINT"),
        ];

        for (outcome, expected_code, description) in test_cases {
            let actual_code = code_for(&outcome);
            assert_eq!(
                actual_code, expected_code,
                "Mismatch for {description}: expected code {expected_code}, got {actual_code}",
            );
        }
    }

    #[test]
    fn pause_outcomes_never_map_to_one() {
        // Verify that pause outcomes (3, 4, 5) are never mapped to 1 (failure).
        let pause_outcomes = vec![
            RunOutcome::ProviderLimit { until: None },
            RunOutcome::HumanGate {
                task: TaskId::new(1),
            },
            RunOutcome::NeedsInput {
                task: TaskId::new(1),
            },
        ];

        for outcome in pause_outcomes {
            let code = code_for(&outcome);
            assert!(
                code != 1,
                "Pause outcome mapped to 1 (failure): {outcome:?}",
            );
            assert!(
                code == 3 || code == 4 || code == 5,
                "Pause outcome mapped to unexpected code {code}: {outcome:?}",
            );
        }
    }
}
