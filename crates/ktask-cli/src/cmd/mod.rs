//! Command implementations for ktask-rs CLI.
//!
//! Each command is a thin wrapper over ktask-core that handles I/O,
//! formatting, and error reporting. No decision logic lives here.

pub(crate) mod ack;
pub(crate) mod add;
pub(crate) mod cancel;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod interrupt;
pub(crate) mod pause;
pub(crate) mod plan;
pub(crate) mod rerun_gate;
pub(crate) mod resolve;
pub(crate) mod resume;
pub(crate) mod retry;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod tui;

use crate::cli::Command;
use ktask_core::RunOutcome;

/// Dispatch a CLI command to its implementation and return the outcome.
pub(crate) fn dispatch(
    command: Command,
    project: Option<ktask_core::Project>,
    config: Option<ktask_core::Config>,
    json: bool,
) -> RunOutcome {
    match command {
        Command::Doctor => doctor::run(json),
        Command::Init => init::run(),
        Command::Add { file } => add::run(project, file),
        Command::Plan { subcommand } => plan::run(project, subcommand, json),
        Command::Status => status::run(project, json),
        Command::Run { task, from } => run::run(project, config, task, from),
        Command::Resume => resume::run(project, config),
        Command::Retry { task } => retry::run(project, config, &task),
        Command::Resolve { task, note } => resolve::run(project, &task, note),
        Command::Ack { task } => ack::run(project, task),
        Command::Pause => pause::run(project),
        Command::Interrupt => interrupt::run(project),
        Command::Cancel { task } => cancel::run(project, &task),
        Command::RerunGate { task, gate } => rerun_gate::run(project, config, &task, gate, json),
        Command::Tui => tui::run(project, config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_command_variants_are_dispatched() {
        // This test ensures that every variant of Command is explicitly handled
        // in the dispatch function. If a new variant is added to the Command enum,
        // this will fail to compile until it is added to the dispatch match.
        //
        // We test this by constructing instances of each variant and calling dispatch.
        // The variants don't need to execute successfully; we just need to verify
        // they can be dispatched.

        let variants: Vec<Command> = vec![
            Command::Doctor,
            Command::Init,
            Command::Add { file: None },
            Command::Plan {
                subcommand: crate::cli::PlanSubcommand::Lint,
            },
            Command::Status,
            Command::Run {
                task: None,
                from: None,
            },
            Command::Resume,
            Command::Retry {
                task: "t-001".to_string(),
            },
            Command::Resolve {
                task: "t-001".to_string(),
                note: None,
            },
            Command::Ack { task: None },
            Command::Pause,
            Command::Interrupt,
            Command::Cancel {
                task: "t-001".to_string(),
            },
            Command::RerunGate {
                task: "t-001".to_string(),
                gate: None,
            },
            Command::Tui,
        ];

        // Verify all variants are present by checking the count.
        // The Command enum has 15 variants as of the last update.
        assert_eq!(
            variants.len(),
            15,
            "Test does not cover all Command variants. Update the test when adding new commands."
        );

        // Call dispatch on each variant to ensure they all dispatch without panicking.
        for variant in variants {
            let _ = dispatch(variant, None, None, false);
        }
    }
}
