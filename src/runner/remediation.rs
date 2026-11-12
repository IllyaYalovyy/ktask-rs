use super::Workspace;
use crate::{
    application::{Definition, Service, VersionedTask, require},
    runner::agent::ModelOverrides,
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
    Ok(())
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
        "Current task changed while its failure was being judged",
    )?;
    require(
        matches!(task.status.as_str(), "failed" | "ready" | "cancelled"),
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
    use crate::application::{Kind, ModelConfig};

    #[test]
    fn revised_models_reject_empty_effort_and_allow_judge_reuse() {
        let model = |name: &str, effort: &str| ModelConfig {
            provider: "codex".into(),
            model: name.into(),
            effort: Some(effort.into()),
        };
        let mut definition = Definition {
            title: "Task".into(),
            goal: "Goal".into(),
            kind: Kind::Ai,
            category: None,
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
        apply_role_models(&mut definition, &changes).unwrap();
        assert_eq!(definition.models[1], Some(model("execution", "low")));
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
            category: None,
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
