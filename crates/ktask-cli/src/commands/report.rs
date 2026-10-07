//! `ktask-rs report`: state the outcome of an attempt.

use std::io::Write;
use std::process::ExitCode;

use ktask_adapters::{
    SystemClock, TomlSettingsStore, builtin_providers, echo, read_text, settings_path,
};
use ktask_core::{AttemptToken, Outcome, SettingsStore};

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
    /// rejected for the test step; retry, stop, skip or supersede for the resolve step — an
    /// outcome that does not belong to the step currently running is refused
    #[arg(value_name = "OUTCOME")]
    outcome: String,
    /// Why it ended that way; required unless the outcome is done, approved, accepted, retry or
    /// supersede
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
    /// The JSON file of tasks that replace the superseded one, in the same format `import`
    /// takes, or - for standard input; required with the `supersede` outcome, refused with
    /// every other one
    #[arg(long, value_name = "FILE")]
    tasks: Option<String>,
}

/// Checks `args.provider`, `args.model`, `args.same_session` and `args.reset_tree` against
/// `outcome`: with `retry`, a named provider must be a known one; with anything else, none of
/// the four may be given at all.
fn check_provider_and_model(outcome: Outcome, args: &Args) -> Result<(), Failure> {
    if outcome == Outcome::Retry {
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

/// Refuses a retry provider before the token is resolved, by reading only its project's
/// settings path. This preserves the command's normal argument-error precedence while still
/// accepting project-defined names.
fn check_defined_provider(token: &AttemptToken, provider: Option<&str>) -> Result<(), Failure> {
    let Some(provider) = provider else {
        return Ok(());
    };
    let path = settings_path(
        crate::build::CHANNEL,
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &token.project,
    )
    .ok_or_else(|| {
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path"
            .to_owned()
    })?;
    let settings = TomlSettingsStore::new(path)
        .load()
        .map_err(|error| error.to_string())?;
    let providers = ktask_core::show_providers(&settings, &builtin_providers())
        .map_err(|error| error.to_string())?;
    if providers.iter().any(|known| known.name == provider) {
        return Ok(());
    }
    let names = providers
        .into_iter()
        .map(|known| known.name)
        .collect::<Vec<_>>()
        .join(", ");
    Err(Failure {
        message: format!("unknown provider {provider:?}; known providers: {names}"),
        code: 2,
    })
}

/// Checks `args.tasks` against `outcome`: `supersede` needs it, every other outcome refuses it.
fn check_tasks_flag(outcome: Outcome, args: &Args) -> Result<(), Failure> {
    match (outcome, &args.tasks) {
        (Outcome::Supersede, None) => Err(Failure {
            message: "outcome supersede needs --tasks".to_owned(),
            code: 2,
        }),
        (Outcome::Supersede, Some(_)) | (_, None) => Ok(()),
        (_, Some(_)) => Err(Failure {
            message: format!("outcome {outcome} does not accept --tasks: only supersede does"),
            code: 2,
        }),
    }
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
            echo::provider().supports_resume,
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
    check_defined_provider(&token, args.provider.as_deref())?;
    check_tasks_flag(outcome, args)?;
    // Read before anything is registered or opened, so that a missing file changes nothing.
    let tasks_json = args
        .tasks
        .as_deref()
        .map(read_text)
        .transpose()
        .map_err(|message| Failure { message, code: 2 })?;
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, Some(&token.project))?;
    let journal = open_journal(&project)?;
    if let Some(tasks_json) = tasks_json {
        let supersede = ktask_core::report_supersede(&journal, &SystemClock, &token, &tasks_json)?;
        render::reported_supersede(token.task, &supersede, stdout)?;
    } else {
        record(&journal, &token, outcome, args)?;
        render::reported(&token, outcome, stdout)?;
    }
    Ok(ExitCode::SUCCESS)
}
