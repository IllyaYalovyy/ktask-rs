//! The far side of [`ktask_adapters::EXEC_TIED_TO_PARENT_MARKER`]: how `ktask-rs run` ties a
//! provider's life to its own, so it does not outlive a run that is killed outright. `main`
//! checks for [`MARKER`] before its normal argument parsing even starts, since this exists for
//! [`ktask_adapters::ProcessCommands`] to invoke on itself, never for a person to type.

use std::io::{self, Write};
use std::process::ExitCode;

/// The `argv[1]` this is matched by.
pub(crate) const MARKER: &str = ktask_adapters::EXEC_TIED_TO_PARENT_MARKER;

/// Runs `args` — `<parent-pid> <program> [program-args...]` — as
/// [`ktask_adapters::exec_tied_to_parent`], which, on success, never returns at all.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let fail = |message: String| {
        let _ = writeln!(io::stderr(), "ktask-rs: {message}");
        ExitCode::FAILURE
    };
    let Some((parent_pid, rest)) = args.split_first() else {
        return fail("exec-tied-to-parent needs a parent process id".to_owned());
    };
    let Ok(parent_pid) = parent_pid.parse() else {
        return fail(format!("{parent_pid:?} is not a process id"));
    };
    let Some((program, program_args)) = rest.split_first() else {
        return fail("exec-tied-to-parent needs a program to run".to_owned());
    };
    let error = ktask_adapters::exec_tied_to_parent(parent_pid, program, program_args);
    fail(format!("cannot run {program}: {error}"))
}
