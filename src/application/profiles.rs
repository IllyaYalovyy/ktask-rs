//! Named model profiles and the layered resolution of every role's model.
//!
//! A role's model comes from the first layer that sets it: the project's own override, the
//! profile the project selected, then the user defaults. A task copies the resolved value
//! into its revision, where it stays pinned; the task's own override is whatever its
//! revision sets explicitly. Profiles only map roles to models the existing provider
//! adapters run. Nothing here names a model as available; the provider decides when it runs.
use super::{
    configuration::{self, Defaults, MODEL_ROLES},
    *,
};
use rusqlite::Connection;
use serde::Serialize;

pub const MAX_NAME_CHARS: usize = 64;

/// A named set of role models. Roles it leaves unset inherit the user defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub id: i64,
    pub version: i64,
    pub name: String,
    pub models: Defaults,
}

/// Which layer supplies a role's effective model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "layer", content = "name", rename_all = "snake_case")]
pub enum Source {
    Unset,
    UserDefaults,
    Profile(String),
    Project,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::Unset => "unset".into(),
            Self::UserDefaults => "user defaults".into(),
            Self::Profile(name) => format!("profile {name}"),
            Self::Project => "project override".into(),
        }
    }
}

/// One role's effective model and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Effective {
    pub role: &'static str,
    pub model: Option<ModelConfig>,
    pub source: Source,
}

/// What still selects a profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub projects: Vec<String>,
    pub default_for_new_projects: bool,
}

impl Usage {
    pub fn in_use(&self) -> bool {
        !self.projects.is_empty() || self.default_for_new_projects
    }
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.projects.is_empty() {
            parts.push(format!("project {}", self.projects.join(", ")));
        }
        if self.default_for_new_projects {
            parts.push("the default for new projects".into());
        }
        parts.join(" and ")
    }
}

/// A task revision's pinned model beside what the project would give a new task now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pinned {
    pub role: &'static str,
    pub pinned: Option<ModelConfig>,
    pub current: Option<ModelConfig>,
    pub current_source: Source,
    /// The project would give a new revision something else.
    pub differs: bool,
}

pub fn effective_models(effective: &[Effective]) -> Defaults {
    std::array::from_fn(|index| effective[index].model.clone())
}

/// Pairs a revision's four pinned role models with the project's current effective ones.
pub fn pinned_rows(pinned: &[Option<ModelConfig>], effective: &[Effective]) -> Vec<Pinned> {
    ROLES
        .iter()
        .zip(pinned)
        .map(|(role, pinned)| {
            let now = effective
                .iter()
                .find(|row| row.role == *role)
                .expect("every revision role is a model role");
            Pinned {
                role,
                differs: *pinned != now.model,
                pinned: pinned.clone(),
                current: now.model.clone(),
                current_source: now.source.clone(),
            }
        })
        .collect()
}

fn profile_where(
    db: &Connection,
    column: &str,
    value: impl rusqlite::ToSql,
) -> Result<Option<Profile>> {
    let head = db
        .query_row(
            &format!("SELECT id,row_version,name FROM model_profiles WHERE {column}=?1"),
            [value],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    head.map(|(id, version, name)| {
        Ok(Profile {
            id,
            version,
            name,
            models: configuration::models(
                db,
                "SELECT role,provider,model,effort FROM model_profile_roles WHERE profile_id=?1",
                [id],
            )?,
        })
    })
    .transpose()
}

fn profile_by_id(db: &Connection, id: i64) -> Result<Option<Profile>> {
    profile_where(db, "id", id)
}

fn profile_by_name(db: &Connection, name: &str) -> Result<Option<Profile>> {
    profile_where(db, "name", name.trim())
}

fn usage(db: &Connection, id: i64) -> Result<Usage> {
    Ok(Usage {
        projects: db
            .prepare("SELECT name FROM projects WHERE profile_id=?1 ORDER BY name")?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?,
        default_for_new_projects: db.query_row(
            "SELECT EXISTS(SELECT 1 FROM user_defaults WHERE default_profile_id=?1)",
            [id],
            |r| r.get(0),
        )?,
    })
}

/// Resolves every role for `project`, or for a project registered now when `None`.
pub(super) fn resolve(db: &Connection, project: Option<i64>) -> Result<Vec<Effective>> {
    let user = configuration::models(
        db,
        "SELECT role,provider,model,effort FROM default_models",
        [],
    )?;
    let selected: Option<i64> = match project {
        Some(project) => db.query_row(
            "SELECT profile_id FROM projects WHERE id=?1",
            [project],
            |r| r.get(0),
        )?,
        None => db
            .query_row(
                "SELECT default_profile_id FROM user_defaults WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten(),
    };
    let profile = selected
        .map(|id| profile_by_id(db, id))
        .transpose()?
        .flatten();
    let own = match project {
        Some(project) => configuration::defaults(db, project)?,
        None => std::array::from_fn(|_| None),
    };
    Ok(MODEL_ROLES
        .iter()
        .enumerate()
        .map(|(index, role)| {
            let (model, source) = if let Some(model) = &own[index] {
                (Some(model.clone()), Source::Project)
            } else if let Some(model) = profile.as_ref().and_then(|p| p.models[index].as_ref()) {
                let name = profile
                    .as_ref()
                    .map_or_else(String::new, |p| p.name.clone());
                (Some(model.clone()), Source::Profile(name))
            } else if let Some(model) = &user[index] {
                (Some(model.clone()), Source::UserDefaults)
            } else {
                (None, Source::Unset)
            };
            Effective {
                role,
                model,
                source,
            }
        })
        .collect())
}

fn valid_name(name: &str) -> Result<&str> {
    let name = name.trim();
    require(!name.is_empty(), "Profile name is required")?;
    require(
        name.chars().count() <= MAX_NAME_CHARS && !name.chars().any(char::is_control),
        "Profile name must be at most 64 characters without control characters",
    )?;
    // "none" is how forms and the CLI say "no profile".
    require(
        !name.eq_ignore_ascii_case("none"),
        "Profile name none is reserved",
    )?;
    Ok(name)
}

impl Service {
    fn validate_profile_models(&self, models: &Defaults) -> Result<()> {
        require(
            models.iter().any(Option::is_some),
            "A profile needs at least one role model",
        )?;
        for (role, model) in MODEL_ROLES.iter().zip(models) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        Ok(())
    }

    /// Every profile, by name.
    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let db = self.db.connection();
        let ids: Vec<i64> = db
            .prepare("SELECT id FROM model_profiles ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| Ok(profile_by_id(db, id)?.expect("listed profile exists")))
            .collect()
    }

    /// The profile named `name`, ignoring case.
    pub fn profile(&self, name: &str) -> Result<Option<Profile>> {
        profile_by_name(self.db.connection(), name)
    }

    /// The profile this project selected.
    pub fn selected_profile(&self) -> Result<Option<Profile>> {
        let id: Option<i64> = self.db.connection().query_row(
            "SELECT profile_id FROM projects WHERE id=?1",
            [self.project_id],
            |r| r.get(0),
        )?;
        id.map(|id| profile_by_id(self.db.connection(), id))
            .transpose()
            .map(Option::flatten)
    }

    /// What selects `profile` now.
    pub fn profile_usage(&self, profile: &Profile) -> Result<Usage> {
        usage(self.db.connection(), profile.id)
    }

    /// This project's models with their source layer, in [`MODEL_ROLES`] order.
    pub fn effective_models(&self) -> Result<Vec<Effective>> {
        resolve(self.db.connection(), Some(self.project_id))
    }

    /// What a project registered now would get.
    pub fn new_project_effective(&self) -> Result<Vec<Effective>> {
        resolve(self.db.connection(), None)
    }

    /// Creates a profile from the given role models, each checked like any model setting.
    pub fn create_profile(&mut self, name: &str, models: &Defaults) -> Result<Profile> {
        let name = valid_name(name)?;
        self.validate_profile_models(models)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            profile_by_name(&tx, name)?.is_none(),
            &format!("Profile {name} already exists"),
        )?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO model_profiles(name,created_at_ms,updated_at_ms) VALUES (?1,?2,?2)",
            params![name, now],
        )?;
        let id = tx.last_insert_rowid();
        write_roles(&tx, id, models)?;
        let profile = profile_by_id(&tx, id)?.expect("inserted profile exists");
        tx.commit()?;
        Ok(profile)
    }

    /// Replaces a profile's name and roles. `expected` is the profile the caller read. A
    /// profile that is still selected can change a role's model but not drop the role, so
    /// no project or new-project default silently loses a value.
    pub fn save_profile(
        &mut self,
        expected: &Profile,
        name: &str,
        models: &Defaults,
    ) -> Result<Profile> {
        let name = valid_name(name)?;
        self.validate_profile_models(models)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = profile_by_id(&tx, expected.id)?
            .ok_or_else(|| Error::Conflict("Profile no longer exists; reopen the list".into()))?;
        require(&current == expected, "Profile changed elsewhere; reopen it")?;
        if let Some(other) = profile_by_name(&tx, name)? {
            require(
                other.id == current.id,
                &format!("Profile {name} already exists"),
            )?;
        }
        let used = usage(&tx, current.id)?;
        if used.in_use() {
            let dropped: Vec<&str> = MODEL_ROLES
                .iter()
                .zip(current.models.iter().zip(models))
                .filter(|(_, (old, new))| old.is_some() && new.is_none())
                .map(|(role, _)| *role)
                .collect();
            require(
                dropped.is_empty(),
                &format!(
                    "Profile {} is used by {}; keep a model for {} or select another profile first",
                    current.name,
                    used.describe(),
                    dropped.join(", ")
                ),
            )?;
        }
        if current.name == name && &current.models == models {
            return Ok(current);
        }
        let version = current
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Conflict("Profile version exhausted".into()))?;
        tx.execute(
            "UPDATE model_profiles SET name=?2,row_version=?3,updated_at_ms=?4 WHERE id=?1",
            params![current.id, name, version, now_ms()],
        )?;
        tx.execute(
            "DELETE FROM model_profile_roles WHERE profile_id=?1",
            [current.id],
        )?;
        write_roles(&tx, current.id, models)?;
        let saved = profile_by_id(&tx, current.id)?.expect("updated profile exists");
        tx.commit()?;
        Ok(saved)
    }

    /// Deletes a profile nothing selects. A selected one stays until it is replaced.
    pub fn delete_profile(&mut self, expected: &Profile) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = profile_by_id(&tx, expected.id)?
            .ok_or_else(|| Error::Conflict("Profile no longer exists; reopen the list".into()))?;
        require(&current == expected, "Profile changed elsewhere; reopen it")?;
        let used = usage(&tx, current.id)?;
        require(
            !used.in_use(),
            &format!(
                "Profile {} is used by {}; select another profile first",
                current.name,
                used.describe()
            ),
        )?;
        tx.execute(
            "DELETE FROM model_profile_roles WHERE profile_id=?1",
            [current.id],
        )?;
        tx.execute("DELETE FROM model_profiles WHERE id=?1", [current.id])?;
        tx.commit()?;
        Ok(())
    }

    /// Selects `name` for this project, or `None` to inherit the user defaults alone. Only
    /// tasks defined afterwards use the new values.
    pub fn select_profile(&mut self, name: Option<&str>) -> Result<Option<Profile>> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let profile = match name {
            Some(name) => Some(
                profile_by_name(&tx, name)?
                    .ok_or_else(|| Error::Conflict(format!("Unknown profile {}", name.trim())))?,
            ),
            None => None,
        };
        tx.execute(
            "UPDATE projects SET profile_id=?2 WHERE id=?1",
            params![self.project_id, profile.as_ref().map(|p| p.id)],
        )?;
        tx.commit()?;
        Ok(profile)
    }

    /// Chooses, or with `None` clears, the profile projects registered later select.
    pub fn set_default_profile(
        &mut self,
        name: Option<&str>,
    ) -> Result<user_defaults::UserDefaults> {
        let current = self.user_defaults()?;
        let value = user_defaults::UserDefaults {
            default_profile: match name {
                Some(name) => Some(
                    self.profile(name)?
                        .ok_or_else(|| Error::Conflict(format!("Unknown profile {}", name.trim())))?
                        .name,
                ),
                None => None,
            },
            ..current.clone()
        };
        if value == current {
            return Ok(current);
        }
        self.save_user_defaults(&current, &value)
    }
}

fn write_roles(tx: &Transaction<'_>, id: i64, models: &Defaults) -> Result<()> {
    for (role, model) in MODEL_ROLES.iter().zip(models) {
        if let Some(model) = model {
            tx.execute(
                "INSERT INTO model_profile_roles(profile_id,role,provider,model,effort) VALUES (?1,?2,?3,?4,?5)",
                params![id, role, model.provider, model.model, model.effort],
            )?;
        }
    }
    Ok(())
}
