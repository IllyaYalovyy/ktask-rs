use super::Workspace;
use crate::{
    application::{Definition, ModelConfig, Service, VersionedTask, require},
    storage::{Error, Result},
};

/// All interactive remediation paths hold the same lease as execution while handing
/// unfinished work to a prerequisite. A preservation commit is not a successful attempt.
pub fn apply(
    service: &mut Service,
    target: VersionedTask,
    model: Option<ModelConfig>,
    repairs: &[Definition],
    reason: &str,
    ready: bool,
) -> Result<Vec<i64>> {
    let workspace = Workspace::acquire(&service.project_root()?)?;
    if !repairs.is_empty() {
        require(model.is_none(), "Choose one remediation strategy")?;
        require(
            !reason.trim().is_empty(),
            "A remediation reason is required",
        )?;
        for repair in repairs {
            repair.validate()?;
        }
        preserve(service, &workspace, target)?;
    }
    service.apply_remediation(target, model, repairs, reason, ready)
}

pub(super) fn preserve(
    service: &mut Service,
    workspace: &Workspace,
    target: VersionedTask,
) -> Result<()> {
    let task = service
        .current_task()?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    require(
        task.id == target.id && task.version == target.version,
        "Current task changed; reopen Remediate",
    )?;
    require(
        matches!(task.status.as_str(), "failed" | "interrupted" | "cancelled"),
        "Only failed current can be remediated",
    )?;
    if workspace.clean()? {
        return Ok(());
    }
    let base = service
        .attempt_base(task.id)?
        .ok_or_else(|| Error::Conflict("Cannot identify the unfinished work's baseline".into()))?;
    let attempt = task
        .attempts
        .last()
        .ok_or_else(|| Error::Conflict("No attempt owns the unfinished work".into()))?;
    workspace.preserve(task.id, attempt.id, &base)?;
    Ok(())
}
