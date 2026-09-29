//! The far side of [`ktask_adapters::KILL_GROUP_IF_ORPHANED_MARKER`]: how `ktask-rs run`
//! guards a command's whole process group — including whatever it went on to start of its
//! own, in the foreground or the background — against surviving a run that is killed
//! outright. `main` checks for [`MARKER`] before its normal argument parsing even starts,
//! since this exists for [`ktask_adapters::ProcessCommands`] to invoke on itself, never for a
//! person to type.

use std::io::{self, Write};
use std::process::ExitCode;

/// The `argv[1]` this is matched by.
pub(crate) const MARKER: &str = ktask_adapters::KILL_GROUP_IF_ORPHANED_MARKER;

/// Runs `args` — `<pgid>` — as [`ktask_adapters::kill_group_if_orphaned`]: blocks until it is
/// told to act, then kills `pgid` and exits.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let fail = |message: String| {
        let _ = writeln!(io::stderr(), "ktask-rs: {message}");
        ExitCode::FAILURE
    };
    let Some(pgid) = args.first() else {
        return fail("kill-group-if-orphaned needs a process group id".to_owned());
    };
    let Ok(pgid) = pgid.parse() else {
        return fail(format!("{pgid:?} is not a process group id"));
    };
    ktask_adapters::kill_group_if_orphaned(pgid);
    ExitCode::SUCCESS
}
