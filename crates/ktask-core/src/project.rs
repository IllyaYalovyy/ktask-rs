//! `Project`: registration and discovery of a repository's state directory.
//!
//! Registering a repository creates its private state directory under
//! [`paths::state_root`] and opens its journal database, recording the
//! repository's canonical path in the journal's `meta` table so that a
//! later `discover` from anywhere inside the repository can find it again.

use crate::{Error, Result, paths};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// The key `register` stores the repository's canonical path under in the
/// journal's `meta` table.
const REPO_PATH_KEY: &str = "repo_path";

/// A repository registered with ktask-rs, and where its state lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The repository's canonical (symlink-resolved, absolute) path.
    pub root: PathBuf,
    /// The project's stable identifier; see [`paths::project_id`].
    pub id: String,
    /// The private directory ktask-rs stores this project's journal under.
    pub state_dir: PathBuf,
}

impl Project {
    /// Registers `root` as a project.
    ///
    /// Creates `root`'s state directory (mode `0700` on Unix) under
    /// [`paths::state_root`] if it does not already exist, opens its journal
    /// database, and records `root`'s canonical path in the journal's `meta`
    /// table. Calling this again for the same `root` is a no-op beyond
    /// re-affirming that row: it neither fails nor duplicates it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when `root` does not exist or its state
    /// directory cannot be created, and [`Error::Database`] when the journal
    /// database cannot be opened or written.
    pub fn register(root: &Path) -> Result<Project> {
        register_with(root, &|key| std::env::var(key).ok())
    }

    /// Walks upward from `start`, returning the first ancestor (inclusive)
    /// already registered as a project's root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] when no ancestor of `start` is a
    /// registered project's root.
    pub fn discover(start: &Path) -> Result<Project> {
        discover_with(start, &|key| std::env::var(key).ok())
    }
}

fn register_with(root: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Project> {
    let canonical = std::fs::canonicalize(root)?;
    let id = paths::project_id(&canonical, None);
    let state_dir = paths::state_root_with(env)?.join(&id);
    std::fs::create_dir_all(&state_dir)?;
    set_private(&state_dir)?;

    let conn = Connection::open(state_dir.join("journal.db"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (REPO_PATH_KEY, canonical.to_string_lossy().as_ref()),
    )?;

    Ok(Project {
        root: canonical,
        id,
        state_dir,
    })
}

fn discover_with(start: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Project> {
    let canonical_start = std::fs::canonicalize(start)?;
    let state_root = paths::state_root_with(env)?;

    let mut candidate = canonical_start.as_path();
    loop {
        let id = paths::project_id(candidate, None);
        let state_dir = state_root.join(&id);
        if state_dir.join("journal.db").is_file() {
            return Ok(Project {
                root: candidate.to_path_buf(),
                id,
                state_dir,
            });
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => {
                return Err(Error::NotFound {
                    what: format!("project containing {}", canonical_start.display()),
                });
            }
        }
    }
}

/// Restricts `dir` to owner-only access. A no-op on non-Unix targets, since
/// there is no equivalent mode bit to set.
#[cfg(unix)]
fn set_private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_home(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state")
    }

    #[test]
    fn register_creates_the_state_directory_with_owner_only_permissions() {
        let repo = tempfile::tempdir().expect("repo dir");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let project = register_with(repo.path(), &env).expect("register");

        assert!(project.state_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&project.state_dir)
                .expect("stat state dir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn register_writes_the_repo_path_into_the_journals_meta_table() {
        let repo = tempfile::tempdir().expect("repo dir");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let project = register_with(repo.path(), &env).expect("register");

        let conn = Connection::open(project.state_dir.join("journal.db")).expect("open journal");
        let stored: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'repo_path'",
                [],
                |row| row.get(0),
            )
            .expect("meta row");
        assert_eq!(stored, project.root.to_string_lossy());
    }

    #[test]
    fn register_is_idempotent() {
        let repo = tempfile::tempdir().expect("repo dir");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let first = register_with(repo.path(), &env).expect("first register");
        let second = register_with(repo.path(), &env).expect("second register");

        assert_eq!(first, second);

        let conn = Connection::open(first.state_dir.join("journal.db")).expect("open journal");
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'repo_path'",
                [],
                |row| row.get(0),
            )
            .expect("count rows");
        assert_eq!(rows, 1);
    }

    #[test]
    fn discover_from_a_subdirectory_finds_the_registered_project() {
        let repo = tempfile::tempdir().expect("repo dir");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);
        let sub = repo.path().join("a").join("b");
        std::fs::create_dir_all(&sub).expect("create subdir");

        let registered = register_with(repo.path(), &env).expect("register");
        let discovered = discover_with(&sub, &env).expect("discover");

        assert_eq!(discovered, registered);
    }

    #[test]
    fn discover_outside_any_project_is_not_found() {
        let repo = tempfile::tempdir().expect("repo dir");
        let state = tempfile::tempdir().expect("state dir");
        let env = env_with_state_home(&state);

        let err = discover_with(repo.path(), &env).expect_err("must fail");
        let canonical = std::fs::canonicalize(repo.path()).expect("canonicalize");
        assert!(
            matches!(&err, Error::NotFound { what } if what.contains(&canonical.to_string_lossy().to_string()))
        );
    }

    fn env_with_state_home(dir: &tempfile::TempDir) -> impl Fn(&str) -> Option<String> {
        let home = state_home(dir).to_string_lossy().to_string();
        move |key| (key == "XDG_STATE_HOME").then(|| home.clone())
    }
}
