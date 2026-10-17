//! Durable application storage. UI state and deterministic execution fixtures do not belong here.
pub mod execution;
mod migrations;
pub mod ordering;
pub mod usage;

use std::{fmt, fs, path::Path, time::Duration};

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
}

impl Database {
    /// CLI inspection must not create an empty application database or state directory.
    pub fn open_existing(paths: &Paths) -> Result<Option<Self>> {
        let file = paths.state.join("ktask.sqlite3");
        match fs::metadata(&file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let connection = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        Self::configure(connection).map(Some)
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
        Self::configure(Connection::open(state_root.join("ktask.sqlite3"))?)
    }

    fn configure(mut connection: Connection) -> Result<Self> {
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
        Ok(Self { connection })
    }

    /// Storage-level access, not the task command API. Callers must use transactions for
    /// cross-table changes. TUI/CLI command authorization belongs in their shared service.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}
