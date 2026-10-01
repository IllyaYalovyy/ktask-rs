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

/// Port: appends to a session's transcript, under the tool's own state directory — never the
/// project's working tree.
pub trait SessionLog {
    /// Appends `content` to the transcript at `path`, creating its parent directory first when
    /// it does not exist yet.
    ///
    /// # Errors
    ///
    /// Fails when the parent directory cannot be created, or the file cannot be opened or
    /// written.
    fn append(&self, path: &Path, content: &[u8]) -> Result<(), SessionLogError>;
}
