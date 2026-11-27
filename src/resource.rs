//! Finite per-run storage budgets. Overrides are positive byte or entry counts.
use serde::{Deserialize, Serialize};
use std::{fmt, fs, io, os::unix::fs::MetadataExt, path::Path};

mod summary;
pub use summary::summarize;

pub fn limit(name: &str, default: u64) -> u64 {
    positive(name).unwrap_or(default)
}

/// How often one limit may be raised for one attempt, each time by [`EXTENSION_FACTOR`].
pub const MAX_EXTENSIONS: u32 = 2;
pub const EXTENSION_FACTOR: u64 = 2;

/// The scratch limits `(bytes, entries)` of a worker that belongs to no stored project
/// setting: the test override, else the built-in default.
pub fn fallback_scratch() -> (u64, u64) {
    (
        Limit::ScratchBytes.fallback(),
        Limit::ScratchEntries.fallback(),
    )
}

/// A storage budget the owner can see and change. All but the runner log stop or truncate
/// work, and can be diagnosed, extended or raised.
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
    RunnerLogBytes,
}

/// What one scope (a project or the instance) stores for a limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub value: Option<u64>,
    /// A raise ktask made for steady growth; only a project has one.
    pub extended: Option<u64>,
    pub ceiling: Option<u64>,
}

/// The limit and ceiling in force, and which layer set the limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effective {
    pub value: u64,
    pub ceiling: u64,
    pub source: &'static str,
}

impl Effective {
    /// The limit after `extensions` raises, never above the ceiling.
    pub fn raised(self, extensions: u32) -> u64 {
        self.value
            .saturating_mul(EXTENSION_FACTOR.pow(extensions.min(MAX_EXTENSIONS)))
            .min(self.ceiling)
    }
}

impl Limit {
    /// The limits a breach can name.
    pub const ALL: [Self; 7] = [
        Self::ScratchBytes,
        Self::ScratchEntries,
        Self::ArtifactFileBytes,
        Self::ArtifactAttemptBytes,
        Self::ArtifactAttemptFiles,
        Self::OutputBytes,
        Self::OutputRows,
    ];

    /// Every limit that is a stored setting.
    pub const SETTINGS: [Self; 8] = [
        Self::ScratchBytes,
        Self::ScratchEntries,
        Self::ArtifactFileBytes,
        Self::ArtifactAttemptBytes,
        Self::ArtifactAttemptFiles,
        Self::OutputBytes,
        Self::OutputRows,
        Self::RunnerLogBytes,
    ];

    /// The name recorded in settings, events and statistics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ScratchBytes => "scratch_bytes",
            Self::ScratchEntries => "scratch_entries",
            Self::ArtifactFileBytes => "artifact_file_bytes",
            Self::ArtifactAttemptBytes => "artifact_attempt_bytes",
            Self::ArtifactAttemptFiles => "artifact_attempt_files",
            Self::OutputBytes => "output_bytes",
            Self::OutputRows => "output_rows",
            Self::RunnerLogBytes => "runner_log_bytes",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::SETTINGS
            .into_iter()
            .find(|limit| limit.name() == name)
    }

    /// The environment variable that overrides every stored setting, for tests.
    pub const fn env(self) -> &'static str {
        match self {
            Self::ScratchBytes => "KTASK_SCRATCH_BYTES",
            Self::ScratchEntries => "KTASK_SCRATCH_ENTRIES",
            Self::ArtifactFileBytes => "KTASK_ARTIFACT_FILE_BYTES",
            Self::ArtifactAttemptBytes => "KTASK_ARTIFACT_ATTEMPT_BYTES",
            Self::ArtifactAttemptFiles => "KTASK_ARTIFACT_ATTEMPT_FILES",
            Self::OutputBytes => "KTASK_OUTPUT_BYTES",
            Self::OutputRows => "KTASK_OUTPUT_ROWS",
            Self::RunnerLogBytes => "KTASK_RUNNER_LOG_BYTES",
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
            Self::RunnerLogBytes => 8 * 1024 * 1024,
        }
    }

    pub const fn is_bytes(self) -> bool {
        matches!(
            self,
            Self::ScratchBytes
                | Self::ArtifactFileBytes
                | Self::ArtifactAttemptBytes
                | Self::OutputBytes
                | Self::RunnerLogBytes
        )
    }

    pub const fn is_scratch(self) -> bool {
        matches!(self, Self::ScratchBytes | Self::ScratchEntries)
    }

    pub const fn is_output(self) -> bool {
        matches!(self, Self::OutputBytes | Self::OutputRows)
    }

    /// The test override of the limit, when its variable holds a positive integer.
    pub fn env_value(self) -> Option<u64> {
        positive(self.env())
    }

    /// The test override of the owner's ceiling, `<VARIABLE>_CEILING`.
    pub fn env_ceiling(self) -> Option<u64> {
        positive(&format!("{}_CEILING", self.env()))
    }

    /// The limit where no stored setting applies.
    pub fn fallback(self) -> u64 {
        self.env_value().unwrap_or_else(|| self.default_value())
    }

    /// The layers, strongest first: the test override, the project, the instance, the
    /// built-in default. An automatic extension raises the limit only up to the ceiling,
    /// which stays four times what the owner configured unless set, and never below it.
    pub fn resolve(
        self,
        env: (Option<u64>, Option<u64>),
        project: Stored,
        instance: Stored,
    ) -> Effective {
        let (configured, source) = match (env.0, project.value, instance.value) {
            (Some(value), ..) => (value, "env"),
            (None, Some(value), _) => (value, "project"),
            (None, None, Some(value)) => (value, "instance"),
            (None, None, None) => (self.default_value(), "default"),
        };
        let ceiling = env
            .1
            .or(project.ceiling)
            .or(instance.ceiling)
            .unwrap_or_else(|| configured.saturating_mul(EXTENSION_FACTOR.pow(MAX_EXTENSIONS)))
            .max(configured);
        let extended = project
            .extended
            .filter(|_| env.0.is_none())
            .map(|value| value.min(ceiling))
            .filter(|&value| value > configured);
        Effective {
            value: extended.unwrap_or(configured),
            ceiling,
            source: if extended.is_some() {
                "automatic"
            } else {
                source
            },
        }
    }

    /// Human text for a measured value: bytes or a count.
    pub fn show(self, value: u64) -> String {
        if self.is_bytes() {
            format!("{value} bytes")
        } else {
            let unit = match self {
                Self::OutputRows => "rows",
                Self::ArtifactAttemptFiles => "files",
                _ => "entries",
            };
            format!("{value} {unit}")
        }
    }
}

fn positive(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&value| value > 0 && value <= i64::MAX as u64)
}

impl fmt::Display for Limit {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// One dimension of the instance's shared storage budget. The retention policy stores it;
/// it is not a resource limit setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageBudget {
    StorageMaxBytes,
    StorageMaxFiles,
}

/// What a resource pause waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PauseLimit {
    Resource(Limit),
    Storage(StorageBudget),
}

impl PauseLimit {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Resource(limit) => limit.name(),
            Self::Storage(StorageBudget::StorageMaxBytes) => "storage_max_bytes",
            Self::Storage(StorageBudget::StorageMaxFiles) => "storage_max_files",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [
            StorageBudget::StorageMaxBytes,
            StorageBudget::StorageMaxFiles,
        ]
        .map(Self::Storage)
        .into_iter()
        .find(|budget| budget.name() == name)
        .or_else(|| Limit::parse(name).map(Self::Resource))
    }

    /// Human text for a measured value, with its unit.
    pub fn show(self, value: u64) -> String {
        match self {
            Self::Resource(limit) => limit.show(value),
            Self::Storage(StorageBudget::StorageMaxBytes) => format!("{value} bytes"),
            Self::Storage(StorageBudget::StorageMaxFiles) => format!("{value} files"),
        }
    }
}

impl fmt::Display for PauseLimit {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// The storage budget refused work to keep its reserve free.
#[derive(Clone, Debug)]
pub struct StorageRefusal {
    pub budget: StorageBudget,
    /// The effective budget in force.
    pub value: u64,
    /// The smallest budget that holds the allocation, the refused claim and the reserve.
    pub need: u64,
    /// What the refused write would have added to the projected allocation.
    pub claim: u64,
    /// The owner's text: the pause reason.
    pub reason: String,
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
    let (bytes, entries) = scratch_usage(root, byte_limit, entry_limit)?;
    Ok(bytes > byte_limit || entries > entry_limit)
}

/// `(bytes, entries)` under `root`. The scan stops once either limit is exceeded.
pub fn scratch_usage(root: &Path, byte_limit: u64, entry_limit: u64) -> io::Result<(u64, u64)> {
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
                return Ok((bytes, entries));
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok((bytes, entries))
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

    #[test]
    fn layers_resolve_in_order_and_an_extension_stays_under_the_owner_ceiling() {
        let limit = Limit::ArtifactAttemptFiles;
        let none = Stored::default();
        let stored = |value, extended, ceiling| Stored {
            value,
            extended,
            ceiling,
        };
        let resolved = |env, project, instance| {
            let effective = limit.resolve(env, project, instance);
            (effective.value, effective.ceiling, effective.source)
        };
        assert_eq!(
            resolved((None, None), none, none),
            (4096, 16_384, "default")
        );
        let instance = stored(Some(10), None, None);
        assert_eq!(resolved((None, None), none, instance), (10, 40, "instance"));
        let project = stored(Some(20), None, None);
        assert_eq!(
            resolved((None, None), project, instance),
            (20, 80, "project")
        );
        // An automatic raise keeps the ceiling anchored to the owner's value.
        let grown = stored(Some(20), Some(40), None);
        assert_eq!(resolved((None, None), grown, none), (40, 80, "automatic"));
        let capped = stored(Some(20), Some(500), Some(50));
        assert_eq!(resolved((None, None), capped, none), (50, 50, "automatic"));
        // The ceiling is never below the limit, and a test override beats everything.
        let low = stored(Some(20), None, Some(5));
        assert_eq!(resolved((None, None), low, none), (20, 20, "project"));
        assert_eq!(resolved((Some(3), None), grown, instance), (3, 12, "env"));
        assert_eq!(resolved((Some(3), Some(4)), grown, instance).1, 4);
    }
}
