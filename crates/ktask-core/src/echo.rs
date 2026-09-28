//! The `echo` provider: a built-in configuration that uses no model and no tokens. It runs
//! the first fenced `bash` code block of a prompt with `bash`, and reports what that produced.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::{CommandSpec, Commands, CommandsError, Output};

/// The name the `echo` provider is known by.
pub const NAME: &str = "echo";

/// Why the `echo` provider could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoError {
    /// The prompt has no fenced `bash` code block.
    NoBashBlock,
    /// The `bash` command could not be started.
    Commands(CommandsError),
}

impl fmt::Display for EchoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBashBlock => {
                f.write_str("the prompt has no fenced bash code block for the echo provider to run")
            }
            Self::Commands(error) => error.fmt(f),
        }
    }
}

impl Error for EchoError {}

impl From<CommandsError> for EchoError {
    fn from(error: CommandsError) -> Self {
        Self::Commands(error)
    }
}

/// Use case: runs the first fenced `bash` code block of `prompt` with `bash`, in `dir`, passing
/// `token` as `$1` and `attempt` as `$2`, killing it and everything it started if it runs past
/// `timeout`.
///
/// # Errors
///
/// Fails, running nothing, when the prompt has no fenced bash code block. Fails when the
/// `bash` command cannot be started at all.
pub fn run_echo(
    commands: &impl Commands,
    prompt: &str,
    token: &str,
    attempt: u32,
    dir: &Path,
    timeout: Duration,
) -> Result<Output, EchoError> {
    let block = first_bash_block(prompt).ok_or(EchoError::NoBashBlock)?;
    let spec = CommandSpec {
        program: "bash".to_owned(),
        args: vec!["-s".to_owned(), token.to_owned(), attempt.to_string()],
        dir: dir.to_owned(),
        stdin: block.into_bytes(),
        timeout,
    };
    Ok(commands.run(&spec)?)
}

/// The content of the first fenced `bash` code block in `prompt`, or `None` when it has none.
/// When the block's closing fence is missing, everything to the end of `prompt` is taken as
/// the block.
fn first_bash_block(prompt: &str) -> Option<String> {
    let mut lines = prompt.lines();
    for line in lines.by_ref() {
        if line.trim() != "```bash" {
            continue;
        }
        let mut block = Vec::new();
        for line in lines.by_ref() {
            if line.trim() == "```" {
                break;
            }
            block.push(line);
        }
        block.push("");
        return Some(block.join("\n"));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fakes::FakeCommands;

    const DIR: &str = "/work/app";
    const TIMEOUT: Duration = Duration::from_secs(30);

    fn output(exit: crate::Exit) -> Output {
        Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit,
        }
    }

    #[test]
    fn a_prompt_with_no_bash_block_is_an_error_and_nothing_is_run() {
        let commands = FakeCommands::returning(Ok(output(crate::Exit::Code(0))));
        let result = run_echo(
            &commands,
            "just some text\n```python\nprint(1)\n```\n",
            "tok",
            1,
            Path::new(DIR),
            TIMEOUT,
        );
        assert_eq!(result, Err(EchoError::NoBashBlock));
        assert!(commands.last.borrow().is_none());
    }

    #[test]
    fn the_first_bash_block_is_run_with_bash_and_the_token_and_attempt_as_positional_args() {
        let commands = FakeCommands::returning(Ok(output(crate::Exit::Code(0))));
        let prompt = "before\n```bash\necho hi\n```\nafter\n";
        run_echo(&commands, prompt, "the-token", 3, Path::new(DIR), TIMEOUT).unwrap();
        let spec = commands.last.borrow().clone().unwrap();
        assert_eq!(spec.program, "bash");
        assert_eq!(spec.args, vec!["-s", "the-token", "3"]);
        assert_eq!(spec.dir, PathBuf::from(DIR));
        assert_eq!(spec.timeout, TIMEOUT);
        assert_eq!(spec.stdin, b"echo hi\n");
    }

    #[test]
    fn only_the_first_of_several_bash_blocks_is_run() {
        let commands = FakeCommands::returning(Ok(output(crate::Exit::Code(0))));
        let prompt = "```bash\nfirst\n```\n```bash\nsecond\n```\n";
        run_echo(&commands, prompt, "t", 1, Path::new(DIR), TIMEOUT).unwrap();
        assert_eq!(commands.last.borrow().clone().unwrap().stdin, b"first\n");
    }

    #[test]
    fn a_block_with_no_closing_fence_runs_to_the_end_of_the_prompt() {
        let commands = FakeCommands::returning(Ok(output(crate::Exit::Code(0))));
        let prompt = "```bash\necho a\necho b";
        run_echo(&commands, prompt, "t", 1, Path::new(DIR), TIMEOUT).unwrap();
        assert_eq!(
            commands.last.borrow().clone().unwrap().stdin,
            b"echo a\necho b\n"
        );
    }

    #[test]
    fn a_commands_failure_is_passed_on() {
        let failure = CommandsError::new("bash not found");
        let commands = FakeCommands::returning(Err(failure.clone()));
        let prompt = "```bash\necho hi\n```\n";
        assert_eq!(
            run_echo(&commands, prompt, "t", 1, Path::new(DIR), TIMEOUT),
            Err(EchoError::Commands(failure))
        );
    }

    #[test]
    fn the_commands_output_is_returned_unchanged() {
        let expected = Output {
            stdout: b"hi\n".to_vec(),
            stderr: Vec::new(),
            exit: crate::Exit::Code(7),
        };
        let commands = FakeCommands::returning(Ok(expected.clone()));
        let prompt = "```bash\necho hi\nexit 7\n```\n";
        assert_eq!(
            run_echo(&commands, prompt, "t", 1, Path::new(DIR), TIMEOUT),
            Ok(expected)
        );
    }
}
