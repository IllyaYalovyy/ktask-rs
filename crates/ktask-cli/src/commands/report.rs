//! `ktask-rs report`: state the outcome of an attempt.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::SystemClock;
use ktask_core::{AttemptToken, Outcome};

use crate::context::{open_journal, open_registry, resolve};
use crate::error::Failure;
use crate::render;

/// `ktask-rs report`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The attempt's token
    #[arg(long)]
    token: String,
    /// What the attempt ended with: done, failed, needs-input or too-large for the
    /// implementation step; approved or changes-requested for the review step; accepted or
    /// rejected for the test step — an outcome that does not belong to the step currently
    /// running is refused
    #[arg(value_name = "OUTCOME")]
    outcome: String,
    /// Why it ended that way; required unless the outcome is done
    #[arg(long)]
    reason: Option<String>,
}

/// Records `args.outcome` (and `args.reason`) for the attempt `args.token` names, in the
/// journal of the project it names — resolved from the token alone, so this needs no
/// `--project` and works from any directory.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let outcome: Outcome = args
        .outcome
        .parse()
        .map_err(|message| Failure { message, code: 2 })?;
    let token: AttemptToken = args
        .token
        .parse()
        .map_err(|message| Failure { message, code: 2 })?;
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, Some(&token.project))?;
    let journal = open_journal(&project)?;
    ktask_core::report(
        &journal,
        &SystemClock,
        &token,
        outcome,
        args.reason.as_deref(),
    )?;
    render::reported(&token, outcome, stdout)?;
    Ok(ExitCode::SUCCESS)
}
