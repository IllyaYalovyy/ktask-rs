//! Registering a directory under a name of the operator's choosing.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::{Clock, Git, GitError, Project, ProjectRegistry, RegistryError};

/// Why a directory was not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The name is empty, is `.` or `..`, or contains a path separator.
    InvalidName(String),
    /// The directory is already registered, under `name`.
    AlreadyRegistered {
        /// The name the directory is registered under.
        name: String,
        /// The directory.
        path: PathBuf,
    },
    /// The name is already the name of another registered project.
    NameTaken {
        /// The requested name.
        name: String,
        /// The directory the name is registered for.
        owner: PathBuf,
    },
    /// The registry could not be read or written.
    Registry(RegistryError),
    /// Git could not tell where the repository is.
    Git(GitError),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "{name:?} is not usable as a project name: it must not be empty, `.` or `..`, or contain `/`"
            ),
            Self::AlreadyRegistered { name, path } => write!(
                f,
                "{} is already registered as project {name:?}",
                path.display()
            ),
            Self::NameTaken { name, owner } => write!(
                f,
                "project name {name:?} is already registered for {}",
                owner.display()
            ),
            Self::Registry(error) => error.fmt(f),
            Self::Git(error) => error.fmt(f),
        }
    }
}

impl Error for RegisterError {}

impl From<RegistryError> for RegisterError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<GitError> for RegisterError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

/// Use case: registers the project at the git root of `cwd`, or at `cwd` itself outside any
/// git repository, under `name`.
///
/// # Errors
///
/// Fails, registering nothing, when `name` is not usable, when the directory is already
/// registered, when `name` is taken, or when the registry or git cannot be used.
pub fn register_project(
    registry: &impl ProjectRegistry,
    git: &impl Git,
    clock: &impl Clock,
    cwd: &Path,
    name: &str,
) -> Result<Project, RegisterError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(RegisterError::InvalidName(name.to_owned()));
    }
    let root = git.work_tree_root(cwd)?.unwrap_or_else(|| cwd.to_owned());
    let known = registry.list()?;
    if let Some(project) = known.iter().find(|project| project.path == root) {
        return Err(RegisterError::AlreadyRegistered {
            name: project.name.clone(),
            path: root,
        });
    }
    if let Some(owner) = known.iter().find(|project| project.name == name) {
        return Err(RegisterError::NameTaken {
            name: name.to_owned(),
            owner: owner.path.clone(),
        });
    }
    let project = Project {
        name: name.to_owned(),
        path: root,
        registered_at: clock.now(),
    };
    registry.add(&project)?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeGit, FakeRegistry, at, project};

    use super::*;

    const NOW: u64 = 5_000;

    fn register(
        registry: &FakeRegistry,
        git: &FakeGit,
        cwd: &str,
        name: &str,
    ) -> Result<Project, RegisterError> {
        register_project(registry, git, &FakeClock(at(NOW)), Path::new(cwd), name)
    }

    fn git_with(roots: &[&str]) -> FakeGit {
        FakeGit {
            roots: roots.iter().map(PathBuf::from).collect(),
            remote_branches: Vec::new(),
            failure: None,
        }
    }

    #[test]
    fn a_new_directory_is_registered_under_the_given_name() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        let git = git_with(&["/elsewhere/app"]);
        let registered = register(&registry, &git, "/elsewhere/app/src", "app-two").unwrap();
        let expected = Project {
            name: "app-two".to_owned(),
            path: PathBuf::from("/elsewhere/app"),
            registered_at: at(NOW),
        };
        assert_eq!(registered, expected);
        assert_eq!(registry.projects.borrow().last(), Some(&expected));
        assert_eq!(registry.projects.borrow().len(), 2);
    }

    #[test]
    fn a_directory_outside_a_repository_is_registered_as_itself() {
        let registry = FakeRegistry::default();
        let registered = register(&registry, &FakeGit::default(), "/scratch/notes", "n").unwrap();
        assert_eq!(registered.path, Path::new("/scratch/notes"));
        assert_eq!(registered.name, "n");
    }

    #[test]
    fn a_name_taken_by_another_directory_is_an_error_and_changes_nothing() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        let git = git_with(&["/elsewhere/app"]);
        assert_eq!(
            register(&registry, &git, "/elsewhere/app", "app"),
            Err(RegisterError::NameTaken {
                name: "app".to_owned(),
                owner: PathBuf::from("/work/app"),
            })
        );
        assert_eq!(*registry.projects.borrow(), [project("app", 10)]);
    }

    #[test]
    fn a_registered_directory_is_an_error_naming_its_current_name() {
        let registry = FakeRegistry::with(vec![project("app", 10), project("other", 20)]);
        let git = git_with(&["/work/app"]);
        // Whether the new name is free or taken, the directory's own registration is reported.
        for name in ["fresh", "other", "app"] {
            assert_eq!(
                register(&registry, &git, "/work/app/src", name),
                Err(RegisterError::AlreadyRegistered {
                    name: "app".to_owned(),
                    path: PathBuf::from("/work/app"),
                })
            );
        }
        assert_eq!(registry.projects.borrow().len(), 2);
    }

    #[test]
    fn names_that_cannot_be_a_folder_name_are_refused() {
        let registry = FakeRegistry::default();
        for name in ["", ".", "..", "a/b", "/"] {
            assert_eq!(
                register(&registry, &FakeGit::default(), "/work/app", name),
                Err(RegisterError::InvalidName(name.to_owned()))
            );
        }
        assert!(registry.projects.borrow().is_empty());
    }

    #[test]
    fn registry_and_git_failures_are_passed_on() {
        let failure = RegistryError::new("disk on fire");
        let registry = FakeRegistry::failing(failure.clone());
        assert_eq!(
            register(&registry, &FakeGit::default(), "/work/app", "app"),
            Err(RegisterError::Registry(failure))
        );
        let git = FakeGit {
            failure: Some(GitError::new("git exploded")),
            ..FakeGit::default()
        };
        assert_eq!(
            register(&FakeRegistry::default(), &git, "/work/app", "app"),
            Err(RegisterError::Git(GitError::new("git exploded")))
        );
    }
}
