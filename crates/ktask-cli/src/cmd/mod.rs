//! Command dispatch: routes a parsed [`Command`] to the `cmd::` module that
//! implements it.
//!
//! Each command gets its own file, named for the command (`control` covers
//! the three run-control commands — pause, interrupt, cancel — together,
//! since `docs/CONTRACT.md` section 3 and a later task treat them as one
//! unit). `init` (T108), `doctor` (T110), `status` (T111), `add` (T112),
//! `plan lint` (T113), `run` (T115), `resume` and `retry` (T116),
//! `resolve` and `ack` (T117), `pause`, `interrupt` and `cancel` (T119) and
//! `rerun-gate` (T120) and `tui` (T131) have their real behavior; `run` and
//! `resume` also reconcile the journal on startup (T118). What this module
//! is responsible for is that [`dispatch`] itself is real: the match below
//! is exhaustive, so a `Command` variant added without a
//! corresponding arm fails to compile instead of silently falling through
//! to a default.

mod ack;
mod add;
mod control;
mod doctor;
mod editor;
mod init;
mod plan;
mod rerun_gate;
mod resolve;
mod resume;
mod retry;
mod run;
mod status;
mod tui;

use crate::cli::Command;
use ktask_core::{Config, Project, RunOutcome};

/// Routes `command` to the `cmd::` function that implements it, against the
/// already-resolved `project` and its effective `config`.
///
/// `json` is `--json` (`docs/CONTRACT.md` section 2): a global option, not
/// part of `Command` itself, so it is threaded through here rather than
/// parsed again per command. Only commands that report state consume it;
/// the rest ignore the parameter, same as they ignore `config` today.
pub(crate) fn dispatch(
    command: &Command,
    project: &Project,
    config: &Config,
    json: bool,
) -> RunOutcome {
    match command {
        Command::Doctor => doctor::run(project, config, json),
        Command::Init => init::run(project, config),
        Command::Add { file } => add::run(project, config, file.as_deref()),
        Command::Plan { command } => plan::run(project, config, command),
        Command::Status => status::run(project, config, json),
        Command::Run { task, from } => run::run(project, config, *task, *from, json),
        Command::Resume => resume::run(project, config, json),
        Command::Retry { task } => retry::run(project, *task, json),
        Command::Resolve { task, note } => resolve::run(project, config, *task, note.as_deref()),
        Command::Ack { task } => ack::run(project, config, *task),
        Command::Pause => control::pause(project, config),
        Command::Interrupt => control::interrupt(project, config),
        Command::Cancel { task } => control::cancel(project, config, *task),
        Command::RerunGate { task, gate } => rerun_gate::run(project, config, *task, *gate, json),
        Command::Tui => tui::run(project, config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::PlanCommand;
    use ktask_core::TaskId;
    use std::path::PathBuf;

    /// A `Project` value good enough for the stubs under test: none of them
    /// touch the filesystem, so nothing here needs to exist on disk.
    fn fixture_project() -> Project {
        Project {
            root: PathBuf::from("/nonexistent/ktask-dispatch-fixture/root"),
            id: "dispatch-fixture".to_string(),
            state_dir: PathBuf::from("/nonexistent/ktask-dispatch-fixture/state"),
        }
    }

    /// One instance of every [`Command`] variant. A variant added to the
    /// enum without an arm in `every_command` still fails to compile the
    /// exhaustive match in [`dispatch`]; this list exists so the test below
    /// actually calls each arm rather than merely typechecking it.
    fn every_command() -> Vec<Command> {
        vec![
            Command::Doctor,
            Command::Init,
            Command::Add {
                file: Some(PathBuf::from(
                    "/nonexistent/ktask-dispatch-fixture/no-such-task.md",
                )),
            },
            Command::Plan {
                command: PlanCommand::Lint,
            },
            Command::Status,
            Command::Run {
                task: None,
                from: None,
            },
            Command::Resume,
            Command::Retry {
                task: TaskId::new(1),
            },
            Command::Resolve {
                task: TaskId::new(1),
                note: None,
            },
            Command::Ack { task: None },
            Command::Pause,
            Command::Interrupt,
            Command::Cancel {
                task: TaskId::new(1),
            },
            Command::RerunGate {
                task: TaskId::new(1),
                gate: None,
            },
            Command::Tui,
        ]
    }

    #[test]
    fn dispatch_reaches_every_command_variant() {
        let project = fixture_project();
        let config = Config::default();

        for command in every_command() {
            let outcome = dispatch(&command, &project, &config, false);

            // `doctor` (T110), `status` (T111), `plan lint` (T113), `resume`
            // and `retry` (T116), `resolve` and `ack` (T117), `pause`,
            // `interrupt` and `cancel` (T119), and `rerun-gate` (T120) have
            // real behavior now: run against this fixture's nonexistent
            // state directory, all eleven fail to even open it, so each reaches `RunOutcome::CheckFailed` rather than
            // the placeholder `Drained` every other still-unimplemented
            // command returns. Their own modules' tests cover the behavior
            // in detail; this loop only needs to prove dispatch reached it.
            if matches!(
                command,
                Command::Doctor
                    | Command::Status
                    | Command::Plan { .. }
                    | Command::Resume
                    | Command::Retry { .. }
                    | Command::Resolve { .. }
                    | Command::Ack { .. }
                    | Command::Pause
                    | Command::Interrupt
                    | Command::Cancel { .. }
                    | Command::RerunGate { .. }
            ) {
                assert!(
                    matches!(outcome, RunOutcome::CheckFailed { .. }),
                    "{command:?} did not reach its cmd:: module's real behavior: {outcome:?}"
                );
                continue;
            }

            // `add` (T112), `run` (T115) and `tui` (T131) have real behavior
            // too: the fixture's `--file` names a path that does not exist,
            // so reading it fails before `add` ever touches the journal,
            // `run` cannot build a runner over a state directory that does
            // not exist, and `tui` refuses because the test's stdout is not
            // a terminal. All three reach `RunOutcome::Usage`.
            if matches!(
                command,
                Command::Add { .. } | Command::Run { .. } | Command::Tui
            ) {
                assert!(
                    matches!(outcome, RunOutcome::Usage { .. }),
                    "{command:?} did not reach its cmd:: module's real behavior: {outcome:?}"
                );
                continue;
            }

            assert_eq!(
                outcome,
                RunOutcome::Drained,
                "{command:?} did not reach its cmd:: placeholder"
            );
        }
    }
}
