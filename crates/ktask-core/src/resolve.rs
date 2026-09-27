//! Which project a command works on.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::{Clock, Git, GitError, Project, ProjectRegistry, RegistryError};

/// The project a command works on, and whether resolving it registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The resolved project.
    pub project: Project,
    /// True when the project was not known before and has just been registered.
    pub registered: bool,
}

/// Why no project could be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `--project` named a project that is not registered.
    UnknownProject(String),
    /// The directory's folder name is already the name of another registered project.
    NameTaken {
        /// The folder name.
        name: String,
        /// The directory that would have been registered.
        path: PathBuf,
        /// The directory the name is registered for.
        owner: PathBuf,
    },
    /// The directory has no folder name to register it under, or one that is not text.
    Unnameable(PathBuf),
    /// The registry could not be read or written.
    Registry(RegistryError),
    /// Git could not tell where the repository is.
    Git(GitError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProject(name) => write!(f, "unknown project {name:?}"),
            Self::NameTaken { name, path, owner } => write!(
                f,
                "cannot register {} as project {name:?}: that name is already registered for {}",
                path.display(),
                owner.display()
            ),
            Self::Unnameable(path) => write!(
                f,
                "cannot register {}: its folder name is not usable as a project name",
                path.display()
            ),
            Self::Registry(error) => error.fmt(f),
            Self::Git(error) => error.fmt(f),
        }
    }
}

impl Error for ResolveError {}

impl From<RegistryError> for ResolveError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<GitError> for ResolveError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

/// Use case: the project a command works on.
///
/// The registered project called `selected` when one is given. Otherwise the project at the
/// git root of `cwd`, or at `cwd` itself outside any git repository; a directory not yet
/// registered is registered under its folder name.
///
/// # Errors
///
/// Fails when `selected` is not registered, when the directory cannot be registered, or when
/// the registry or git cannot be used.
pub fn resolve_project(
    registry: &impl ProjectRegistry,
    git: &impl Git,
    clock: &impl Clock,
    cwd: &Path,
    selected: Option<&str>,
) -> Result<Resolution, ResolveError> {
    let known = registry.list()?;
    if let Some(name) = selected {
        return known
            .into_iter()
            .find(|project| project.name == name)
            .map(|project| Resolution {
                project,
                registered: false,
            })
            .ok_or_else(|| ResolveError::UnknownProject(name.to_owned()));
    }
    let root = git.work_tree_root(cwd)?.unwrap_or_else(|| cwd.to_owned());
    if let Some(project) = known.iter().find(|project| project.path == root) {
        return Ok(Resolution {
            project: project.clone(),
            registered: false,
        });
    }
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ResolveError::Unnameable(root.clone()))?
        .to_owned();
    if let Some(owner) = known.iter().find(|project| project.name == name) {
        return Err(ResolveError::NameTaken {
            name,
            path: root,
            owner: owner.path.clone(),
        });
    }
    let project = Project {
        name,
        path: root,
        registered_at: clock.now(),
    };
    registry.add(&project)?;
    Ok(Resolution {
        project,
        registered: true,
    })
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeGit, FakeRegistry, at, project};

    use super::*;

    const NOW: u64 = 5_000;

    fn resolve(
        registry: &FakeRegistry,
        git: &FakeGit,
        cwd: &str,
        selected: Option<&str>,
    ) -> Result<Resolution, ResolveError> {
        resolve_project(registry, git, &FakeClock(at(NOW)), Path::new(cwd), selected)
    }

    fn git_with(roots: &[&str]) -> FakeGit {
        FakeGit {
            roots: roots.iter().map(PathBuf::from).collect(),
            failure: None,
        }
    }

    #[test]
    fn a_new_git_repository_is_registered_under_its_folder_name() {
        let registry = FakeRegistry::default();
        let git = git_with(&["/work/app"]);
        let resolution = resolve(&registry, &git, "/work/app", None).unwrap();
        let expected = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: at(NOW),
        };
        assert_eq!(
            resolution,
            Resolution {
                project: expected.clone(),
                registered: true
            }
        );
        assert_eq!(*registry.projects.borrow(), [expected]);
    }

    #[test]
    fn a_registered_project_is_found_again_and_nothing_is_registered() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        let git = git_with(&["/work/app"]);
        let resolution = resolve(&registry, &git, "/work/app", None).unwrap();
        assert_eq!(
            resolution,
            Resolution {
                project: project("app", 10),
                registered: false
            }
        );
        assert_eq!(registry.projects.borrow().len(), 1);
    }

    #[test]
    fn a_subdirectory_resolves_to_the_repository_root() {
        let registry = FakeRegistry::default();
        let git = git_with(&["/work/app"]);
        let first = resolve(&registry, &git, "/work/app/src/deep", None).unwrap();
        assert_eq!(first.project.path, Path::new("/work/app"));
        assert!(first.registered);
        let second = resolve(&registry, &git, "/work/app", None).unwrap();
        assert_eq!(second.project, first.project);
        assert!(!second.registered);
        assert_eq!(registry.projects.borrow().len(), 1);
    }

    #[test]
    fn outside_a_repository_the_directory_itself_is_the_project() {
        let registry = FakeRegistry::default();
        let resolution = resolve(&registry, &FakeGit::default(), "/scratch/notes", None).unwrap();
        assert_eq!(resolution.project.name, "notes");
        assert_eq!(resolution.project.path, Path::new("/scratch/notes"));
        assert!(resolution.registered);
    }

    #[test]
    fn a_selected_project_is_found_by_name_from_any_directory() {
        let registry = FakeRegistry::with(vec![project("app", 10), project("other", 20)]);
        let resolution = resolve(&registry, &FakeGit::default(), "/elsewhere", Some("other"));
        assert_eq!(
            resolution,
            Ok(Resolution {
                project: project("other", 20),
                registered: false
            })
        );
        assert_eq!(registry.projects.borrow().len(), 2);
    }

    #[test]
    fn a_selected_project_that_is_not_registered_is_an_error_and_registers_nothing() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        let git = git_with(&["/work/ghost"]);
        let result = resolve(&registry, &git, "/work/ghost", Some("ghost"));
        assert_eq!(
            result,
            Err(ResolveError::UnknownProject("ghost".to_owned()))
        );
        assert_eq!(registry.projects.borrow().len(), 1);
    }

    #[test]
    fn a_folder_name_registered_for_another_path_is_an_error() {
        let registry = FakeRegistry::with(vec![project("app", 10)]);
        let git = git_with(&["/elsewhere/app"]);
        let result = resolve(&registry, &git, "/elsewhere/app", None);
        assert_eq!(
            result,
            Err(ResolveError::NameTaken {
                name: "app".to_owned(),
                path: PathBuf::from("/elsewhere/app"),
                owner: PathBuf::from("/work/app"),
            })
        );
        assert_eq!(registry.projects.borrow().len(), 1);
    }

    #[test]
    fn a_directory_without_a_folder_name_cannot_be_registered() {
        let registry = FakeRegistry::default();
        let result = resolve(&registry, &FakeGit::default(), "/", None);
        assert_eq!(result, Err(ResolveError::Unnameable(PathBuf::from("/"))));
        assert!(registry.projects.borrow().is_empty());
    }

    #[test]
    fn registry_and_git_failures_are_passed_on() {
        let failure = RegistryError::new("disk on fire");
        let registry = FakeRegistry::failing(failure.clone());
        for selected in [None, Some("app")] {
            let result = resolve(&registry, &FakeGit::default(), "/work/app", selected);
            assert_eq!(result, Err(ResolveError::Registry(failure.clone())));
        }
        let git = FakeGit {
            failure: Some(GitError::new("git exploded")),
            ..FakeGit::default()
        };
        let result = resolve(&FakeRegistry::default(), &git, "/work/app", None);
        assert_eq!(
            result,
            Err(ResolveError::Git(GitError::new("git exploded")))
        );
    }

    #[test]
    fn a_failed_registration_is_passed_on() {
        struct ReadOnly(FakeRegistry);
        impl ProjectRegistry for ReadOnly {
            fn list(&self) -> Result<Vec<Project>, RegistryError> {
                self.0.list()
            }
            fn add(&self, _: &Project) -> Result<(), RegistryError> {
                Err(RegistryError::new("read-only"))
            }
        }
        let result = resolve_project(
            &ReadOnly(FakeRegistry::default()),
            &FakeGit::default(),
            &FakeClock(at(NOW)),
            Path::new("/work/app"),
            None,
        );
        assert_eq!(
            result,
            Err(ResolveError::Registry(RegistryError::new("read-only")))
        );
    }
}
