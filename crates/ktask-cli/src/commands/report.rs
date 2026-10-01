//! `ktask-rs report`: state the outcome of an attempt.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{SystemClock, echo};
use ktask_core::{AttemptToken, Outcome};

use crate::context::{open_journal, open_registry, reject_project, resolve};
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
    /// rejected for the test step; retry, stop or skip for the resolve step — an outcome that
    /// does not belong to the step currently running is refused
    #[arg(value_name = "OUTCOME")]
    outcome: String,
    /// Why it ended that way; required unless the outcome is done, approved, accepted or retry
    #[arg(long)]
    reason: Option<String>,
    /// The provider the task's next attempt should run with; only valid with the `retry`
    /// outcome, refused when it does not name a known provider
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,
    /// The model the task's next attempt should run with; only valid with the `retry` outcome
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
    /// Ask the task's next attempt to resume this attempt's own session instead of starting
    /// fresh; only valid with the `retry` outcome, refused when this attempt reported no
    /// session, or the configured provider does not support resuming one at all
    #[arg(long)]
    same_session: bool,
    /// Return the working tree to the commit this attempt started from before the task's next
    /// attempt begins, discarding every file it created or changed; only valid with the
    /// `retry` outcome
    #[arg(long)]
    reset_tree: bool,
}

/// Checks `args.provider`, `args.model`, `args.same_session` and `args.reset_tree` against
/// `outcome`: with `retry`, a named provider must be a known one; with anything else, none of
/// the four may be given at all.
fn check_provider_and_model(outcome: Outcome, args: &Args) -> Result<(), Failure> {
    if outcome == Outcome::Retry {
        if let Some(provider) = &args.provider {
            super::provider::check_known(provider)?;
        }
        return Ok(());
    }
    if args.provider.is_some() || args.model.is_some() || args.same_session || args.reset_tree {
        return Err(Failure {
            message: format!(
                "outcome {outcome} does not accept --provider, --model, --same-session or \
                 --reset-tree: only retry does"
            ),
            code: 2,
        });
    }
    Ok(())
}

/// Records `args.outcome` (and `args.reason`, or — for `retry` — `args.model`,
/// `args.same_session` and `args.reset_tree`) for the attempt `token` names, as the step
/// currently running for it allows.
fn record(
    journal: &impl ktask_core::Journal,
    token: &AttemptToken,
    outcome: Outcome,
    args: &Args,
) -> Result<(), ktask_core::ReportError> {
    if outcome == Outcome::Retry {
        ktask_core::report_retry(
            journal,
            &SystemClock,
            token,
            args.model.as_deref(),
            args.same_session,
            echo::PROVIDER.supports_resume,
            args.reset_tree,
        )
    } else {
        ktask_core::report(
            journal,
            &SystemClock,
            token,
            outcome,
            args.reason.as_deref(),
        )
    }
}

/// Records `args.outcome` (and `args.reason`) for the attempt `args.token` names, in the
/// journal of the project it names — resolved from the token alone, so this needs no
/// `--project` and works from any directory; one named before it is refused.
pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    reject_project(project)?;
    let outcome: Outcome = args
        .outcome
        .parse()
        .map_err(|message| Failure { message, code: 2 })?;
    let token: AttemptToken = args
        .token
        .parse()
        .map_err(|message| Failure { message, code: 2 })?;
    check_provider_and_model(outcome, args)?;
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, Some(&token.project))?;
    let journal = open_journal(&project)?;
    record(&journal, &token, outcome, args)?;
    render::reported(&token, outcome, stdout)?;
    Ok(ExitCode::SUCCESS)
}
