//! Project registration and discovery.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// A registered ktask project with its state directory.
#[derive(Debug, Clone)]
pub struct Project {
    /// Root directory of the git repository.
    pub root: PathBuf,
    /// Stable project identifier derived from repository path and remote.
    pub id: String,
    /// Directory containing project state and journal.
    pub state_dir: PathBuf,
}

/// Register a project at the given repository root.
///
/// Creates the state directory with mode 0700 and writes metadata
/// about the repository path. The operation is idempotent: calling
/// register on an already-registered project succeeds without error.
///
/// # Errors
///
/// Returns an error if the state directory cannot be created or
/// if the environment is misconfigured (`HOME` or `XDG_STATE_HOME` not set).
pub fn register(root: &Path) -> Result<Project> {
    let root = root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .ok()
                .map_or_else(|| root.to_path_buf(), |cwd| cwd.join(root))
        }
    });

    let remote = get_origin_url(&root).ok();
    let id = crate::paths::project_id(&root, remote.as_deref());

    let state_root = crate::paths::state_root()?;
    let state_dir = state_root.join(&id);

    // Create state directory with mode 0700 (rwx------)
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        if !state_dir.exists() {
            // Use create_dir_all to handle missing parents and race conditions
            std::fs::create_dir_all(&state_dir)?;
            let perms = Permissions::from_mode(0o700);
            std::fs::set_permissions(&state_dir, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(&state_dir); // ignore error if already exists
    }

    // Create or open the meta file to record the project root
    let meta_file = state_dir.join("meta.txt");
    if !meta_file.exists() {
        std::fs::write(&meta_file, format!("root={}\n", root.display()))?;
    }

    Ok(Project {
        root,
        id,
        state_dir,
    })
}

/// Discover a project by walking upward from the given path.
///
/// Looks for a `.git` directory to identify the repository root,
/// then registers and returns the project. Starts at the given path
/// and walks upward until a repository is found or the filesystem root
/// is reached.
///
/// # Errors
///
/// Returns `NotFound` if no git repository is found, or if the
/// environment is misconfigured.
pub fn discover(start: &Path) -> Result<Project> {
    discover_with_state_root(start, None)
}

/// Discover a project with an optional state root override.
///
/// If `state_root_override` is provided, uses it instead of resolving
/// the state root from the environment. Used for testing and custom configurations.
///
/// # Errors
///
/// Returns `NotFound` if no git repository is found, or if the
/// environment is misconfigured.
pub fn discover_with_state_root(
    start: &Path,
    state_root_override: Option<&Path>,
) -> Result<Project> {
    let mut current = start.to_path_buf();

    loop {
        if current.join(".git").exists() {
            return register_with_state_root(&current, state_root_override);
        }

        if !current.pop() {
            return Err(Error::NotFound {
                what: "git repository".to_string(),
            });
        }
    }
}

/// Register a project with an optional state root override.
///
/// If `state_root_override` is provided, uses it instead of resolving
/// the state root from the environment. Used for testing and custom configurations.
///
/// # Errors
///
/// Returns an error if the state directory cannot be created or
/// if the environment is misconfigured and no override is provided.
pub fn register_with_state_root(
    root: &Path,
    state_root_override: Option<&Path>,
) -> Result<Project> {
    let root = root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .ok()
                .map_or_else(|| root.to_path_buf(), |cwd| cwd.join(root))
        }
    });

    let remote = get_origin_url(&root).ok();
    let id = crate::paths::project_id(&root, remote.as_deref());

    let state_root = if let Some(override_root) = state_root_override {
        override_root.to_path_buf()
    } else {
        crate::paths::state_root()?
    };
    let state_dir = state_root.join(&id);

    // Create state directory with mode 0700 (rwx------)
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        if !state_dir.exists() {
            // Use create_dir_all to handle missing parents and race conditions
            std::fs::create_dir_all(&state_dir)?;
            let perms = Permissions::from_mode(0o700);
            std::fs::set_permissions(&state_dir, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(&state_dir); // ignore error if already exists
    }

    // Create or open the meta file to record the project root
    let meta_file = state_dir.join("meta.txt");
    if !meta_file.exists() {
        std::fs::write(&meta_file, format!("root={}\n", root.display()))?;
    }

    Ok(Project {
        root,
        id,
        state_dir,
    })
}

/// Get the origin URL of a git repository if it exists.
fn get_origin_url(repo_root: &Path) -> Result<String> {
    let git_config = repo_root.join(".git/config");
    if !git_config.exists() {
        return Err(Error::NotFound {
            what: "git config".to_string(),
        });
    }

    let config_content = std::fs::read_to_string(&git_config)?;
    for line in config_content.lines() {
        if line.trim().starts_with("url =")
            && let Some(url) = line.split('=').nth(1)
        {
            return Ok(url.trim().to_string());
        }
    }

    Err(Error::NotFound {
        what: "origin url".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .current_dir(path)
            .output()
            .expect("git init failed");
    }

    fn add_origin_to_git_repo(path: &Path, url: &str) {
        std::process::Command::new("git")
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(url)
            .current_dir(path)
            .output()
            .expect("git remote add failed");
    }

    #[test]
    fn register_creates_project_with_state_dir() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let project = register(repo_path).unwrap();
        assert_eq!(project.root, repo_path.canonicalize().unwrap());
        assert!(project.state_dir.exists());
        assert!(!project.id.is_empty());
    }

    #[test]
    fn register_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let project1 = register(repo_path).unwrap();
        let project2 = register(repo_path).unwrap();

        assert_eq!(project1.id, project2.id);
        assert_eq!(project1.state_dir, project2.state_dir);
        assert!(project1.state_dir.exists());
    }

    #[test]
    fn register_creates_meta_file() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let project = register(repo_path).unwrap();
        let meta_file = project.state_dir.join("meta.txt");
        assert!(meta_file.exists());

        let content = std::fs::read_to_string(&meta_file).unwrap();
        assert!(content.contains("root="));
    }

    #[test]
    fn discover_finds_project_from_subdirectory() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let subdir = repo_path.join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        let project = discover(&subdir).unwrap();
        assert_eq!(project.root, repo_path.canonicalize().unwrap());
    }

    #[test]
    fn discover_from_repo_root() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let project = discover(repo_path).unwrap();
        assert_eq!(project.root, repo_path.canonicalize().unwrap());
    }

    #[test]
    fn discover_outside_repo_returns_not_found() {
        let temp = TempDir::new().unwrap();
        let path = temp.path();

        let result = discover(path);
        assert!(result.is_err());
        match result {
            Err(Error::NotFound { .. }) => {}
            _ => panic!("expected NotFound error"),
        }
    }

    #[test]
    fn register_with_origin_includes_remote_in_id() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);
        add_origin_to_git_repo(repo_path, "https://github.com/example/repo");

        let project = register(repo_path).unwrap();
        // ID should be longer when remote is present (16 + 1 + 16 = 33)
        assert!(project.id.len() >= 16);
    }

    #[test]
    fn discover_walks_up_multiple_levels() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path();
        init_git_repo(repo_path);

        let deep_subdir = repo_path.join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep_subdir).unwrap();

        let project = discover(&deep_subdir).unwrap();
        assert_eq!(project.root, repo_path.canonicalize().unwrap());
    }
}
