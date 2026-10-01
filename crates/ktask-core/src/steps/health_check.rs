//! The health-check gate: runs the project's configured command before a task's attempt is
//! even begun, to prove the code base is healthy before anything is built on it. Not one of
//! the [`super::Step`]s an attempt walks — refusing to run leaves no attempt at all, so a task
//! it stops stays `pending` — but shaped the same way as every step: a name, a switch, and
//! running it.

use std::time::Duration;

use crate::run::RunEnd;
use crate::steps::INTERRUPTED;
use crate::{
    Clock, CommandSpec, Commands, CommandsError, Exit, HEALTH_CHECK_STEP, Journal, Output,
    RunContext, RunError, TaskId,
};

/// How many lines of a failing health check's combined output are shown to the operator.
const OUTPUT_TAIL_LINES: usize = 20;

/// What running the project's health check produced.
#[derive(Debug)]
pub(crate) enum HealthCheck {
    /// It exited zero: `duration` is how long it took.
    Passed(Duration),
    /// It did not: `reason` says how, `output_tail` is the end of what it printed.
    Failed { reason: String, output_tail: String },
}

/// Whether the health-check gate is switched on for `context`: a command is configured, and
/// the step is not named in `context.disabled_steps`.
pub(crate) fn enabled(context: RunContext<'_>) -> bool {
    context.health_check_command.is_some() && context.step_enabled(HEALTH_CHECK_STEP)
}

/// The last [`OUTPUT_TAIL_LINES`] lines of `output`'s combined standard output and standard
/// error.
fn output_tail(output: &Output) -> String {
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&combined);
    let lines: Vec<&str> = text.lines().collect();
    let tail: Vec<&str> = lines
        .iter()
        .rev()
        .take(OUTPUT_TAIL_LINES)
        .rev()
        .copied()
        .collect();
    tail.join("\n")
}

/// What `run` found, turned into a [`HealthCheck`].
fn health_check_outcome(result: Result<Output, CommandsError>, duration: Duration) -> HealthCheck {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return HealthCheck::Failed {
                reason: format!("the health check could not be run: {error}"),
                output_tail: String::new(),
            };
        }
    };
    match output.exit {
        Exit::Code(0) => HealthCheck::Passed(duration),
        Exit::Code(code) => HealthCheck::Failed {
            reason: format!("the health check exited with code {code}"),
            output_tail: output_tail(&output),
        },
        Exit::Killed => HealthCheck::Failed {
            reason: "the health check ran past its time limit and was killed".to_owned(),
            output_tail: output_tail(&output),
        },
        Exit::Interrupted => HealthCheck::Failed {
            reason: INTERRUPTED.to_owned(),
            output_tail: output_tail(&output),
        },
    }
}

/// Runs `command` with `bash -c` in `context.project_dir`, subject to `context.attempt_timeout`
/// the same way an attempt's own steps are.
pub(crate) fn run(
    commands: &dyn Commands,
    clock: &dyn Clock,
    command: &str,
    context: RunContext<'_>,
) -> HealthCheck {
    let started = clock.now();
    let spec = CommandSpec {
        program: "bash".to_owned(),
        args: vec!["-c".to_owned(), command.to_owned()],
        dir: context.project_dir.to_owned(),
        stdin: Vec::new(),
        timeout: context.attempt_timeout,
    };
    let result = commands.run(&spec);
    let duration = clock.now().duration_since(started).unwrap_or_default();
    health_check_outcome(result, duration)
}

/// What failed and what is expected of the operator, condensed to one line so it fits a status
/// step's own line — the same words `ktask-rs run` prints for it.
fn gate_message(command: &str, reason: &str, output_tail: &str) -> String {
    let mut message = format!("{command}: {reason}");
    if !output_tail.is_empty() {
        message.push_str(": ");
        message.push_str(&output_tail.replace('\n', " / "));
    }
    message.push_str("; fix the health check, then run again");
    message
}

/// Runs the health-check gate ahead of `task_id`'s attempt: the pre-step to record when it
/// passes, `None` when the gate is switched off, or the [`RunEnd`] that stops the run when it
/// fails — recording why in the journal first, so a later `status` can show it even though the
/// task stays `pending`.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn run_gate(
    journal: &dyn Journal,
    commands: &dyn Commands,
    clock: &dyn Clock,
    context: RunContext<'_>,
    task_id: TaskId,
) -> Result<Result<Option<super::PreStep>, RunEnd>, RunError> {
    if !enabled(context) {
        return Ok(Ok(None));
    }
    let Some(command) = context.health_check_command else {
        unreachable!("health_check::enabled only returns true when a command is configured");
    };
    match run(commands, clock, command, context) {
        HealthCheck::Passed(duration) => Ok(Ok(Some(super::PreStep {
            name: HEALTH_CHECK_STEP,
            duration,
            reason: None,
        }))),
        HealthCheck::Failed {
            reason,
            output_tail,
        } => {
            crate::attempt::record_gate_failure(
                journal,
                clock,
                task_id,
                HEALTH_CHECK_STEP,
                &gate_message(command, &reason, &output_tail),
            )?;
            Ok(Err(RunEnd::HealthCheckFailed {
                id: task_id,
                command: command.to_owned(),
                reason,
                output_tail,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use super::*;
    use crate::fakes::FakeCommands;

    fn context() -> RunContext<'static> {
        RunContext {
            project_name: "proj",
            project_dir: Path::new("/work/proj"),
            binary_path: Path::new("/opt/ktask-rs/bin/ktask-rs"),
            attempt_timeout: Duration::from_secs(42),
            health_check_command: Some("make check"),
            tracked_branch: None,
            disabled_steps: &[],
            max_attempts: 1,
            resolver_model: "",
            sessions_dir: Path::new("/state/sessions"),
        }
    }

    struct StoppedClock;
    impl Clock for StoppedClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
    }

    #[test]
    fn disabled_when_no_command_is_configured() {
        let mut ctx = context();
        ctx.health_check_command = None;
        assert!(!enabled(ctx));
    }

    #[test]
    fn disabled_when_switched_off_even_with_a_command_configured() {
        let mut ctx = context();
        ctx.disabled_steps = &[HEALTH_CHECK_STEP];
        assert!(!enabled(ctx));
    }

    #[test]
    fn enabled_with_a_command_configured_and_the_switch_on() {
        assert!(enabled(context()));
    }

    #[test]
    fn a_zero_exit_passes() {
        let commands = FakeCommands::returning(Ok(Output {
            stdout: b"all good\n".to_vec(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        match run(&commands, &StoppedClock, "make check", context()) {
            HealthCheck::Passed(_) => {}
            failed @ HealthCheck::Failed { .. } => panic!("expected Passed, got {failed:?}"),
        }
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.program, "bash");
        assert_eq!(spec.args, vec!["-c".to_owned(), "make check".to_owned()]);
        assert_eq!(spec.dir, Path::new("/work/proj"));
        assert_eq!(spec.timeout, Duration::from_secs(42));
    }

    #[test]
    fn a_nonzero_exit_fails_with_the_tail_of_its_output() {
        let commands = FakeCommands::returning(Ok(Output {
            stdout: b"building...\n".to_vec(),
            stderr: b"ERROR: nope\n".to_vec(),
            exit: Exit::Code(1),
        }));
        match run(&commands, &StoppedClock, "make check", context()) {
            HealthCheck::Failed {
                reason,
                output_tail,
            } => {
                assert_eq!(reason, "the health check exited with code 1");
                assert_eq!(output_tail, "building...\nERROR: nope");
            }
            passed @ HealthCheck::Passed(_) => panic!("expected Failed, got {passed:?}"),
        }
    }

    #[test]
    fn a_run_past_its_time_limit_is_killed_and_counts_as_failing() {
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Killed,
        }));
        match run(&commands, &StoppedClock, "sleep 999", context()) {
            HealthCheck::Failed { reason, .. } => {
                assert!(reason.contains("time limit"), "{reason}");
            }
            passed @ HealthCheck::Passed(_) => panic!("expected Failed, got {passed:?}"),
        }
    }

    #[test]
    fn a_command_that_cannot_be_run_fails_with_why() {
        let commands = FakeCommands::returning(Err(CommandsError::new("bash not found")));
        match run(&commands, &StoppedClock, "make check", context()) {
            HealthCheck::Failed { reason, .. } => {
                assert!(reason.contains("bash not found"), "{reason}");
            }
            passed @ HealthCheck::Passed(_) => panic!("expected Failed, got {passed:?}"),
        }
    }

    #[test]
    fn the_gate_message_fits_on_one_line_even_with_several_lines_of_output_and_says_what_is_expected()
     {
        let message = gate_message(
            "make check",
            "the health check exited with code 1",
            "building...\nERROR: nope",
        );
        assert_eq!(message.lines().count(), 1, "{message:?}");
        assert!(message.contains("make check"), "{message}");
        assert!(message.contains("exited with code 1"), "{message}");
        assert!(message.contains("building..."), "{message}");
        assert!(message.contains("ERROR: nope"), "{message}");
        assert!(message.contains("fix the health check"), "{message}");
    }

    #[test]
    fn the_gate_message_needs_no_separator_when_there_is_no_output() {
        let message = gate_message("make check", "the health check exited with code 1", "");
        assert_eq!(
            message,
            "make check: the health check exited with code 1; fix the health check, then run \
             again"
        );
    }
}
