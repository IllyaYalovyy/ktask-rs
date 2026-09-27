//! The git command line.

use std::path::{Path, PathBuf};
use std::process::Command;

use ktask_core::{Git, GitError};

/// Git, by running the `git` executable found on `PATH`.
#[derive(Debug, Clone, Copy)]
pub struct GitCli;

impl Git for GitCli {
    fn work_tree_root(&self, dir: &Path) -> Result<Option<PathBuf>, GitError> {
        let fail = |cause: String| {
            GitError::new(format!(
                "cannot find the git repository of {}: {cause}",
                dir.display()
            ))
        };
        // Git's own words for "not a repository" are what tell it from a real failure, so
        // ask for them in one language.
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(dir)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| fail(format!("cannot run git: {e}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            let root = String::from_utf8(output.stdout)
                .map_err(|_| fail("git printed a path that is not text".to_owned()))?;
            Ok(Some(PathBuf::from(root.trim_end_matches('\n'))))
        } else if stderr.contains("not a git repository") {
            Ok(None)
        } else {
            Err(fail(stderr.trim().to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn init(dir: &Path) {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn the_root_of_a_repository_is_found_from_its_subdirectory() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init(&root);
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(GitCli.work_tree_root(&deep), Ok(Some(root.clone())));
        assert_eq!(GitCli.work_tree_root(&root), Ok(Some(root)));
    }

    #[test]
    fn a_directory_outside_any_repository_has_no_root() {
        let dir = TempDir::new().unwrap();
        assert_eq!(GitCli.work_tree_root(dir.path()), Ok(None));
    }

    #[test]
    fn a_directory_that_does_not_exist_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");
        let error = GitCli.work_tree_root(&missing).unwrap_err().to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
    }
}
