//! Reusable settings a new project starts with, kept once per user and instance.
//!
//! Models live in `default_models`; the rest is one versioned row. A new project copies them
//! at registration. An existing project changes only through [`Service::apply_user_defaults`],
//! after [`Service::defaults_preview`] showed each effective value. Commands, directories,
//! remotes and branches are never part of the defaults.
use super::{
    configuration::{self, DeliveryConfig, MODEL_ROLES},
    preferences::{Continuation, PlannerApproval},
    *,
};
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

pub const DEFAULT_CHECK_TIMEOUT_MS: i64 = 600_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UserDefaults {
    pub version: i64,
    pub agent_network: bool,
    pub check_before_task: bool,
    pub check_timeout_ms: i64,
    pub continuation: Continuation,
    pub planner_approval: PlannerApproval,
    pub delivery_mode: String,
}

impl Default for UserDefaults {
    fn default() -> Self {
        Self {
            version: 0,
            agent_network: false,
            check_before_task: true,
            check_timeout_ms: DEFAULT_CHECK_TIMEOUT_MS,
            continuation: Continuation::default(),
            planner_approval: PlannerApproval::default(),
            delivery_mode: DeliveryConfig::default().mode,
        }
    }
}

/// One setting with the project's value and the value the user defaults would give it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DefaultsRow {
    pub setting: String,
    pub current: String,
    pub default: String,
    /// Applying changes this setting.
    pub changes: bool,
}

pub fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

pub(super) fn read_user_defaults(db: &Connection) -> Result<UserDefaults> {
    Ok(db
        .query_row(
            "SELECT row_version,agent_network,check_before_task,check_timeout_ms,continuation,planner_approval,delivery_mode FROM user_defaults WHERE id=1",
            [],
            |r| {
                let continuation: String = r.get(4)?;
                let approval: String = r.get(5)?;
                Ok(UserDefaults {
                    version: r.get(0)?,
                    agent_network: r.get(1)?,
                    check_before_task: r.get(2)?,
                    check_timeout_ms: r.get(3)?,
                    continuation: Continuation::parse(&continuation).unwrap_or_default(),
                    planner_approval: PlannerApproval::parse(&approval).unwrap_or_default(),
                    delivery_mode: r.get(6)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

fn model_text(model: Option<&ModelConfig>) -> String {
    model.map_or_else(
        || "unconfigured".into(),
        |model| match &model.effort {
            Some(effort) => format!("{} / {effort}", model.identifier()),
            None => model.identifier(),
        },
    )
}

fn live_attempt(db: &Connection, project: i64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts WHERE project_id=?1 AND state IN ('running','paused','waiting'))",
        [project],
        |r| r.get(0),
    )?)
}

fn row(setting: &str, current: String, default: String) -> DefaultsRow {
    DefaultsRow {
        setting: setting.into(),
        changes: current != default,
        current,
        default,
    }
}

impl Service {
    pub fn user_defaults(&self) -> Result<UserDefaults> {
        read_user_defaults(self.db.connection())
    }

    /// Replaces the non-model defaults new projects start with. Existing projects keep theirs.
    pub fn save_user_defaults(
        &mut self,
        expected: &UserDefaults,
        value: &UserDefaults,
    ) -> Result<UserDefaults> {
        require(
            matches!(value.delivery_mode.as_str(), "local" | "direct" | "pr"),
            "Delivery mode must be local, direct or pr",
        )?;
        require(
            value.check_timeout_ms > 0,
            "Check timeout must be a positive number of milliseconds",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            read_user_defaults(&tx)? == *expected,
            "User defaults changed elsewhere; reopen the form",
        )?;
        let version = expected
            .version
            .checked_add(1)
            .ok_or_else(|| Error::Conflict("User defaults version exhausted".into()))?;
        tx.execute(
            "INSERT INTO user_defaults(id,row_version,agent_network,check_before_task,check_timeout_ms,continuation,planner_approval,delivery_mode) VALUES (1,?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET row_version=excluded.row_version,agent_network=excluded.agent_network,check_before_task=excluded.check_before_task,check_timeout_ms=excluded.check_timeout_ms,continuation=excluded.continuation,planner_approval=excluded.planner_approval,delivery_mode=excluded.delivery_mode",
            params![
                version,
                value.agent_network,
                value.check_before_task,
                value.check_timeout_ms,
                value.continuation.name(),
                value.planner_approval.name(),
                value.delivery_mode
            ],
        )?;
        tx.commit()?;
        Ok(UserDefaults {
            version,
            ..value.clone()
        })
    }

    /// Saves the settings and the five role models together; models are checked first.
    pub fn save_new_project_setup(
        &mut self,
        expected: &UserDefaults,
        value: &UserDefaults,
        models: &configuration::Defaults,
    ) -> Result<UserDefaults> {
        for (role, model) in MODEL_ROLES.iter().zip(models) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        let saved = self.save_user_defaults(expected, value)?;
        self.save_new_project_defaults(models)?;
        Ok(saved)
    }

    /// This project's effective values beside what the user defaults would set.
    pub fn defaults_preview(&self, config: &Path) -> Result<Vec<DefaultsRow>> {
        let preferences = self.preferences(config)?;
        preview_rows(self.db.connection(), self.project_id, &preferences)
    }

    /// Applies the previewed defaults to this project. `expected` is the preview the user
    /// saw. One immediate transaction refuses during live work, requires the preview to
    /// still be current, and writes delivery mode, flags, check timeouts and models; the
    /// preferences file follows under its own lock. If that last step fails, the error is
    /// returned and a new preview shows the remaining difference. Remote, base branch and
    /// check commands stay.
    pub fn apply_user_defaults(
        &mut self,
        config: &Path,
        expected: &[DefaultsRow],
    ) -> Result<Vec<DefaultsRow>> {
        let saved = self.preferences(config)?;
        let user = self.user_defaults()?;
        let new_models = self.new_project_defaults()?;
        for (role, model) in MODEL_ROLES.iter().zip(&new_models) {
            if let Some(model) = model {
                self.validate_model(role, model)?;
            }
        }
        let project = self.project_id;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            !live_attempt(&tx, project)?,
            "Cannot apply defaults during a live attempt",
        )?;
        require(
            !tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_sessions WHERE project_id=?1 AND state IN ('running','resume_requested'))", [project], |r| r.get::<_, bool>(0))?,
            "Cannot apply defaults during a live queue session",
        )?;
        require(
            read_user_defaults(&tx)? == user && preview_rows(&tx, project, &saved)? == expected,
            "Project or defaults changed since the preview; preview them again",
        )?;
        let delivery = configuration::delivery(&tx, project)?;
        if delivery.mode != user.delivery_mode {
            let version = delivery
                .version
                .checked_add(1)
                .ok_or_else(|| Error::Conflict("Delivery version exhausted".into()))?;
            let value = DeliveryConfig {
                mode: user.delivery_mode.clone(),
                ..delivery
            };
            configuration::write_delivery(&tx, project, version, &value)?;
        }
        tx.execute(
            "UPDATE projects SET agent_network=?2,check_before_task=?3 WHERE id=?1",
            params![project, user.agent_network, user.check_before_task],
        )?;
        tx.execute(
            "UPDATE project_checks SET timeout_ms=?2 WHERE project_id=?1",
            params![project, user.check_timeout_ms],
        )?;
        for (role, model) in MODEL_ROLES.iter().zip(&new_models) {
            if let Some(model) = model {
                tx.execute("INSERT INTO project_models(project_id,role,provider,model,effort) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(project_id,role) DO UPDATE SET provider=excluded.provider,model=excluded.model,effort=excluded.effort", params![project, role, model.provider, model.model, model.effort])?;
            }
        }
        tx.commit()?;
        if saved.continuation != user.continuation
            || saved.planner_approval != user.planner_approval
        {
            let value = preferences::Preferences {
                continuation: user.continuation,
                planner_approval: user.planner_approval,
                ..saved.clone()
            };
            self.save_preferences(config, &saved, &value)?;
        }
        self.defaults_preview(config)
    }
}

/// The project's effective values beside the defaults, read from one connection so an
/// open transaction sees a consistent snapshot. `preferences` are file-backed.
fn preview_rows(
    db: &Connection,
    project: i64,
    preferences: &preferences::Preferences,
) -> Result<Vec<DefaultsRow>> {
    let user = read_user_defaults(db)?;
    let new_models = configuration::models(
        db,
        "SELECT role,provider,model,effort FROM default_models",
        [],
    )?;
    let own_models = configuration::defaults(db, project)?;
    let mut rows = Vec::new();
    for (index, role) in MODEL_ROLES.iter().enumerate() {
        let current = model_text(own_models[index].as_ref());
        // A role without a user default keeps the project's own model.
        let default = new_models[index]
            .as_ref()
            .map_or_else(|| current.clone(), |model| model_text(Some(model)));
        rows.push(row(&format!("{role} model"), current, default));
    }
    let mut timeouts: Vec<i64> = configuration::checks(db, project)?
        .iter()
        .map(|c| c.timeout_ms)
        .collect();
    timeouts.sort_unstable();
    timeouts.dedup();
    let current_timeout = if timeouts.is_empty() {
        "no checks".to_owned()
    } else {
        timeouts
            .iter()
            .map(|ms| format!("{ms} ms"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let default_timeout = if timeouts.iter().all(|ms| *ms == user.check_timeout_ms) {
        current_timeout.clone()
    } else {
        format!("{} ms", user.check_timeout_ms)
    };
    let (network, baseline): (bool, bool) = db.query_row(
        "SELECT agent_network,check_before_task FROM projects WHERE id=?1",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    rows.extend([
        row(
            "agent network",
            on_off(network).into(),
            on_off(user.agent_network).into(),
        ),
        row(
            "baseline checks",
            on_off(baseline).into(),
            on_off(user.check_before_task).into(),
        ),
        row("check timeout", current_timeout, default_timeout),
        row(
            "run mode",
            preferences.continuation.name().into(),
            user.continuation.name().into(),
        ),
        row(
            "planner approval",
            preferences.planner_approval.name().into(),
            user.planner_approval.name().into(),
        ),
        row(
            "delivery mode",
            configuration::delivery(db, project)?.mode,
            user.delivery_mode,
        ),
    ]);
    Ok(rows)
}
