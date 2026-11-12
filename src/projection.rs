//! Read projections of stored tasks for the screens; nothing here writes back.
use crate::{
    application as domain,
    presentation::{self as model, Kind, RevisionState, Status},
};
use std::time::{Duration, Instant};

/// An attempt's own state, as its row shows it: an ended attempt that did not succeed or
/// fail was stopped.
fn attempt_state(attempt: &domain::Attempt) -> Status {
    match attempt.state.as_str() {
        "running" | "paused" | "waiting" | "failed" | "succeeded" => {
            Status::of(&attempt.state, attempt.wait_reason.as_deref())
        }
        _ => Status::Cancelled,
    }
}
fn project_revision(revision: &domain::Revision) -> Result<model::Revision, String> {
    let d = &revision.definition;
    Ok(model::Revision {
        number: revision.number as usize,
        state: match revision.lifecycle.as_str() {
            "ready" => RevisionState::Ready,
            "draft" => RevisionState::Draft,
            "rejected" => RevisionState::Rejected,
            "retired" => RevisionState::Retired,
            _ => return Err("Unsupported revision lifecycle".into()),
        },
        title: d.title.clone(),
        goal: d.goal.clone(),
        check: d.criteria.join("\n"),
        kind: match d.kind {
            domain::Kind::Ai => Kind::Ai,
            domain::Kind::Human => Kind::Human,
            domain::Kind::Docs => Kind::Docs,
        },
        settings: model::Settings {
            models: std::array::from_fn(|i| model::Model {
                name: d.models[i]
                    .as_ref()
                    .map_or("unconfigured".into(), domain::ModelConfig::identifier),
                effort: d.models[i].as_ref().and_then(|m| m.effort.clone()),
            }),
            attempt_limit: d.attempt_limit.unwrap_or(0) as usize,
            timeout_ms: d.timeout_ms.unwrap_or(0) as u64,
            stage_timeout_ms: d.stage_timeout_ms.unwrap_or(0) as u64,
        },
        actor: revision
            .actor
            .clone()
            .unwrap_or_else(|| "Needs approval".into()),
        repaired: false,
    })
}
/// An execution step after the failure judge's plan in the same attempt is a repair.
fn step_label(steps: &[domain::Step], index: usize) -> String {
    if let (Some(lens), "review") = (&steps[index].lens, steps[index].role.as_str()) {
        return format!("Review ({lens} lens)");
    }
    match steps[index].role.as_str() {
        "execution"
            if steps[..index]
                .iter()
                .any(|s| matches!(s.role.as_str(), "remediation" | "execution")) =>
        {
            "Repair"
        }
        "execution" => "Execution",
        "check" if !steps[..index].iter().any(|s| s.role == "execution") => "Starting checks",
        "check" => "Checks",
        "review" => "Review",
        "validation" => "Validation",
        _ => "Remediation",
    }
    .to_owned()
}
/// Keep the worker's claim distinct from the supervisor's outcome and recorded steps.
fn report_text(attempt: &domain::Attempt) -> String {
    let supervisor_report = attempt
        .report
        .clone()
        .filter(|report| attempt.question.as_deref() != Some(report.trim()))
        .unwrap_or_default();
    let mut text = String::new();
    if let Some(claim) = &attempt.worker_claim {
        text.push_str(&format!("Worker claim (unverified):\n{claim}"));
    }
    let latest_check = attempt.steps.iter().rposition(|step| step.role == "check");
    let last = attempt.steps.len();
    for (i, step) in attempt.steps.iter().enumerate() {
        if step.role == "execution" && step.state == "succeeded" {
            continue;
        }
        if step.role == "check" {
            if Some(i) != latest_check {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "Supervisor check {} · {}",
                step.check_name.as_deref().unwrap_or("unknown"),
                step.state
            ));
            if let Some(counts) = step.counts {
                text.push_str(&format!(
                    "\nCheck {}: {} passed, {} failed, {} ignored",
                    step.check_name.as_deref().unwrap_or("unknown"),
                    counts.passed,
                    counts.failed,
                    counts.ignored
                ));
            } else {
                text.push_str(&format!(
                    "\nCheck {}: test counts unavailable",
                    step.check_name.as_deref().unwrap_or("unknown")
                ));
            }
            if let Some(report) = step
                .report
                .as_deref()
                .filter(|report| !report.trim().is_empty())
            {
                text.push_str(&format!("\n{}", report.lines().next().unwrap_or_default()));
            }
            continue;
        }
        let finding = step.report.as_deref().unwrap_or_default().trim();
        if finding.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let state = if attempt.state == "waiting" && i + 1 == last {
            "waiting"
        } else {
            if step.state == "changes_requested" {
                "fixes requested"
            } else {
                step.state.as_str()
            }
        };
        let model = step
            .model
            .as_deref()
            .map(|model| {
                format!(
                    " · {}/{}",
                    step.provider.as_deref().unwrap_or("unknown"),
                    model
                )
            })
            .unwrap_or_default();
        text.push_str(&format!(
            "{} · {state}{model} verdict: {finding}",
            step_label(&attempt.steps, i)
        ));
    }
    if attempt.state == "succeeded"
        && attempt
            .steps
            .iter()
            .any(|step| step.role == "validation" && step.state == "succeeded")
    {
        text.push_str("\n\nAccepted: validation passed");
    }
    if !supervisor_report.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        if supervisor_report.starts_with("Workflow · ")
            || supervisor_report.starts_with("Git delivery · ")
        {
            text.push_str(&supervisor_report);
        } else {
            text.push_str(&format!("Supervisor outcome: {supervisor_report}"));
        }
    }
    text
}
pub(crate) fn project_task(task: &domain::Task) -> Result<model::Task, String> {
    let revisions = task
        .revisions
        .iter()
        .map(project_revision)
        .collect::<Result<Vec<_>, _>>()?;
    let attempts = task
        .attempts
        .iter()
        .map(|a| {
            let revision = revisions
                .iter()
                .find(|r| r.number == a.revision as usize)
                .ok_or("Attempt references missing revision")?
                .clone();
            let state = attempt_state(a);
            Ok(model::Attempt {
                id: Some(a.id),
                revision,
                session: a.session.map(|id| id as usize),
                state,
                resume_at: a.resume_at_ms.map(|at| {
                    Instant::now()
                        + Duration::from_millis(at.saturating_sub(domain::now_ms()).max(0) as u64)
                }),
                resume_at_ms: a.resume_at_ms,
                logs: a.logs.clone(),
                outcome: a.outcome.clone().unwrap_or_else(|| match a.state.as_str() {
                    "waiting" => "Waiting".to_owned(),
                    state => state.to_owned(),
                }),
                supervisor_status: a.report.clone().unwrap_or_default(),
                report: report_text(a),
                question: a.question.clone(),
                steps: a
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        // A waiting attempt resumes its last step; it has not failed.
                        let state = if a.state == "waiting" && i + 1 == a.steps.len() {
                            "waiting".to_owned()
                        } else {
                            s.state.clone()
                        };
                        (step_label(&a.steps, i), state)
                    })
                    .collect(),
                step: 0,
                cost_cents: a.cost_microusd.unwrap_or(0) as usize / 10_000,
                elapsed_ms: a.active_ms as u64,
                waiting_ms: a.waiting_ms as u64,
                usage: a.usage.label(),
                usage_tokens: a.usage.token_line(),
                usage_tokens_short: a.usage.short_token_line(),
                usage_cost: format!(
                    "{}{}",
                    if a.usage.cost_microusd.is_some() {
                        "Cost "
                    } else {
                        ""
                    },
                    a.usage.cost_label()
                ),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let stored_status = task.state();
    Ok(model::Task {
        id: task.id as usize,
        origin: task
            .origin_task_id
            .zip(task.origin_attempt_id)
            .map(|(task, attempt)| (task as usize, attempt as usize)),
        follow_ups: task
            .follow_ups
            .iter()
            .map(|(id, title)| (*id as usize, title.clone()))
            .collect(),
        stored_status: Some(stored_status),
        stored_budget: Some(task.attempts_used as usize),
        revisions,
        attempts,
        resolution: None,
        resolution_reason: task
            .resolution_reason
            .as_ref()
            .map_or(String::new(), |reason| {
                format!(
                    "{} · {reason}",
                    task.resolved_by.as_deref().unwrap_or("unknown actor")
                )
            }),
        answer: String::new(),
        needs_question: false,
        needs_rate_limit: false,
        repairs: None,
        closed_at_ms: task.closed_at_ms,
        unresolved: crate::application::stop::unresolvable(task),
    })
}
