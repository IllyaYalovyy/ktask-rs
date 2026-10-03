//! `ktask-rs provider`: the agents a task can be handed to.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::Subcommand;
use ktask_adapters::{ProcessCommands, builtin_providers, echo};
use ktask_core::{
    ProviderRunError, ProviderView, provider_field_source, provider_fields, show_providers,
};

use crate::context::{current_dir, open_registry, reject_project, resolve};
use crate::error::Failure;
use crate::render;

/// The `echo` provider's default time limit: generous, since it runs whatever a task's
/// prompt wrote, but not unbounded.
const DEFAULT_ECHO_TIMEOUT_MS: u64 = 300_000;

/// `ktask-rs provider`'s subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// List every built-in provider and this project's additions
    List {
        /// Print a JSON array instead of one provider name per line
        #[arg(long)]
        json: bool,
    },
    /// Show one provider's complete effective definition
    Show {
        /// The provider to show
        #[arg(value_name = "NAME")]
        name: String,
        /// Print a JSON object instead of one field per line
        #[arg(long)]
        json: bool,
    },
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
    match command {
        Command::List { json } => list(project, *json, stdout),
        Command::Show { name, json } => show(project, name, *json, stdout),
        Command::Run {
            provider,
            token,
            attempt,
            step,
            timeout_ms,
        } => {
            reject_project(project)?;
            check_known(provider)?;
            let prompt = read_prompt()?;
            let dir = current_dir()?;
            let output = run_echo(&prompt, token, *attempt, step, &dir, *timeout_ms)?;
            render::provider_output(&output, stdout, &mut io::stderr())?;
            exit_code(&output, provider, *timeout_ms)
        }
    }
}

/// Reads the effective catalogue for the selected project. The same settings validation all
/// project commands use happens before a catalogue is rendered.
fn providers(project: Option<&str>) -> Result<Vec<ProviderView>, Failure> {
    let registry = open_registry()?;
    let (_project, settings) = resolve(&registry, project)?;
    show_providers(&settings, &builtin_providers()).map_err(|error| error.to_string().into())
}

fn list(project: Option<&str>, json: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let providers = providers(project)?;
    if json {
        let names: Vec<&str> = providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        let names = serde_json::to_string(&names).map_err(|error| error.to_string())?;
        writeln!(stdout, "{names}").map_err(|error| error.to_string())?;
    } else {
        for provider in providers {
            writeln!(stdout, "{}", provider.name).map_err(|error| error.to_string())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn show(
    project: Option<&str>,
    name: &str,
    json: bool,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let provider = providers(project)?
        .into_iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| Failure {
            message: format!("unknown provider {name:?}"),
            code: 2,
        })?;
    let definition = &provider.definition;
    if json {
        let value = serde_json::json!({
            "name": provider.name, "command": definition.command, "args": definition.args,
            "prompt": definition.prompt, "model": definition.model, "resume": definition.resume,
            "denied-tools": definition.denied_tools, "parser": definition.parser.to_string(),
            "session-id": definition.session_id, "usage": definition.usage,
            "limit-message": definition.limit_message, "overridden": provider.overridden,
        });
        writeln!(stdout, "{value}").map_err(|error| error.to_string())?;
    } else {
        for (field, value) in provider_fields(&provider) {
            let source = provider_field_source(&provider, field);
            writeln!(stdout, "{field}\t{value}\t{source}").map_err(|error| error.to_string())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes `prompt` to its own scratch file, runs the `echo` provider on it for `token`,
/// `attempt` and `step` in `dir` with `timeout_ms`, then removes the scratch file again,
/// whatever running it produced — `provider run` works on no project, so it has no state
/// directory of its own to keep the file under, unlike a real attempt's own steps.
fn run_echo(
    prompt: &str,
    token: &str,
    attempt: u32,
    step: &str,
    dir: &std::path::Path,
    timeout_ms: u64,
) -> Result<ktask_core::Output, Failure> {
    let prompt_path = write_prompt_scratch(prompt)?;
    let output = ktask_core::run_provider(
        &ProcessCommands,
        &echo::provider(),
        prompt,
        ktask_core::StepCall {
            token,
            attempt,
            step,
            model: None,
            resume: None,
            prompt_path: &prompt_path,
        },
        dir,
        Duration::from_millis(timeout_ms),
        None,
    )
    .map_err(|error| match error {
        ProviderRunError::Build(message) => Failure { message, code: 2 },
        ProviderRunError::Commands(_) => Failure::from(error.to_string()),
    });
    let _ = std::fs::remove_file(&prompt_path);
    output
}

/// Writes `prompt` to a scratch file of its own under the system's temporary directory.
fn write_prompt_scratch(prompt: &str) -> Result<std::path::PathBuf, Failure> {
    let path = std::env::temp_dir().join(format!(
        "ktask-rs-provider-run-{}.prompt",
        std::process::id()
    ));
    std::fs::write(&path, prompt).map_err(|e| {
        format!(
            "cannot write the prompt scratch file {}: {e}",
            path.display()
        )
    })?;
    Ok(path)
}

/// Rejects any provider name other than the one built-in `echo` provider — shared with
/// `ktask-rs report`'s own `--provider`, which names the provider the resolver wants the
/// task's next attempt to run with.
pub(crate) fn check_known(provider: &str) -> Result<(), Failure> {
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
