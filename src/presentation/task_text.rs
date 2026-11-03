//! The one task text. `ktask show` prints it in full; task details print the overview and
//! page through each attempt's evidence in their own sections.
use super::role_setting;
use crate::application::{
    ROLES, Revision, Task, limits, usage_report::RoleUsage, usage_report::UsageTotals,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Definitions, revisions, attempt summaries and resolution.
    Overview,
    /// The overview plus each attempt's report, artifacts, checks, verdicts and output.
    Full,
}

/// Usage across all of a task's attempts.
pub fn usage(task: &Task) -> UsageTotals {
    let mut total = UsageTotals::default();
    for attempt in &task.attempts {
        total.merge(&attempt.usage);
    }
    total
}

/// "Revision 2 · Title", then its approval, goal, criteria, one line per role and its limits.
pub fn revision(revision: &Revision, escape: fn(&str) -> String) -> String {
    let definition = &revision.definition;
    let models = ROLES
        .iter()
        .zip(&definition.models)
        .map(|(role, model)| escape(&role_setting(role, model.as_ref())))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Revision {} · {}\nApproval: {} · {}\nGoal: {}\nAcceptance: {}\n{models}\nAttempt limit: {} · Task limit: {} · Stage limit: {}",
        revision.number,
        escape(&definition.title),
        revision.lifecycle,
        escape(revision.actor.as_deref().unwrap_or("not recorded")),
        escape(&definition.goal),
        definition
            .criteria
            .iter()
            .enumerate()
            .map(|(i, criterion)| format!("{}: {}", i + 1, escape(criterion)))
            .collect::<Vec<_>>()
            .join("; "),
        definition
            .attempt_limit
            .map_or("unlimited".into(), |limit| limit.to_string()),
        limits::label(definition.timeout_ms),
        limits::label(definition.stage_timeout_ms),
    )
}

/// One role's share of an attempt: "execution: codex/gpt / high · 3 calls … · provider time 2m 01s".
pub fn role_usage(role: &RoleUsage) -> String {
    format!(
        "{} · {} · provider time {}",
        super::role_model(&role.role, Some((&role.model, role.effort.as_deref()))),
        role.totals.label(),
        limits::elapsed(role.provider_elapsed_ms)
    )
}

pub fn task(task: &Task, detail: Detail, escape: fn(&str) -> String) -> String {
    let selected = task.definition();
    let state = task.state();
    let total = usage(task);
    let mut text = format!(
        "#{} v{} · {} · {}\n{}\nActions: {}\nCurrent {}\nTask usage: {}{}\n",
        task.id,
        task.version,
        state.label(),
        selected.definition.kind.as_str(),
        escape(&selected.definition.title),
        state
            .actions()
            .iter()
            .map(|a| a.label())
            .collect::<Vec<_>>()
            .join(", "),
        revision(selected, escape),
        if total.cost_microusd.is_some() {
            "cost "
        } else {
            ""
        },
        total.cost_label()
    );
    if let (Some(origin), Some(attempt)) = (task.origin_task_id, task.origin_attempt_id) {
        text.push_str(&format!(
            "Follow-up from task #{origin}, attempt #{attempt}\n"
        ));
    }
    for (id, title) in &task.follow_ups {
        text.push_str(&format!("Follow-up #{id}: {}\n", escape(title)));
    }
    for other in &task.revisions {
        let shown = revision(other, escape);
        if other.number < selected.number {
            text.push_str(&format!("Earlier {shown}\n"));
        } else if other.number > selected.number && other.lifecycle == "draft" {
            text.push_str(&format!(
                "Newer draft revision {} (needs approval)\n{shown}\n",
                other.number
            ));
        } else if other.number > selected.number {
            text.push_str(&format!("Newer {shown}\n"));
        }
    }
    for (number, attempt) in (1..).zip(&task.attempts) {
        // The task's own count, then the id that stashes and commits name.
        text.push_str(&format!(
            "Attempt {number} · #{}: {} · revision {} · active {} · waiting {}\n  Outcome: {}\n  Usage: {}\n",
            attempt.id,
            attempt.state,
            attempt.revision,
            limits::elapsed(attempt.active_ms),
            limits::elapsed(attempt.waiting_ms),
            escape(attempt.outcome.as_deref().unwrap_or("in progress")),
            attempt.usage.label()
        ));
        for role in &attempt.role_usage {
            text.push_str(&format!("  {}\n", escape(&role_usage(role))));
        }
        if detail == Detail::Full {
            evidence(&mut text, attempt, escape);
        }
    }
    if let Some(reason) = &task.resolution_reason {
        text.push_str(&format!(
            "Resolution: {} · {}\n",
            escape(task.resolved_by.as_deref().unwrap_or("unknown actor")),
            escape(reason)
        ));
    }
    for entry in &task.lifecycle_events {
        text.push_str(&format!(
            "{}: {} · {} · {} · {}\n",
            if entry.action == "closed" {
                "Resolution"
            } else {
                "Reopened"
            },
            entry.outcome.as_deref().unwrap_or("pending"),
            escape(&entry.actor),
            escape(&entry.reason),
            entry.occurred_at_ms
        ));
    }
    text
}

/// An attempt's recorded evidence, indented under its summary.
fn evidence(text: &mut String, a: &crate::application::Attempt, escape: fn(&str) -> String) {
    if a.output_pruning_records > 0 {
        text.push_str(&format!(
            "  PARTIALLY PRUNED: {} stored output bytes were removed by retention; remaining records are shown.\n",
            a.output_pruning_bytes
        ));
    } else if a.output_pruned {
        text.push_str("  PRUNED: stored output was removed by retention.\n");
    }
    if let Some(report) = &a.report {
        text.push_str(&format!("  Supervisor outcome: {}\n", escape(report)));
    }
    for artifact in &a.artifacts {
        text.push_str(&format!(
            "  Artifact {}: {} · {} · {} bytes · sha256 {} · tree {}{}\n",
            artifact.id,
            escape(&artifact.name),
            artifact.media_type,
            artifact.size_bytes,
            artifact.checksum_sha256.as_deref().unwrap_or("unrecorded"),
            artifact.source_tree.as_deref().unwrap_or("unrecorded"),
            if artifact.pruned { " · PRUNED" } else { "" }
        ));
    }
    if a.steps.iter().any(|step| step.role == "check") {
        text.push_str("  Supervisor check results:\n");
    }
    for step in a.steps.iter().filter(|step| step.role == "check") {
        let counts = step.counts.map_or_else(
            || "test counts unavailable".into(),
            |counts| {
                format!(
                    "{} passed, {} failed, {} ignored",
                    counts.passed, counts.failed, counts.ignored
                )
            },
        );
        text.push_str(&format!(
            "  Check {}: {counts}\n",
            escape(step.check_name.as_deref().unwrap_or("unknown"))
        ));
    }
    if let Some(claim) = &a.worker_claim {
        text.push_str(&format!("  Worker claim (unverified): {}\n", escape(claim)));
    }
    for step in a
        .steps
        .iter()
        .filter(|step| matches!(step.role.as_str(), "review" | "validation"))
    {
        text.push_str(&format!(
            "  {} verdict · {}/{} · {}: {}\n",
            step.role,
            step.provider.as_deref().unwrap_or("unknown"),
            step.model.as_deref().unwrap_or("unknown"),
            step.state,
            escape(step.report.as_deref().unwrap_or("No report recorded"))
        ));
    }
    if a.state == "succeeded"
        && a.steps
            .iter()
            .any(|step| step.role == "validation" && step.state == "succeeded")
    {
        text.push_str("  Accepted: validation passed\n");
    }
    for line in &a.logs {
        text.push_str(&format!("  {}\n", escape(line)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Attempt, Definition, Kind, ModelConfig};

    fn revision_with(number: i64, lifecycle: &str, title: &str) -> Revision {
        Revision {
            number,
            lifecycle: lifecycle.into(),
            actor: Some("Human: owner".into()),
            definition: Definition {
                title: title.into(),
                goal: format!("{title} goal"),
                kind: Kind::Ai,
                criteria: vec!["First".into(), "Second".into()],
                models: std::array::from_fn(|i| {
                    (i != 3).then(|| ModelConfig {
                        provider: "codex".into(),
                        model: format!("m{i}"),
                        effort: (i == 0).then(|| "high".into()),
                    })
                }),
                attempt_limit: Some(3),
                timeout_ms: Some(123_000),
                stage_timeout_ms: None,
            },
        }
    }

    fn sample() -> Task {
        Task {
            id: 7,
            version: 4,
            lifecycle: "open".into(),
            status: "failed".into(),
            order_key: "a".into(),
            origin_task_id: Some(2),
            origin_attempt_id: Some(5),
            follow_ups: vec![(9, "Later\twork".into())],
            resolution_reason: None,
            resolved_by: None,
            lifecycle_events: Vec::new(),
            revisions: vec![
                revision_with(1, "retired", "First take"),
                revision_with(2, "ready", "Current take"),
                revision_with(3, "draft", "Proposed take"),
            ],
            attempts: vec![Attempt {
                id: 11,
                revision: 2,
                state: "failed".into(),
                outcome: Some("Checks failed".into()),
                failure_category: Some("check".into()),
                active_ms: 61_000,
                waiting_ms: 0,
                usage: UsageTotals::default(),
                role_usage: Vec::new(),
                wait_reason: None,
                resume_at_ms: None,
                session: None,
                logs: vec!["log line".into()],
                output_pruned: true,
                output_pruning_bytes: 0,
                output_pruning_records: 0,
                cost_microusd: None,
                report: Some("Supervisor said".into()),
                worker_claim: None,
                question: None,
                steps: Vec::new(),
                artifacts: Vec::new(),
            }],
            attempts_used: 1,
            counted_from: 0,
            closed_at_ms: None,
        }
    }

    #[test]
    fn revision_lists_approval_criteria_roles_and_limits() {
        assert_eq!(
            revision(&revision_with(2, "ready", "Take"), str::to_owned),
            "Revision 2 · Take\nApproval: ready · Human: owner\nGoal: Take goal\nAcceptance: 1: First; 2: Second\nexecution: codex/m0 / high\nreview: codex/m1 / none\nvalidation: codex/m2 / none\nremediation: unconfigured\nAttempt limit: 3 · Task limit: 2 min 3 s · Stage limit: Off"
        );
    }

    #[test]
    fn overview_places_each_revision_once_and_full_adds_attempt_evidence() {
        let task = sample();
        let overview = super::task(&task, Detail::Overview, str::to_owned);
        assert!(overview.starts_with("#7 v4 · Failed · ai\nCurrent take\nActions: "));
        assert!(overview.contains("\nCurrent Revision 2 · Current take\n"));
        assert_eq!(
            overview.matches("Earlier Revision 1 · First take").count(),
            1
        );
        assert_eq!(
            overview
                .matches("Newer draft revision 3 (needs approval)\nRevision 3 · Proposed take")
                .count(),
            1
        );
        assert!(!overview.contains("Newer Revision"));
        assert!(
            overview.contains("Follow-up from task #2, attempt #5\nFollow-up #9: Later\twork\n")
        );
        assert!(overview.contains(
            "Attempt 1 · #11: failed · revision 2 · active 1m 01s · waiting 0s\n  Outcome: Checks failed\n  Usage: 0 calls"
        ));
        assert!(!overview.contains("PRUNED") && !overview.contains("log line"));
        let full = super::task(&task, Detail::Full, |s| s.escape_default().to_string());
        assert!(full.contains("Follow-up #9: Later\\twork\n"));
        assert!(full.contains("  PRUNED: stored output was removed by retention.\n"));
        assert!(full.contains("  Supervisor outcome: Supervisor said\n  log line\n"));
    }
}
