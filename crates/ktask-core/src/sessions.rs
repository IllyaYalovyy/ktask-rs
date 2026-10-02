//! Port: keeping a provider session's transcript, so a resumed invocation of a provider that
//! supports it can read back what an earlier one wrote under the same session.

use std::error::Error;
use std::fmt;
use std::path::Path;

/// Why a transcript could not be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLogError(String);

impl SessionLogError {
    /// An error described by `message`, which names what failed and why.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SessionLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SessionLogError {}

/// Port: keeps a step's own files under the tool's own state directory — never the project's
/// working tree: a session's transcript, appended to, and a step's own whole prompt, written
/// fresh for the step and removed once it has run.
pub trait SessionLog {
    /// Appends `content` to the transcript at `path`, creating its parent directory first when
    /// it does not exist yet.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created, or the file cannot be opened or
    /// written.
    fn append(&self, path: &Path, content: &[u8]) -> Result<(), SessionLogError>;

    /// Writes `prompt` to `path`, creating its parent directory first when it does not exist
    /// yet, replacing whatever was there before — a step's whole prompt, so a provider whose
    /// own script or command line only ever sees part of it can still read everything it said.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created, or the file cannot be written.
    fn write_prompt(&self, path: &Path, prompt: &str) -> Result<(), SessionLogError>;

    /// Removes the prompt scratch file at `path`, once the step it was written for has run.
    /// Does nothing, successfully, when it is already gone.
    ///
    /// # Errors
    ///
    /// Fails when `path` exists but cannot be removed.
    fn remove_prompt(&self, path: &Path) -> Result<(), SessionLogError>;
}
