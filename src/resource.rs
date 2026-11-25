//! Finite per-run storage budgets. Overrides are positive byte or entry counts.
use serde::{Deserialize, Serialize};
use std::{fmt, fs, io, os::unix::fs::MetadataExt, path::Path};

mod summary;
pub use summary::summarize;

pub fn limit(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&value| value > 0 && value <= i64::MAX as u64)
        .unwrap_or(default)
}

/// How often one limit may be raised for one attempt, each time by [`EXTENSION_FACTOR`].
pub const MAX_EXTENSIONS: u32 = 2;
pub const EXTENSION_FACTOR: u64 = 2;

/// The scratch limits `(bytes, entries)` the guardian enforces when an attempt does not
/// narrow or extend them: the single source the worker prompt states and the guardian checks.
pub fn configured_scratch() -> (u64, u64) {
    (
        Limit::ScratchBytes.configured(),
        Limit::ScratchEntries.configured(),
    )
}

/// A storage budget the supervisor can diagnose, extend or escalate when it is breached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    ScratchBytes,
    ScratchEntries,
    ArtifactFileBytes,
    ArtifactAttemptBytes,
    ArtifactAttemptFiles,
    OutputBytes,
    OutputRows,
}

impl Limit {
    pub const ALL: [Self; 7] = [
        Self::ScratchBytes,
        Self::ScratchEntries,
        Self::ArtifactFileBytes,
        Self::ArtifactAttemptBytes,
        Self::ArtifactAttemptFiles,
        Self::OutputBytes,
        Self::OutputRows,
    ];

    /// The name recorded in events and statistics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ScratchBytes => "scratch_bytes",
            Self::ScratchEntries => "scratch_entries",
            Self::ArtifactFileBytes => "artifact_file_bytes",
            Self::ArtifactAttemptBytes => "artifact_attempt_bytes",
            Self::ArtifactAttemptFiles => "artifact_attempt_files",
            Self::OutputBytes => "output_bytes",
            Self::OutputRows => "output_rows",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|limit| limit.name() == name)
    }

    /// The environment variable that overrides the default.
    pub const fn env(self) -> &'static str {
        match self {
            Self::ScratchBytes => "KTASK_SCRATCH_BYTES",
            Self::ScratchEntries => "KTASK_SCRATCH_ENTRIES",
            Self::ArtifactFileBytes => "KTASK_ARTIFACT_FILE_BYTES",
            Self::ArtifactAttemptBytes => "KTASK_ARTIFACT_ATTEMPT_BYTES",
            Self::ArtifactAttemptFiles => "KTASK_ARTIFACT_ATTEMPT_FILES",
            Self::OutputBytes => "KTASK_OUTPUT_BYTES",
            Self::OutputRows => "KTASK_OUTPUT_ROWS",
        }
    }

    pub const fn default_value(self) -> u64 {
        match self {
            Self::ScratchBytes => 4 * 1024 * 1024 * 1024,
            Self::ScratchEntries => 200_000,
            Self::ArtifactFileBytes => 128 * 1024 * 1024,
            Self::ArtifactAttemptBytes => 1024 * 1024 * 1024,
            Self::ArtifactAttemptFiles => 4096,
            Self::OutputBytes => 64 * 1024 * 1024,
            Self::OutputRows => 100_000,
        }
    }

    pub const fn is_bytes(self) -> bool {
        matches!(
            self,
            Self::ScratchBytes
                | Self::ArtifactFileBytes
                | Self::ArtifactAttemptBytes
                | Self::OutputBytes
        )
    }

    pub const fn is_scratch(self) -> bool {
        matches!(self, Self::ScratchBytes | Self::ScratchEntries)
    }

    /// The configured limit before any extension.
    pub fn configured(self) -> u64 {
        limit(self.env(), self.default_value())
    }

    /// The owner's ceiling for extensions: `<VARIABLE>_CEILING`, else four times the
    /// configured limit, which is what two extensions reach.
    pub fn ceiling(self) -> u64 {
        let configured = self.configured();
        limit(
            &format!("{}_CEILING", self.env()),
            configured.saturating_mul(EXTENSION_FACTOR.pow(MAX_EXTENSIONS)),
        )
        .max(configured)
    }

    /// The limit after `extensions` raises, never above the ceiling.
    pub fn raised(self, extensions: u32) -> u64 {
        self.configured()
            .saturating_mul(EXTENSION_FACTOR.pow(extensions.min(MAX_EXTENSIONS)))
            .min(self.ceiling())
    }

    /// Human text for a measured value: bytes or a count.
    pub fn show(self, value: u64) -> String {
        if self.is_bytes() {
            format!("{value} bytes")
        } else {
            format!(
                "{value} {}",
                if self == Self::OutputRows {
                    "rows"
                } else {
                    "entries"
                }
            )
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// A limit that stopped its producer: what was configured, what was measured, and what
/// consumed the budget, for the remediation model to read before anything pauses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Breach {
    pub limit: Limit,
    /// The limit in effect when it was breached.
    pub configured: u64,
    pub measured: u64,
    /// A stable key for what filled the budget; a repeat with the same key is not a new cause.
    pub cause: String,
    /// A bounded description of what filled the budget; empty when none could be taken.
    pub summary: String,
    /// The owner's text: the pause reason when nothing can be decided automatically.
    pub reason: String,
}

impl Breach {
    pub fn with_summary(mut self, cause: String, summary: String) -> Self {
        self.cause = cause;
        self.summary = summary;
        self
    }
}

/// Replaces digit runs so that `build-17` and `build-18` name one cause.
pub fn normalized(name: &str) -> String {
    let mut out = String::new();
    let mut digits = false;
    for character in name.chars() {
        if character.is_ascii_digit() {
            if !digits {
                out.push('#');
            }
            digits = true;
        } else {
            digits = false;
            out.push(character);
        }
    }
    out
}

/// Entries that vanish or cannot be inspected are skipped: a supervised process may create,
/// delete or lock its own files (a test fixture with mode 000, for example) during the scan,
/// and that is no storage failure. A locked directory still counts as one entry.
pub fn scratch_exceeded(root: &Path, byte_limit: u64, entry_limit: u64) -> io::Result<bool> {
    let mut bytes = 0_u64;
    let mut entries = 0_u64;
    let mut directories = vec![root.to_path_buf()];
    while let Some(path) = directories.pop() {
        let children = match fs::read_dir(path) {
            Ok(children) => children,
            Err(error) if skippable(&error) => continue,
            Err(error) => return Err(error),
        };
        for entry in children {
            let entry = entry?;
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if skippable(&error) => continue,
                Err(error) => return Err(error),
            };
            entries = entries.saturating_add(1);
            let allocated = metadata.blocks().saturating_mul(512);
            bytes = bytes.saturating_add(if metadata.is_file() {
                metadata.len().max(allocated)
            } else {
                allocated
            });
            if bytes > byte_limit || entries > entry_limit {
                return Ok(true);
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(false)
}

fn skippable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn scan_skips_directories_the_owner_locked() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let locked = root.path().join("a/locked");
        let unsearchable = root.path().join("unsearchable");
        fs::create_dir_all(&locked)?;
        fs::create_dir_all(&unsearchable)?;
        fs::write(locked.join("hidden"), [0_u8; 64])?;
        fs::write(unsearchable.join("file"), [0_u8; 64])?;
        fs::write(root.path().join("seen"), [0_u8; 64])?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        // Listable, but its children cannot be inspected.
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o400))?;

        let within = scratch_exceeded(root.path(), 1024 * 1024, 100);
        // a, a/locked, unsearchable and seen: the fifth entry is out of reach.
        let over_entries = scratch_exceeded(root.path(), 1024 * 1024, 3);
        let at_entries = scratch_exceeded(root.path(), 1024 * 1024, 4);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o700))?;
        assert!(!within?);
        assert!(over_entries?);
        assert!(!at_entries?);
        Ok(())
    }
}
