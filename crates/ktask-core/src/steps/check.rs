//! The check step: runs the project's own check command after the implementation, so that
//! whether the agent's work stands is established by the tool, never by the agent's word.

use std::time::Duration;

use crate::route::Signals;
use crate::steps::{Deps, INTERRUPTED, PipelineState, Step, StepOutcome};
use crate::{CommandSpec, CommandsError, Exit, Output, RunContext, RunError, TaskStatus};

/// The journal name of the check step.
pub const CHECK_STEP: &str = "check";

/// How many lines of a failing check's output the decider is shown.
const OUTPUT_TAIL_LINES: usize = 60;

/// Whether the check is switched on for `context`: a command is set, and the step is not named in
/// `context.disabled_steps`.
fn enabled(context: RunContext<'_>) -> bool {
    context.check_command.is_some() && context.step_enabled(CHECK_STEP)
}

/// The check step of a task's attempt.
pub(crate) struct Check;

impl Step for Check {
    fn name(&self) -> &'static str {
        CHECK_STEP
    }

    fn enabled(&self, context: RunContext<'_>, _state: &PipelineState<'_>) -> bool {
        enabled(context)
    }

    fn attempt_reason(&self, step_reason: Option<String>) -> Option<String> {
        step_reason.map(|reason| format!("check failed ({reason})"))
    }

    fn run(
        &self,
        deps: &Deps<'_>,
        context: RunContext<'_>,
        state: &mut PipelineState<'_>,
    ) -> Result<StepOutcome, RunError> {
        let Some(command) = context.check_command else {
            unreachable!("Check::enabled only returns true when a command is configured");
        };
        let spec = CommandSpec {
            program: "bash".to_owned(),
            args: vec!["-c".to_owned(), command.to_owned()],
            dir: context.project_dir.to_owned(),
            stdin: Vec::new(),
            timeout: context.attempt_timeout,
            output_path: Some(context.step_output_path(state.token, CHECK_STEP)),
        };
        let started = deps.clock.now();
        let result = deps.commands.run(&spec);
        let duration = deps.clock.now().duration_since(started).unwrap_or_default();
        Ok(check_outcome(result, duration))
    }
}

/// What running the check produced, as a step outcome.
fn check_outcome(result: Result<Output, CommandsError>, duration: Duration) -> StepOutcome {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            let reason = format!("the check could not be run: {error}");
            return unfinished(duration, reason);
        }
    };
    match output.exit {
        Exit::Code(0) => StepOutcome::Passed {
            duration,
            exit_code: Some(0),
            reason: None,
            reported: None,
            routed: None,
        },
        Exit::Code(code) => failed(duration, Some(code), format!("exit {code}"), &output),
        Exit::Killed => failed(
            duration,
            None,
            "ran past its time limit and was killed".to_owned(),
            &output,
        ),
        Exit::Interrupted => unfinished(duration, INTERRUPTED.to_owned()),
    }
}

/// The check ran and did not pass: `output`'s tail goes to the router.
fn failed(
    duration: Duration,
    exit_code: Option<i32>,
    reason: String,
    output: &Output,
) -> StepOutcome {
    ended(
        duration,
        exit_code,
        TaskStatus::Failed,
        reason,
        Some(output.tail(OUTPUT_TAIL_LINES)),
    )
}

/// The check never gave an answer, so there is no output for the decider to read.
fn unfinished(duration: Duration, reason: String) -> StepOutcome {
    ended(duration, None, TaskStatus::FailedUnknown, reason, None)
}

fn ended(
    duration: Duration,
    exit_code: Option<i32>,
    status: TaskStatus,
    reason: String,
    check_output: Option<String>,
) -> StepOutcome {
    StepOutcome::Ended {
        duration,
        exit_code,
        status,
        reason: Some(reason),
        reported: None,
        signals: Signals {
            check_output,
            ..Signals::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn context() -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(42),
            health_check_command: None,
            check_command: Some("make check"),
            tracked_branch: None,
            disabled_steps: &[],
            max_attempts: 1,
            transport_retries: 3,
            model: "",
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
            outputs_dir: Path::new("/state/outputs"),
            instructions_dir: "docs",
        }
    }

    fn output(stdout: &str, stderr: &str, exit: Exit) -> Output {
        Output {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            exit,
        }
    }

    fn ended(outcome: StepOutcome) -> (Option<i32>, TaskStatus, Option<String>, Signals) {
        match outcome {
            StepOutcome::Ended {
                exit_code,
                status,
                reason,
                signals,
                ..
            } => (exit_code, status, reason, signals),
            StepOutcome::Passed { .. } => panic!("expected the check to end the attempt"),
        }
    }

    #[test]
    fn it_runs_only_with_a_command_set_and_the_switch_on() {
        assert!(enabled(context()));
        let mut unset = context();
        unset.check_command = None;
        assert!(!enabled(unset));
        let mut off = context();
        off.disabled_steps = &[CHECK_STEP];
        assert!(!enabled(off));
    }

    #[test]
    fn a_zero_exit_passes_with_nothing_to_say() {
        let outcome = check_outcome(Ok(output("all green\n", "", Exit::Code(0))), Duration::ZERO);
        assert!(matches!(
            outcome,
            StepOutcome::Passed {
                exit_code: Some(0),
                reason: None,
                ..
            }
        ));
    }

    #[test]
    fn a_nonzero_exit_fails_with_exit_n_and_keeps_the_last_sixty_lines() {
        let lines = (1..=70)
            .map(|n| format!("line {n}\n"))
            .fold(String::new(), |all, line| all + &line);
        let (exit_code, status, reason, signals) = ended(check_outcome(
            Ok(output(&lines, "boom\n", Exit::Code(2))),
            Duration::ZERO,
        ));
        assert_eq!(exit_code, Some(2));
        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(reason.as_deref(), Some("exit 2"));
        let tail = signals.check_output.expect("the output tail");
        assert_eq!(tail.lines().count(), 60);
        assert!(tail.starts_with("line 12\n"), "{tail}");
        assert!(tail.ends_with("line 70\nboom"), "{tail}");
    }

    #[test]
    fn a_check_past_its_time_limit_fails_as_killed() {
        let (exit_code, status, reason, signals) = ended(check_outcome(
            Ok(output("", "", Exit::Killed)),
            Duration::ZERO,
        ));
        assert_eq!(exit_code, None);
        assert_eq!(status, TaskStatus::Failed);
        assert!(reason.is_some_and(|reason| reason.contains("time limit")));
        assert!(signals.check_output.is_some());
    }

    #[test]
    fn a_check_that_cannot_be_run_or_was_interrupted_has_no_output_to_show() {
        let (_, status, reason, signals) = ended(check_outcome(
            Err(CommandsError::new("bash not found")),
            Duration::ZERO,
        ));
        assert_eq!(status, TaskStatus::FailedUnknown);
        assert!(reason.is_some_and(|reason| reason.contains("bash not found")));
        assert_eq!(signals.check_output, None);

        let (_, status, reason, signals) = ended(check_outcome(
            Ok(output("", "", Exit::Interrupted)),
            Duration::ZERO,
        ));
        assert_eq!(status, TaskStatus::FailedUnknown);
        assert_eq!(reason.as_deref(), Some(INTERRUPTED));
        assert_eq!(signals.check_output, None);
    }

    #[test]
    fn the_attempt_names_the_check_and_how_it_failed() {
        assert_eq!(
            Check.attempt_reason(Some("exit 2".to_owned())),
            Some("check failed (exit 2)".to_owned())
        );
        assert_eq!(Check.attempt_reason(None), None);
    }
}
