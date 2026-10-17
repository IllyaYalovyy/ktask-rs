//! Project configuration uses relational rows and compare-before-write transactions.
use super::*;
use rusqlite::Connection;

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
    let mut values = std::array::from_fn(|_| None);
    let mut stmt =
        db.prepare("SELECT role,provider,model,effort FROM project_models WHERE project_id=?1")?;
    for row in stmt.query_map([project], |r| {
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
    pub fn project_defaults(&self) -> Result<Defaults> {
        defaults(self.db.connection(), self.project_id)
    }
    pub fn project_checks(&self) -> Result<Vec<ProjectCheck>> {
        checks(self.db.connection(), self.project_id)
    }
    pub fn save_defaults(&mut self, expected: &Defaults, values: &Defaults) -> Result<()> {
        for model in values.iter().flatten() {
            require(
                !model.provider.trim().is_empty()
                    && !model.model.trim().is_empty()
                    && model.effort.as_ref().is_none_or(|s| !s.trim().is_empty()),
                "Provider and model are required together; effort may be blank",
            )?;
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
