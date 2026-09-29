//! The registry of projects the tool knows about.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// A project registered with the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The name the project is known by.
    pub name: String,
    /// The canonical path of the project's directory.
    pub path: PathBuf,
    /// When the project was registered.
    pub registered_at: SystemTime,
}

/// Why the registry could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
    message: String,
}

impl RegistryError {
    /// An error described by `message`, which names what failed and why.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RegistryError {}

/// Port: where the registered projects are kept.
pub trait ProjectRegistry {
    /// Every registered project, in no particular order.
    ///
    /// # Errors
    ///
    /// Fails when the registry cannot be read.
    fn list(&self) -> Result<Vec<Project>, RegistryError>;

    /// Registers `project`. The caller has checked that its name and path are free.
    ///
    /// # Errors
    ///
    /// Fails when the registry cannot be written.
    fn add(&self, project: &Project) -> Result<(), RegistryError>;

    /// Removes the project named `name` from the registry. The caller has checked that it is
    /// registered; nothing else the tool wrote for it — its journal included — is touched.
    ///
    /// # Errors
    ///
    /// Fails when the registry cannot be written.
    fn remove(&self, name: &str) -> Result<(), RegistryError>;
}

/// Use case: the registered projects, oldest registration first, ties broken by name.
///
/// # Errors
///
/// Fails when the registry cannot be read.
pub fn list_projects(registry: &impl ProjectRegistry) -> Result<Vec<Project>, RegistryError> {
    let mut projects = registry.list()?;
    projects.sort_by(|a, b| {
        a.registered_at
            .cmp(&b.registered_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeRegistry, project};

    use super::*;

    #[test]
    fn an_empty_registry_lists_nothing() {
        let registry = FakeRegistry::default();
        assert_eq!(list_projects(&registry), Ok(vec![]));
    }

    #[test]
    fn projects_come_back_oldest_registration_first() {
        let registry = FakeRegistry::with(vec![project("late", 30), project("early", 10)]);
        assert_eq!(
            list_projects(&registry),
            Ok(vec![project("early", 10), project("late", 30)])
        );
    }

    #[test]
    fn projects_registered_at_the_same_time_are_ordered_by_name() {
        let registry = FakeRegistry::with(vec![project("beta", 10), project("alpha", 10)]);
        assert_eq!(
            list_projects(&registry),
            Ok(vec![project("alpha", 10), project("beta", 10)])
        );
    }

    #[test]
    fn a_registry_failure_is_passed_on() {
        let failure = RegistryError::new("disk on fire");
        let registry = FakeRegistry::failing(failure.clone());
        assert_eq!(list_projects(&registry), Err(failure));
    }
}
