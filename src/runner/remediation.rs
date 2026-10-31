use super::Workspace;
use crate::{
    application::{
        Actor, ActorKind, Definition, Kind, ModelConfig, Service, VersionedTask, require,
    },
    runner::agent::{Judgment, ModelOverrides},
    storage::{Error, Result},
};

pub fn apply_role_models(definition: &mut Definition, changes: &ModelOverrides) -> Result<()> {
    for (role, model) in changes.effective() {
        let index = crate::application::ROLES
            .iter()
            .position(|candidate| candidate == role)
            .ok_or_else(|| Error::Conflict(format!("Unknown revised model role {role}")))?;
        model.validate_for_role(role)?;
        definition.models[index] = Some(model.clone());
    }
    for (index, role) in crate::application::ROLES.iter().enumerate().skip(1) {
        require(
            !definition.reuses_execution_model(index),
            &format!(
                "{role} must use a model distinct from execution; changing effort alone is insufficient"
            ),
        )?;
    }
    Ok(())
}

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
            models,
            reason,
        } => {
            let mut definition = current.definition().definition.clone();
            definition.title = title.clone();
            definition.goal = goal.clone();
            definition.criteria = criteria.clone();
            apply_role_models(&mut definition, models)?;
            service.validate_judge_models(&definition)?;
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
                    stage_timeout_ms: current.definition().definition.stage_timeout_ms,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revised_models_reject_invalid_effort_and_judge_reuse() {
        let model = |name: &str, effort: &str| ModelConfig {
            provider: "codex".into(),
            model: name.into(),
            effort: Some(effort.into()),
        };
        let mut definition = Definition {
            title: "Task".into(),
            goal: "Goal".into(),
            kind: Kind::Ai,
            criteria: vec!["Check".into()],
            models: [
                Some(model("execution", "high")),
                Some(model("review", "high")),
                Some(model("validation", "high")),
                Some(model("remediation", "high")),
            ],
            attempt_limit: Some(3),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        let mut changes = std::collections::BTreeMap::new();
        changes.insert("execution".into(), None);
        changes.insert("review".into(), Some(model("review", "")));
        changes.insert("validation".into(), None);
        changes.insert("remediation".into(), None);
        let mut changes = ModelOverrides(Some(changes));
        assert!(
            apply_role_models(&mut definition, &changes)
                .unwrap_err()
                .to_string()
                .contains("review effort must be nonempty")
        );
        changes
            .0
            .as_mut()
            .unwrap()
            .insert("review".into(), Some(model("execution", "low")));
        assert!(
            apply_role_models(&mut definition, &changes)
                .unwrap_err()
                .to_string()
                .contains("distinct from execution")
        );
    }

    #[test]
    fn null_role_overrides_preserve_existing_models() {
        let model = |name: &str| ModelConfig {
            provider: "codex".into(),
            model: name.into(),
            effort: None,
        };
        let mut definition = Definition {
            title: "Task".into(),
            goal: "Goal".into(),
            kind: Kind::Ai,
            criteria: vec!["Proof".into()],
            models: [
                Some(model("worker")),
                Some(model("review")),
                Some(model("validation")),
                Some(model("remediation")),
            ],
            attempt_limit: Some(3),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        let original = definition.models.clone();
        let changes = ModelOverrides(Some(std::collections::BTreeMap::from([
            ("execution".into(), None),
            ("review".into(), Some(model("strong-review"))),
            ("validation".into(), None),
            ("remediation".into(), None),
        ])));
        apply_role_models(&mut definition, &changes).unwrap();
        assert_eq!(definition.models[0], original[0]);
        assert_eq!(
            definition.models[1].as_ref().unwrap().model,
            "strong-review"
        );
        assert_eq!(definition.models[2], original[2]);
        assert_eq!(definition.models[3], original[3]);
        apply_role_models(&mut definition, &ModelOverrides(None)).unwrap();
        assert_eq!(
            definition.models[1].as_ref().unwrap().model,
            "strong-review"
        );
    }
}
