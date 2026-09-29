//! Forgetting a project the tool no longer needs to track.

use std::error::Error;
use std::fmt;

use crate::{Project, ProjectRegistry, RegistryError};

/// Why a project was not forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetError {
    /// No project is registered under this name.
    UnknownProject(String),
    /// The registry could not be read or written.
    Registry(RegistryError),
}

impl fmt::Display for ForgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProject(name) => write!(f, "unknown project {name:?}"),
            Self::Registry(error) => error.fmt(f),
        }
    }
}

impl Error for ForgetError {}

impl From<RegistryError> for ForgetError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

/// Use case: removes the project named `name` from the registry, giving back the project that
/// was removed. Nothing else the tool wrote for it — its journal included — is touched, or even
/// looked at: the caller says where it was left, from the path this returns.
///
/// # Errors
///
/// Fails, removing nothing, when no project is registered under `name`, or the registry cannot
/// be read or written.
pub fn forget_project(registry: &impl ProjectRegistry, name: &str) -> Result<Project, ForgetError> {
    let project = registry
        .list()?
        .into_iter()
        .find(|project| project.name == name)
        .ok_or_else(|| ForgetError::UnknownProject(name.to_owned()))?;
    registry.remove(name)?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeRegistry, project};

    use super::*;

    #[test]
    fn a_registered_project_is_removed_and_given_back() {
        let registry = FakeRegistry::with(vec![project("app", 10), project("other", 20)]);
        let forgotten = forget_project(&registry, "app").unwrap();
        assert_eq!(forgotten, project("app", 10));
        assert_eq!(*registry.projects.borrow(), [project("other", 20)]);
    }

    #[test]
    fn an_unknown_name_is_an_error_and_changes_nothing() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        assert_eq!(
            forget_project(&registry, "ghost"),
            Err(ForgetError::UnknownProject("ghost".to_owned()))
        );
        assert_eq!(*registry.projects.borrow(), [project("app", 10)]);
    }

    #[test]
    fn a_registry_failure_is_passed_on() {
        let failure = RegistryError::new("disk on fire");
        let registry = FakeRegistry::failing(failure.clone());
        assert_eq!(
            forget_project(&registry, "app"),
            Err(ForgetError::Registry(failure))
        );
    }
}
