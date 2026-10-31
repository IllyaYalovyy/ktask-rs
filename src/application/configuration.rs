//! Project configuration uses relational rows and compare-before-write transactions.
use super::*;
use rusqlite::Connection;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeliveryConfig {
    pub version: i64,
    pub mode: String,
    pub remote: String,
    pub base_branch: String,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            version: 0,
            mode: "local".into(),
            remote: "origin".into(),
            base_branch: "main".into(),
        }
    }
}

fn valid_ref_component(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && value
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

pub const MODEL_ROLES: [&str; 5] = [
    "planning",
    "execution",
    "review",
    "validation",
    "remediation",
];
pub type Defaults = [Option<ModelConfig>; 5];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectCheck {
    pub id: Option<i64>,
    pub name: String,
    pub command: String,
    pub directory: String,
    pub timeout_ms: i64,
    pub enabled: bool,
}

fn defaults(db: &Connection, project: i64) -> Result<Defaults> {
    models(
        db,
        "SELECT role,provider,model,effort FROM project_models WHERE project_id=?1",
        [project],
    )
}
fn models(db: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Defaults> {
    let mut values = std::array::from_fn(|_| None);
    let mut stmt = db.prepare(sql)?;
    for row in stmt.query_map(params, |r| {
        Ok((
            r.get::<_, String>(0)?,
            ModelConfig {
                provider: r.get(1)?,
                model: r.get(2)?,
                effort: r.get(3)?,
            },
        ))
    })? {
        let (role, model) = row?;
        if let Some(index) = MODEL_ROLES.iter().position(|r| *r == role) {
            values[index] = Some(model);
        }
    }
    Ok(values)
}
fn checks(db: &Connection, project: i64) -> Result<Vec<ProjectCheck>> {
    Ok(db.prepare("SELECT id,name,command,working_directory,timeout_ms,enabled FROM project_checks WHERE project_id=?1 ORDER BY id")?.query_map([project], |r| Ok(ProjectCheck { id: Some(r.get(0)?), name: r.get(1)?, command: r.get(2)?, directory: r.get(3)?, timeout_ms: r.get(4)?, enabled: r.get(5)? }))?.collect::<rusqlite::Result<_>>()?)
}
impl Service {
    pub fn delivery_config(&self) -> Result<DeliveryConfig> {
        Ok(self.db.connection().query_row(
            "SELECT row_version,mode,remote,base_branch FROM project_delivery WHERE project_id=?1",
            [self.project_id],
            |r| Ok(DeliveryConfig { version: r.get(0)?, mode: r.get(1)?, remote: r.get(2)?, base_branch: r.get(3)? }),
        ).optional()?.unwrap_or_default())
    }
    pub fn save_delivery_config(
        &mut self,
        expected: &DeliveryConfig,
        value: &DeliveryConfig,
    ) -> Result<DeliveryConfig> {
        require(
            matches!(value.mode.as_str(), "local" | "direct" | "pr"),
            "Delivery mode must be local, direct or pr",
        )?;
        require(
            valid_ref_component(&value.remote) && !value.remote.contains('/'),
            "Remote must be a Git remote name",
        )?;
        require(
            valid_ref_component(&value.base_branch),
            "Base branch must be a valid branch name",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('running','paused','waiting'))", [self.project_id], |r| r.get::<_, bool>(0))?, "Cannot change delivery during a live attempt")?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_sessions WHERE project_id=?1 AND state IN ('running','resume_requested'))", [self.project_id], |r| r.get::<_, bool>(0))?, "Cannot change delivery during a live queue session")?;
        let current = tx.query_row("SELECT row_version,mode,remote,base_branch FROM project_delivery WHERE project_id=?1", [self.project_id], |r| Ok(DeliveryConfig { version: r.get(0)?, mode: r.get(1)?, remote: r.get(2)?, base_branch: r.get(3)? })).optional()?.unwrap_or_default();
        require(
            &current == expected,
            "Delivery configuration changed elsewhere; reopen the form",
        )?;
        let version = current
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Conflict("Delivery version exhausted".into()))?;
        tx.execute("INSERT INTO project_delivery(project_id,row_version,mode,remote,base_branch) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project_id) DO UPDATE SET row_version=excluded.row_version,mode=excluded.mode,remote=excluded.remote,base_branch=excluded.base_branch,branch=CASE WHEN project_delivery.mode=excluded.mode AND project_delivery.remote=excluded.remote AND project_delivery.base_branch=excluded.base_branch THEN project_delivery.branch ELSE NULL END,owned_tip=CASE WHEN project_delivery.mode=excluded.mode AND project_delivery.remote=excluded.remote AND project_delivery.base_branch=excluded.base_branch THEN project_delivery.owned_tip ELSE NULL END", params![self.project_id,version,value.mode,value.remote,value.base_branch])?;
        tx.commit()?;
        Ok(DeliveryConfig {
            version,
            ..value.clone()
        })
    }
    pub fn project_defaults(&self) -> Result<Defaults> {
        defaults(self.db.connection(), self.project_id)
    }
    /// The model defaults a newly registered project starts with.
    pub fn new_project_defaults(&self) -> Result<Defaults> {
        models(
            self.db.connection(),
            "SELECT role,provider,model,effort FROM default_models",
            [],
        )
    }
    /// Replaces the defaults new projects start with; existing projects keep their own.
    pub fn save_new_project_defaults(&mut self, values: &Defaults) -> Result<()> {
        for (role, model) in MODEL_ROLES.iter().zip(values) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        let tx = self.db.connection_mut().transaction()?;
        tx.execute("DELETE FROM default_models", [])?;
        for (role, model) in MODEL_ROLES.iter().zip(values) {
            if let Some(model) = model {
                tx.execute(
                    "INSERT INTO default_models(role,provider,model,effort) VALUES (?1,?2,?3,?4)",
                    params![role, model.provider, model.model, model.effort],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    /// Whether writable agent turns in this project may use the network.
    pub fn agent_network(&self) -> Result<bool> {
        Ok(self.db.connection().query_row(
            "SELECT agent_network FROM projects WHERE id=?1",
            [self.project_id],
            |r| r.get(0),
        )?)
    }
    pub fn set_agent_network(&mut self, enabled: bool) -> Result<()> {
        self.db.connection().execute(
            "UPDATE projects SET agent_network=?2 WHERE id=?1",
            params![self.project_id, enabled],
        )?;
        Ok(())
    }
    /// Whether a task's first attempt runs the project checks before any agent works.
    pub fn check_before_task(&self) -> Result<bool> {
        Ok(self.db.connection().query_row(
            "SELECT check_before_task FROM projects WHERE id=?1",
            [self.project_id],
            |r| r.get(0),
        )?)
    }
    pub fn set_check_before_task(&mut self, enabled: bool) -> Result<()> {
        self.db.connection().execute(
            "UPDATE projects SET check_before_task=?2 WHERE id=?1",
            params![self.project_id, enabled],
        )?;
        Ok(())
    }
    pub fn project_checks(&self) -> Result<Vec<ProjectCheck>> {
        checks(self.db.connection(), self.project_id)
    }
    pub fn save_defaults(&mut self, expected: &Defaults, values: &Defaults) -> Result<()> {
        for (role, model) in MODEL_ROLES.iter().zip(values) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            &defaults(&tx, self.project_id)? == expected,
            "Defaults changed elsewhere; cancel and reopen this form",
        )?;
        for (role, model) in MODEL_ROLES.iter().zip(values) {
            if let Some(model) = model {
                tx.execute("INSERT INTO project_models(project_id,role,provider,model,effort) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project_id,role) DO UPDATE SET provider=excluded.provider,model=excluded.model,effort=excluded.effort", params![self.project_id,role,model.provider,model.model,model.effort])?;
            } else {
                tx.execute(
                    "DELETE FROM project_models WHERE project_id=?1 AND role=?2",
                    params![self.project_id, role],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn save_check(&mut self, expected: &[ProjectCheck], value: &ProjectCheck) -> Result<()> {
        require(
            !value.name.trim().is_empty()
                && !value.command.trim().is_empty()
                && value.timeout_ms > 0,
            "Check needs a name, command and positive timeout",
        )?;
        let directory = std::path::Path::new(&value.directory);
        require(
            !value.directory.trim().is_empty()
                && !directory.is_absolute()
                && !directory
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Check directory must be relative within the project",
        )?;
        require(
            self.project_root()?
                .join(directory)
                .canonicalize()?
                .starts_with(self.project_root()?)
                && self.project_root()?.join(directory).is_dir(),
            "Check directory must exist within the project",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(!tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('running','paused','waiting'))", [self.project_id], |r| r.get::<_, bool>(0))?, "Cannot change checks during a live attempt")?;
        require(
            checks(&tx, self.project_id)? == expected,
            "Checks changed elsewhere; cancel and reopen this form",
        )?;
        require(
            !expected
                .iter()
                .any(|c| c.name == value.name && c.id != value.id),
            "A check with this name already exists",
        )?;
        if let Some(id) = value.id {
            require(tx.execute("UPDATE project_checks SET name=?3,command=?4,working_directory=?5,timeout_ms=?6,enabled=?7 WHERE project_id=?1 AND id=?2", params![self.project_id,id,value.name,value.command,value.directory,value.timeout_ms,value.enabled])? == 1, "Check no longer exists")?;
        } else {
            tx.execute("INSERT INTO project_checks(project_id,name,command,working_directory,timeout_ms,enabled) VALUES (?1,?2,?3,?4,?5,?6)", params![self.project_id,value.name,value.command,value.directory,value.timeout_ms,value.enabled])?;
        }
        tx.commit()?;
        Ok(())
    }
}
