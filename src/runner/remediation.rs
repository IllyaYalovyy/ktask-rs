use super::Workspace;
use crate::{
    application::{
        Actor, ActorKind, Definition, Kind, ModelConfig, Service, VersionedTask, require,
    },
    runner::codex::Judgment,
    storage::{Error, Result},
};

/// Apply a reviewed failure judgment through versioned application commands.
pub fn apply_judgment(
    service: &mut Service,
    target: VersionedTask,
    judgment: &Judgment,
    ready: bool,
) -> Result<Vec<i64>> {
    let current = service
        .current_task()?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    require(
        current.id == target.id && current.version == target.version && current.status == "failed",
        "Only failed current can be remediated",
    )?;
    require(
        service.open_question(target.id)?.is_none(),
        "Answer the owner question before another judgment",
    )?;
    match judgment {
        Judgment::Repair { plan, reason } => {
            service.record_failure_judgment(target, "repair", plan, reason)?;
            Ok(vec![])
        }
        Judgment::Ask { question, reason } => {
            service.record_failure_judgment(target, "ask", question, reason)?;
            Ok(vec![])
        }
        Judgment::Revise {
            title,
            goal,
            criteria,
            reason,
        } => {
            let mut definition = current.definition().definition.clone();
            definition.title = title.clone();
            definition.goal = goal.clone();
            definition.criteria = criteria.clone();
            service.revise_as(
                target,
                &definition,
                ready,
                &Actor {
                    kind: ActorKind::Ai,
                    name: "judge".into(),
                },
                Some(reason),
            )?;
            Ok(vec![target.id])
        }
        Judgment::Insert { tasks, reason } => {
            let defaults = service.defaults()?;
            let repairs = tasks
                .iter()
                .map(|task| Definition {
                    title: task.title.clone(),
                    goal: task.goal.clone(),
                    kind: Kind::Ai,
                    criteria: task.criteria.clone(),
                    models: defaults.clone(),
                    attempt_limit: Some(3),
                    timeout_ms: current.definition().definition.timeout_ms,
                })
                .collect::<Vec<_>>();
            apply(service, target, None, &repairs, reason, ready)
        }
        Judgment::Escalate { model, reason } => {
            apply(service, target, Some(model.clone()), &[], reason, true)
        }
    }
}

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
