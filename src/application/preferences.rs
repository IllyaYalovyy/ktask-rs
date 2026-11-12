//! Typed, project-scoped preferences. A stable lock serializes atomic replacements.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerApproval {
    #[default]
    Automatic,
    ReviewRequired,
}
impl PlannerApproval {
    /// The name stored in SQLite and shown by the CLI.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::ReviewRequired => "review_required",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Automatic, Self::ReviewRequired]
            .into_iter()
            .find(|value| value.name() == name)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Continuation {
    Single,
    /// The supervisor keeps taking the next task until something needs the owner.
    #[default]
    Queue,
}
impl Continuation {
    /// The name stored in SQLite and shown by the CLI.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Queue => "queue",
        }
    }
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Single, Self::Queue]
            .into_iter()
            .find(|value| value.name() == name)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub version: u64,
    pub mouse: bool,
    pub planner_approval: PlannerApproval,
    pub continuation: Continuation,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 0,
            mouse: true,
            planner_approval: PlannerApproval::default(),
            continuation: Continuation::default(),
        }
    }
}
impl Service {
    /// The saved file, or what the project started with from the user defaults.
    fn read_preferences(&self, path: &Path) -> Result<Preferences> {
        match fs::read(path) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|e| Error::Conflict(format!("Invalid preferences: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (continuation, approval): (Continuation, PlannerApproval) =
                    self.db.connection().query_row(
                        "SELECT initial_continuation, initial_planner_approval FROM projects WHERE id=?1",
                        [self.project_id],
                        |r| {
                            let continuation: String = r.get(0)?;
                            let approval: String = r.get(1)?;
                            Ok((
                                Continuation::parse(&continuation).unwrap_or_default(),
                                PlannerApproval::parse(&approval).unwrap_or_default(),
                            ))
                        },
                    )?;
                Ok(Preferences {
                    continuation,
                    planner_approval: approval,
                    ..Preferences::default()
                })
            }
            Err(e) => Err(e.into()),
        }
    }
    fn preferences_path(&self, config: &Path) -> PathBuf {
        config.join(format!("project-{}.json", self.project_id))
    }
    pub fn preferences(&self, config: &Path) -> Result<Preferences> {
        self.read_preferences(&self.preferences_path(config))
    }
    pub fn save_preferences(
        &self,
        config: &Path,
        expected: &Preferences,
        value: &Preferences,
    ) -> Result<Preferences> {
        fs::create_dir_all(config)?;
        let path = self.preferences_path(config);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        require(
            self.read_preferences(&path)? == *expected,
            "Preferences changed elsewhere; cancel and reopen this form",
        )?;
        let mut saved = value.clone();
        saved.version = expected
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Conflict("Preference version exhausted".into()))?;
        let mut temp = tempfile::NamedTempFile::new_in(config)?;
        temp.write_all(
            &serde_json::to_vec_pretty(&saved).map_err(|e| Error::Conflict(e.to_string()))?,
        )?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|e| Error::Io(e.error))?;
        File::open(config)?.sync_all()?;
        Ok(saved)
    }
}
