//! A provider: a name, and how a prompt, a token and an attempt number become the command
//! that runs it. Nothing here names or knows any particular provider — every one is a
//! [`Provider`] value defined outside `core`.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::{CommandSpec, Commands, CommandsError, Output};

/// The program, arguments and standard input a provider decided to run a prompt with —
/// everything about the command except where it runs and how long it may run, which the
/// caller of [`run_provider`] supplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommand {
    /// The program to run, found on `PATH` or given as a path.
    pub program: String,
    /// The arguments passed to `program`.
    pub args: Vec<String>,
    /// The bytes written to the command's standard input, then closed.
    pub stdin: Vec<u8>,
}

/// Which step of which attempt a provider command runs for: the token that names the attempt,
/// its number, and the step's name — passed to a real provider's script or command line as
/// positional arguments, so it can call back into `ktask-rs report` and tell which step it is
/// running as.
#[derive(Debug, Clone, Copy)]
pub struct StepCall<'a> {
    /// The attempt's token.
    pub token: &'a str,
    /// The attempt's number.
    pub attempt: u32,
    /// The step's name.
    pub step: &'a str,
}

/// A provider: a name it is known by, and a pure function from a prompt and a [`StepCall`] to
/// the [`ProviderCommand`] that runs it.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// The name the provider is known by.
    pub name: &'static str,
    /// Turns a prompt and a [`StepCall`] into the command that runs it.
    ///
    /// # Errors
    ///
    /// Fails when the prompt cannot be turned into a command to run.
    pub command: fn(prompt: &str, call: StepCall<'_>) -> Result<ProviderCommand, String>,
}

/// Why running a provider failed — never for the command's own exit code, which is a normal
/// [`Output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRunError {
    /// The provider could not turn the prompt into a command to run.
    Build(String),
    /// The command could not be started.
    Commands(CommandsError),
}

impl fmt::Display for ProviderRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build(message) => f.write_str(message),
            Self::Commands(error) => error.fmt(f),
        }
    }
}

impl Error for ProviderRunError {}

impl From<CommandsError> for ProviderRunError {
    fn from(error: CommandsError) -> Self {
        Self::Commands(error)
    }
}

/// Use case: turns `prompt` into `provider`'s command for `call`, and runs it in `dir`, killing
/// it, and everything it started, if it runs past `timeout`.
///
/// # Errors
///
/// Fails, running nothing, when `provider` cannot turn `prompt` into a command. Fails when the
/// command cannot be started at all.
pub fn run_provider(
    commands: &dyn Commands,
    provider: &Provider,
    prompt: &str,
    call: StepCall<'_>,
    dir: &Path,
    timeout: Duration,
) -> Result<Output, ProviderRunError> {
    let built = (provider.command)(prompt, call).map_err(ProviderRunError::Build)?;
    let spec = CommandSpec {
        program: built.program,
        args: built.args,
        dir: dir.to_owned(),
        stdin: built.stdin,
        timeout,
    };
    Ok(commands.run(&spec)?)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::Exit;
    use crate::fakes::FakeCommands;

    const DIR: &str = "/work/app";
    const TIMEOUT: Duration = Duration::from_secs(30);

    fn provider_that_builds(
        command: fn(&str, StepCall<'_>) -> Result<ProviderCommand, String>,
    ) -> Provider {
        Provider {
            name: "test",
            command,
        }
    }

    fn call<'a>(token: &'a str, attempt: u32, step: &'a str) -> StepCall<'a> {
        StepCall {
            token,
            attempt,
            step,
        }
    }

    #[test]
    fn the_built_command_is_run_in_the_given_directory_with_the_given_timeout() {
        let provider = provider_that_builds(|prompt, call| {
            Ok(ProviderCommand {
                program: "run-it".to_owned(),
                args: vec![
                    call.token.to_owned(),
                    call.attempt.to_string(),
                    call.step.to_owned(),
                ],
                stdin: prompt.as_bytes().to_vec(),
            })
        });
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        run_provider(
            &commands,
            &provider,
            "the prompt",
            call("the-token", 3, "implementation"),
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap();
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.program, "run-it");
        assert_eq!(spec.args, vec!["the-token", "3", "implementation"]);
        assert_eq!(spec.stdin, b"the prompt");
        assert_eq!(spec.dir, Path::new(DIR));
        assert_eq!(spec.timeout, TIMEOUT);
    }

    #[test]
    fn a_provider_that_cannot_build_a_command_runs_nothing_and_fails() {
        let provider = provider_that_builds(|_, _| Err("no can do".to_owned()));
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let error = run_provider(
            &commands,
            &provider,
            "the prompt",
            call("t", 1, "implementation"),
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error, ProviderRunError::Build("no can do".to_owned()));
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_commands_failure_is_passed_on() {
        let provider = provider_that_builds(|_, call| {
            Ok(ProviderCommand {
                program: "run-it".to_owned(),
                args: vec![call.token.to_owned(), call.attempt.to_string()],
                stdin: Vec::new(),
            })
        });
        let failure = CommandsError::new("bash not found");
        let commands = FakeCommands::returning(Err(failure.clone()));
        let error = run_provider(
            &commands,
            &provider,
            "the prompt",
            call("t", 1, "implementation"),
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error, ProviderRunError::Commands(failure));
    }
}
