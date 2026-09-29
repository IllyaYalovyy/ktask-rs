//! `ktask-rs provider`: the agents a task can be handed to.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::Subcommand;
use ktask_adapters::{ProcessCommands, echo};
use ktask_core::ProviderRunError;

use crate::context::{current_dir, reject_project};
use crate::error::Failure;
use crate::render;

/// The `echo` provider's default time limit: generous, since it runs whatever a task's
/// prompt wrote, but not unbounded.
const DEFAULT_ECHO_TIMEOUT_MS: u64 = 300_000;

/// `ktask-rs provider`'s subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a provider on a prompt read from standard input, and print what it produced
    Run {
        /// The provider to run: currently only `echo`
        #[arg(value_name = "PROVIDER")]
        provider: String,
        /// The attempt's token, passed to the provider as $1
        #[arg(long)]
        token: String,
        /// The attempt number, passed to the provider as $2
        #[arg(long)]
        attempt: u32,
        /// The step's name, passed to the provider as $3
        #[arg(long, default_value = "manual")]
        step: String,
        /// How long the provider may run before it is killed, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = DEFAULT_ECHO_TIMEOUT_MS)]
        timeout_ms: u64,
    },
}

/// Runs the `provider` subcommand `command` names. `provider run` works on no project, so a
/// `--project` named before it is refused, the same as one named after it.
pub(crate) fn run(
    command: &Command,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    reject_project(project)?;
    let Command::Run {
        provider,
        token,
        attempt,
        step,
        timeout_ms,
    } = command;
    check_known(provider)?;
    let prompt = read_prompt()?;
    let dir = current_dir()?;
    let output = ktask_core::run_provider(
        &ProcessCommands,
        &echo::PROVIDER,
        &prompt,
        ktask_core::StepCall {
            token,
            attempt: *attempt,
            step,
        },
        &dir,
        Duration::from_millis(*timeout_ms),
    )
    .map_err(|error| match error {
        ProviderRunError::Build(message) => Failure { message, code: 2 },
        ProviderRunError::Commands(_) => Failure::from(error.to_string()),
    })?;
    render::provider_output(&output, stdout, &mut io::stderr())?;
    exit_code(&output, provider, *timeout_ms)
}

/// Rejects any provider name other than the one built-in `echo` provider.
fn check_known(provider: &str) -> Result<(), Failure> {
    if provider == echo::NAME {
        return Ok(());
    }
    Err(Failure {
        message: format!(
            "unknown provider {provider:?}; known providers: {}",
            echo::NAME
        ),
        code: 2,
    })
}

/// Reads the whole of standard input as the prompt to run the provider on.
fn read_prompt() -> Result<String, Failure> {
    let mut prompt = String::new();
    io::stdin()
        .lock()
        .read_to_string(&mut prompt)
        .map_err(|e| format!("cannot read the prompt: {e}"))?;
    Ok(prompt)
}

/// Maps what the provider process did to this command's exit code, or the failure that
/// reports it was killed for running past its time limit.
fn exit_code(
    output: &ktask_core::Output,
    provider: &str,
    timeout_ms: u64,
) -> Result<ExitCode, Failure> {
    match output.exit {
        ktask_core::Exit::Code(code) => Ok(ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))),
        ktask_core::Exit::Killed => Err(Failure {
            message: format!(
                "provider {provider} ran past its time limit of {timeout_ms}ms and was killed, along with everything it started"
            ),
            code: 124,
        }),
        ktask_core::Exit::Interrupted => Err(Failure {
            message: format!(
                "interrupted: provider {provider} was killed, along with everything it started"
            ),
            code: 130,
        }),
    }
}
