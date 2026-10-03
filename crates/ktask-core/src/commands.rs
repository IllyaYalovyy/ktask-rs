//! Port: starting a process, feeding it input, and capturing what it produces.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Why a command could not be run at all — never used for the command running and failing
/// on its own terms, which is a normal [`Output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandsError {
    message: String,
}

impl CommandsError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CommandsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for CommandsError {}

/// A command to start: the program, its arguments, the directory it starts in, the bytes fed
/// to its standard input, and how long it may run before it, and every process it started, is
/// killed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The program to run, found on `PATH` or given as a path.
    pub program: String,
    /// The arguments passed to `program`.
    pub args: Vec<String>,
    /// The directory the command starts in.
    pub dir: PathBuf,
    /// The bytes written to the command's standard input, then closed.
    pub stdin: Vec<u8>,
    /// How long the command may run before it is killed.
    pub timeout: Duration,
    /// Tool-state file that receives output as it arrives, or `None` for capture-only calls.
    pub output_path: Option<PathBuf>,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command exited on its own, with this code.
    Code(i32),
    /// The command was still running once its time limit passed, and was killed along with
    /// every process it started.
    Killed,
    /// This process was asked to stop (`SIGINT`, `SIGTERM` or `SIGHUP`) while the command was
    /// still running; it, and every process it started, was killed along with it.
    Interrupted,
}

/// What a command produced: its captured output and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Everything the command wrote to its standard output.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to its standard error.
    pub stderr: Vec<u8>,
    /// How the command ended.
    pub exit: Exit,
}

/// Port: starting a process, feeding it input on its standard input, and capturing its
/// standard output, standard error and exit code — killing it, and every process it started,
/// if it runs past its time limit.
pub trait Commands {
    /// Runs `spec` to completion, or until its time limit passes.
    ///
    /// # Errors
    ///
    /// Fails when the command cannot be started at all.
    fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError>;
}
