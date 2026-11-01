use crate::application::{Definition, ROLES, Task};
use serde_json::{Value, json};

pub struct Reply {
    pub data: Value,
    pub text: String,
}
impl Reply {
    pub fn task(task: &Task) -> Self {
        Self {
            data: task_value(task),
            text: task_text(task),
        }
    }
    pub fn list(tasks: &[Task]) -> Self {
        Self {
            data: json!({"tasks": tasks.iter().map(summary).collect::<Vec<_>>(), "count": tasks.len()}),
            text: if tasks.is_empty() {
                "No tasks.\n".into()
            } else {
                tasks
                    .iter()
                    .map(|t| {
                        format!(
                            "{}\tv{}\t{}\t{}\n",
                            t.id,
                            t.version,
                            t.state().label(),
                            safe(&t.definition().definition.title)
                        )
                    })
                    .collect()
            },
        }
    }
}
pub fn safe(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub fn definition(d: &Definition) -> Value {
    let models: serde_json::Map<String, Value> = ROLES
        .iter()
        .zip(&d.models)
        .map(|(role, model)| ((*role).into(), json!(model)))
        .collect();
    json!({"title":d.title, "goal":d.goal, "kind":d.kind, "criteria":d.criteria, "models":models, "attempt_limit":d.attempt_limit, "timeout_ms":d.timeout_ms, "stage_timeout_ms":d.stage_timeout_ms})
}
pub fn summary(task: &Task) -> Value {
    let selected = task.definition();
    let pending = task.revisions.iter().rev().find(|r| {
        r.lifecycle == "draft" && (selected.lifecycle != "ready" || r.number > selected.number)
    });
    let state = task.state();
    json!({"id":task.id,"version":task.version,"state":state,"actions":state.actions().iter().map(|a| a.label()).collect::<Vec<_>>(),"lifecycle":task.lifecycle,"status":task.status,"order_key":task.order_key,
        "title":selected.definition.title,"kind":selected.definition.kind,"selected_revision":selected.number,"pending_revision":pending.map(|r| r.number), "attempts_used":task.attempts_used,
        "origin_task_id":task.origin_task_id,"origin_attempt_id":task.origin_attempt_id,"follow_ups":task.follow_ups.iter().map(|(id,title)| json!({"id":id,"title":title})).collect::<Vec<_>>()})
}
pub fn task_value(task: &Task) -> Value {
    let mut value = summary(task);
    let object = value.as_object_mut().expect("summary object");
    object.insert(
        "definition".into(),
        definition(&task.definition().definition),
    );
    object.insert("revisions".into(), json!(task.revisions.iter().map(|r| json!({"number":r.number,"lifecycle":r.lifecycle,"actor":r.actor,"definition":definition(&r.definition)})).collect::<Vec<_>>()));
    object.insert("attempts".into(), json!(task.attempts));
    object.insert("resolution_reason".into(), json!(task.resolution_reason));
    object.insert("resolved_by".into(), json!(task.resolved_by));
    object.insert("lifecycle_events".into(), json!(task.lifecycle_events));
    object.insert("output_limit_per_attempt".into(), json!(2000));
    value
}
fn task_text(task: &Task) -> String {
    let revision = task.definition();
    let d = &revision.definition;
    let state = task.state();
    let mut text = format!(
        "#{} v{} · {} · {}\n{}\nGoal: {}\nRevision: {} ({})\nActions: {}\n",
        task.id,
        task.version,
        state.label(),
        d.kind.as_str(),
        safe(&d.title),
        safe(&d.goal),
        revision.number,
        revision.lifecycle,
        state
            .actions()
            .iter()
            .map(|a| a.label())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for c in &d.criteria {
        text.push_str(&format!("Check: {}\n", safe(c)));
    }
    if let (Some(task_id), Some(attempt_id)) = (task.origin_task_id, task.origin_attempt_id) {
        text.push_str(&format!(
            "Follow-up from task #{task_id}, attempt #{attempt_id}\n"
        ));
    }
    for (id, title) in &task.follow_ups {
        text.push_str(&format!("Follow-up #{id}: {}\n", safe(title)));
    }
    for (role, model) in ROLES.iter().zip(&d.models) {
        text.push_str(&format!(
            "{role}: {}\n",
            model.as_ref().map_or("unconfigured".into(), |m| format!(
                "{}/{} · effort {}",
                safe(&m.provider),
                safe(&m.model),
                safe(m.effort.as_deref().unwrap_or("none"))
            ))
        ));
    }
    text.push_str(&format!(
        "Attempt limit: {} · task limit: {} · stage limit: {}\n",
        limit(d.attempt_limit),
        crate::application::limits::label(d.timeout_ms),
        crate::application::limits::label(d.stage_timeout_ms)
    ));
    for r in &task.revisions {
        text.push_str(&format!(
            "Revision {}: {} · {} · {}\n",
            r.number,
            r.lifecycle,
            safe(r.actor.as_deref().unwrap_or("undecided")),
            safe(&r.definition.title)
        ));
    }
    for (number, a) in (1..).zip(&task.attempts) {
        // The task's own count, then the id that stashes and commits name.
        text.push_str(&format!(
            "Attempt {number} · #{}: {} · revision {} · {}\n",
            a.id,
            a.state,
            a.revision,
            safe(a.outcome.as_deref().unwrap_or("in progress"))
        ));
        if a.output_pruned {
            text.push_str("  PRUNED: stored output was removed by retention.\n");
        }
        if let Some(report) = &a.report {
            text.push_str(&format!("  Supervisor outcome: {}\n", safe(report)));
        }
        for artifact in &a.artifacts {
            text.push_str(&format!(
                "  Artifact {}: {} · {} · {} bytes · sha256 {} · tree {}{}\n",
                artifact.id,
                safe(&artifact.name),
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
                safe(step.check_name.as_deref().unwrap_or("unknown"))
            ));
        }
        if let Some(claim) = &a.worker_claim {
            text.push_str(&format!("  Worker claim (unverified): {}\n", safe(claim)));
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
                safe(step.report.as_deref().unwrap_or("No report recorded"))
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
            text.push_str(&format!("  {}\n", safe(line)));
        }
    }
    if let Some(reason) = &task.resolution_reason {
        text.push_str(&format!(
            "Resolution: {} · {}\n",
            safe(task.resolved_by.as_deref().unwrap_or("unknown")),
            safe(reason)
        ));
    }
    for entry in &task.lifecycle_events {
        text.push_str(&format!(
            "{}: {} · {} · {} · {}\n",
            if entry.action == "closed" {
                "Resolution"
            } else {
                "Reopen"
            },
            entry.outcome.as_deref().unwrap_or("pending"),
            safe(&entry.actor),
            safe(&entry.reason),
            entry.occurred_at_ms
        ));
    }
    text
}
fn limit(n: Option<i64>) -> String {
    n.map_or("unlimited".into(), |n| n.to_string())
}
