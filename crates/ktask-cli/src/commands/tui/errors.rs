//! Errors raised while the terminal application's project state is opened or changed.

use std::fmt;

use ktask_core::{ForgetError, JournalError, RegisterError, RegistryError};

/// Why opening a project's state failed: an environment problem — the same kind every other
/// command already reports as text, since there is no further structure to it — or the journal
/// could not be used, keeping that error's own meaning until it is shown.
#[derive(Debug)]
pub(super) enum OpenProjectError {
    Environment(String),
    Journal(JournalError),
}

impl fmt::Display for OpenProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(message) => f.write_str(message),
            Self::Journal(error) => error.fmt(f),
        }
    }
}

/// Why switching the terminal application to another project failed.
#[derive(Debug)]
pub(super) enum SwitchProjectError {
    List(RegistryError),
    Unknown(String),
    Open(OpenProjectError),
}

impl fmt::Display for SwitchProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List(error) => error.fmt(f),
            Self::Unknown(name) => write!(f, "unknown project {name:?}"),
            Self::Open(error) => error.fmt(f),
        }
    }
}

/// Why forgetting a registered project failed.
#[derive(Debug)]
pub(super) enum ForgetProjectError {
    Forget(ForgetError),
    List(RegistryError),
}

impl fmt::Display for ForgetProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forget(error) => error.fmt(f),
            Self::List(error) => error.fmt(f),
        }
    }
}

/// Why registering the terminal's current directory failed.
#[derive(Debug)]
pub(super) enum RegisterProjectError {
    Register(RegisterError),
    Open(OpenProjectError),
}

impl fmt::Display for RegisterProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(error) => error.fmt(f),
            Self::Open(error) => error.fmt(f),
        }
    }
}
