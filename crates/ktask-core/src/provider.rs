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

/// A provider: a name it is known by, and a pure function from a prompt, a token and an
/// attempt number to the [`ProviderCommand`] that runs it.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// The name the provider is known by.
    pub name: &'static str,
    /// Turns a prompt, a token and an attempt number into the command that runs it.
    ///
    /// # Errors
    ///
    /// Fails when the prompt cannot be turned into a command to run.
    pub command: fn(prompt: &str, token: &str, attempt: u32) -> Result<ProviderCommand, String>,
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

/// Use case: turns `prompt` into `provider`'s command for `token` and `attempt`, and runs it
/// in `dir`, killing it, and everything it started, if it runs past `timeout`.
///
/// # Errors
///
/// Fails, running nothing, when `provider` cannot turn `prompt` into a command. Fails when the
/// command cannot be started at all.
pub fn run_provider(
    commands: &impl Commands,
    provider: &Provider,
    prompt: &str,
    token: &str,
    attempt: u32,
    dir: &Path,
    timeout: Duration,
) -> Result<Output, ProviderRunError> {
    let built = (provider.command)(prompt, token, attempt).map_err(ProviderRunError::Build)?;
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
        command: fn(&str, &str, u32) -> Result<ProviderCommand, String>,
    ) -> Provider {
        Provider {
            name: "test",
            command,
        }
    }

    #[test]
    fn the_built_command_is_run_in_the_given_directory_with_the_given_timeout() {
        let provider = provider_that_builds(|prompt, token, attempt| {
            Ok(ProviderCommand {
                program: "run-it".to_owned(),
                args: vec![token.to_owned(), attempt.to_string()],
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
            "the-token",
            3,
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap();
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.program, "run-it");
        assert_eq!(spec.args, vec!["the-token", "3"]);
        assert_eq!(spec.stdin, b"the prompt");
        assert_eq!(spec.dir, Path::new(DIR));
        assert_eq!(spec.timeout, TIMEOUT);
    }

    #[test]
    fn a_provider_that_cannot_build_a_command_runs_nothing_and_fails() {
        let provider = provider_that_builds(|_, _, _| Err("no can do".to_owned()));
        let commands = FakeCommands::returning(Ok(Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }));
        let error = run_provider(
            &commands,
            &provider,
            "the prompt",
            "t",
            1,
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error, ProviderRunError::Build("no can do".to_owned()));
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn a_commands_failure_is_passed_on() {
        let provider = provider_that_builds(|_, token, attempt| {
            Ok(ProviderCommand {
                program: "run-it".to_owned(),
                args: vec![token.to_owned(), attempt.to_string()],
                stdin: Vec::new(),
            })
        });
        let failure = CommandsError::new("bash not found");
        let commands = FakeCommands::returning(Err(failure.clone()));
        let error = run_provider(
            &commands,
            &provider,
            "the prompt",
            "t",
            1,
            Path::new(DIR),
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error, ProviderRunError::Commands(failure));
    }
}
