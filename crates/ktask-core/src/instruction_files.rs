//! Port: reading the instruction files — `VISION.md` and a role's own — that every agent's
//! prompt opens with.

use std::error::Error;
use std::fmt;
use std::path::Path;

/// Why an instruction file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFilesError(String);

impl InstructionFilesError {
    /// An error described by `message`, which says why the file could not be read.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for InstructionFilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for InstructionFilesError {}

/// Port: reads the text of an instruction file.
pub trait InstructionFiles {
    /// The content of the file at `path`, exactly as it is.
    ///
    /// # Errors
    ///
    /// Fails when the file does not exist or cannot be read as text.
    fn read(&self, path: &Path) -> Result<String, InstructionFilesError>;
}
