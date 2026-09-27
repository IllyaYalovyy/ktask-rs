//! The project registry in SQLite.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ktask_core::{Project, ProjectRegistry, RegistryError};
use rusqlite::Connection;

/// The project registry, kept in a SQLite database file.
#[derive(Debug)]
pub struct SqliteRegistry {
    connection: Connection,
}

/// A [`RegistryError`] saying that `doing` `path` failed because of `cause`.
fn failed(doing: &str, path: &Path, cause: &dyn Display) -> RegistryError {
    RegistryError::new(format!("{doing} {}: {cause}", path.display()))
}

impl SqliteRegistry {
    /// Opens the registry database at `path`, creating the file, its directory and its
    /// tables when they do not exist yet.
    ///
    /// # Errors
    ///
    /// Fails when the directory or file cannot be created or is not a usable database.
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|e| failed("cannot create the state directory", directory, &e))?;
        }
        let fail =
            |cause: rusqlite::Error| failed("cannot open the project registry", path, &cause);
        let connection = Connection::open(path).map_err(fail)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS projects (
                     name TEXT PRIMARY KEY,
                     path TEXT NOT NULL UNIQUE,
                     registered_at INTEGER NOT NULL
                 )",
            )
            .map_err(fail)?;
        Ok(Self { connection })
    }
}

impl ProjectRegistry for SqliteRegistry {
    fn list(&self) -> Result<Vec<Project>, RegistryError> {
        let fail = |cause: rusqlite::Error| {
            RegistryError::new(format!("cannot read the project registry: {cause}"))
        };
        let mut statement = self
            .connection
            .prepare("SELECT name, path, registered_at FROM projects")
            .map_err(fail)?;
        let rows = statement
            .query_map([], |row| {
                let seconds: i64 = row.get(2)?;
                Ok(Project {
                    name: row.get(0)?,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    registered_at: from_unix_seconds(seconds),
                })
            })
            .map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)
    }
}

fn from_unix_seconds(seconds: i64) -> SystemTime {
    match u64::try_from(seconds) {
        Ok(after_epoch) => SystemTime::UNIX_EPOCH + Duration::from_secs(after_epoch),
        Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn insert(registry: &SqliteRegistry, name: &str, path: &str, seconds: i64) {
        registry
            .connection
            .execute(
                "INSERT INTO projects (name, path, registered_at) VALUES (?1, ?2, ?3)",
                (name, path, seconds),
            )
            .unwrap();
    }

    #[test]
    fn a_new_database_is_created_with_its_directory_and_lists_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("registry.db");
        let registry = SqliteRegistry::open(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(registry.list(), Ok(vec![]));
    }

    #[test]
    fn projects_survive_reopening_the_database() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.db");
        insert(
            &SqliteRegistry::open(&path).unwrap(),
            "alpha",
            "/work/alpha",
            1_000,
        );
        let listed = SqliteRegistry::open(&path).unwrap().list().unwrap();
        assert_eq!(
            listed,
            [Project {
                name: "alpha".to_owned(),
                path: PathBuf::from("/work/alpha"),
                registered_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            }]
        );
    }

    #[test]
    fn every_project_is_listed() {
        let dir = TempDir::new().unwrap();
        let registry = SqliteRegistry::open(&dir.path().join("registry.db")).unwrap();
        insert(&registry, "alpha", "/work/alpha", 1);
        insert(&registry, "beta", "/work/beta", 2);
        let mut names: Vec<_> = registry
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error_naming_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("registry.db");
        std::fs::write(
            &path,
            "this is not sqlite, and it is long enough to be checked",
        )
        .unwrap();
        let error = SqliteRegistry::open(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_an_error() {
        let dir = TempDir::new().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let error = SqliteRegistry::open(&blocker.join("registry.db")).unwrap_err();
        assert!(error.to_string().contains("state directory"), "{error}");
    }
}
