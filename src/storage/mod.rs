//! Durable application storage. UI state and deterministic execution fixtures do not belong here.
pub mod execution;
mod migrations;
pub mod ordering;
pub mod usage;

use std::{
    fmt, fs,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};

use crate::instance::Paths;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Incompatible(String),
    Conflict(String),
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(out, "storage: {error}"),
            Self::Sql(error) => write!(out, "database: {error}"),
            Self::Incompatible(message) => write!(out, "incompatible database: {message}"),
            Self::Conflict(message) => write!(out, "command conflict: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sql(error) => Some(error),
            Self::Incompatible(_) | Self::Conflict(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Database {
    connection: Connection,
    state_dir: fs::File,
    database_identity: (u64, u64),
}

impl Database {
    /// CLI inspection must not create an empty application database or state directory.
    pub fn open_existing(paths: &Paths) -> Result<Option<Self>> {
        let state_dir = match Self::open_directory(&paths.state) {
            Ok(directory) => directory,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let database_identity = match Self::database_identity(&state_dir, false) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let connection = Self::open_connection(&state_dir, database_identity)?;
        Self::configure(connection, state_dir, database_identity).map(Some)
    }
    /// One database per explicit instance state root; projects are rows, not directories.
    pub fn open(paths: &Paths) -> Result<Self> {
        Self::open_in(&paths.state)
    }

    /// Also used by tests with a managed, disposable state directory.
    pub fn open_in(state_root: &Path) -> Result<Self> {
        if !state_root.is_absolute() {
            return Err(Error::Incompatible("state root must be absolute".into()));
        }
        fs::create_dir_all(state_root)?;
        let state_dir = Self::open_directory(state_root)?;
        let database_identity = Self::database_identity(&state_dir, true)?;
        let connection = Self::open_connection(&state_dir, database_identity)?;
        Self::configure(connection, state_dir, database_identity)
    }

    fn open_directory(path: &Path) -> Result<fs::File> {
        Ok(fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(path)?)
    }

    fn database_identity(directory: &fs::File, create: bool) -> Result<(u64, u64)> {
        let path = format!(
            "/proc/{}/fd/{}/ktask.sqlite3",
            std::process::id(),
            directory.as_raw_fd()
        );
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                // Do not open/close extra descriptors to an existing SQLite database:
                // doing so could release this process's POSIX database locks.
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(nix::libc::O_NOFOLLOW)
                    .open(&path)
                {
                    Ok(file) => drop(file),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                fs::symlink_metadata(&path)?
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(Error::Conflict("Database is not a regular file".into()));
        }
        Ok((metadata.dev(), metadata.ino()))
    }

    fn open_connection(directory: &fs::File, expected: (u64, u64)) -> Result<Connection> {
        let path = format!(
            "/proc/{}/fd/{}/ktask.sqlite3",
            std::process::id(),
            directory.as_raw_fd()
        );
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let actual = fs::metadata(
            connection
                .path()
                .ok_or_else(|| Error::Conflict("Missing database path".into()))?,
        )?;
        let current = Self::database_identity(directory, false)?;
        if (actual.dev(), actual.ino()) != expected || current != expected {
            return Err(Error::Conflict(
                "Database identity changed while opening storage".into(),
            ));
        }
        Ok(connection)
    }

    pub(crate) fn evidence_connection(&self) -> Result<Connection> {
        Self::open_connection(&self.state_dir, self.database_identity)
    }

    fn configure(
        mut connection: Connection,
        state_dir: fs::File,
        database_identity: (u64, u64),
    ) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        // Validate identity/version before changing the persistent journal mode.
        migrations::apply(&mut connection, migrations::ALL)?;
        let mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if mode != "wal" {
            return Err(Error::Incompatible(format!("WAL unavailable: {mode}")));
        }
        Ok(Self {
            connection,
            state_dir,
            database_identity,
        })
    }

    /// Storage-level access, not the task command API. Callers must use transactions for
    /// cross-table changes. TUI/CLI command authorization belongs in their shared service.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub fn state_dir(&self) -> &fs::File {
        &self.state_dir
    }
}
