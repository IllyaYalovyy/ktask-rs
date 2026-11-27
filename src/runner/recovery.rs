//! Consult the configured remediator while the attempt still owns its work.
use super::{Context, Outcome, attempt, backoff, cycle, diagnosis, turn};
use crate::{
    application::{Service, execution::Run, stop::unresolvable},
    storage::{Error, Result},
};

/// A delivery failure retains its attempt and checkpoint. Ask the remediation
/// model before yielding to the owner, then reconcile Git again after a repair.
pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    opening: cycle::Opening,
    mut pinned: Result<()>,
) -> Result<Outcome> {
    let mut previous = String::new();
    let mut failures = 0_u32;
    loop {
        let outcome = match attempt::drive(service, run, ctx, opening, pinned) {
            Ok(Outcome::Replanned(judgment)) if service.automatic_replans(run.task)? >= 10 => {
                Outcome::Failed(
                    "protocol",
                    format!(
                        "Automatic recovery exhausted after 10 judge replans: {}",
                        super::judge::describe(&judgment)
                    ),
                )
            }
            Ok(outcome) => outcome,
            Err(error) if super::resource_limit(&error).is_some() => return Err(error),
            Err(error) => Outcome::Failed("environment", error.to_string()),
        };
        let reason = match &outcome {
            Outcome::DeliveryFailure(reason) | Outcome::WorkerQuestion(reason) => reason.clone(),
            Outcome::Failed("transport", _) if service.rate_waits(run)? > 0 => return Ok(outcome),
            Outcome::Failed(category, message) if would_stop(service, run, category, message)? => {
                message.clone()
            }
            _ => return Ok(outcome),
        };
        if ctx.interrupted(service, run)? {
            return Ok(Outcome::Paused);
        }
        if let Some(limit) = ctx.deadline_outcome(run) {
            return Ok(limit);
        }
        service.note(run, &format!("Supervisor needs remediation: {reason}"))?;
        let packet = crate::scratch::Scratch::new(&ctx.paths.state.join("review-packets"))?;
        let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
        let evidence =
            service.review_bundle(run.attempt, &ctx.paths.state, packet.path(), &tree, &reason)?;
        let answers = service.answers(run)?;
        let attempts_used = service
            .task(run.task)?
            .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?
            .attempts_used;
        let repair_available = run
            .definition
            .attempt_limit
            .is_none_or(|limit| attempts_used < limit);
        let mut instructions = format!(
            "Task: {}. Goal: {}. Owner answers: {answers}\n{evidence}\nExecution would stop: {reason}\nPrevious recovery: {previous}\nCan the supervisor reasonably resolve this issue and resume? Choose repair with a concrete workspace repair plan, extend to retry if the external condition can have cleared, or escalate only if no feasible authorized recovery remains. Explain why. Preserve work, repository identity, configured branch, hooks and verification. Workspace repair available within the attempt budget: {repair_available}. The worker cannot change delivery policy or publish. Never repeat an ineffective plan without new evidence.",
            run.definition.title, run.definition.goal
        );
        let result = backoff::bounded(
            service,
            run,
            ctx,
            backoff::Notice {
                failed: "Supervisor diagnosis unavailable",
                retained: "checkpoint retained",
            },
            turn::operational_failure,
            |service, ctx| {
                let decision = match diagnosis::judge_once(
                    service,
                    run,
                    ctx,
                    &instructions,
                    &diagnosis::schema(repair_available, true),
                    ("supervisor-diagnosis", "Diagnosing supervisor stop"),
                ) {
                    Ok(decision) => decision,
                    Err(error) if super::resource_limit(&error).is_some() => return Err(error),
                    Err(error) => Outcome::Failed("environment", error.to_string()),
                };
                if let Outcome::Diagnosis(diagnosis::Judgment::Repair { plan, .. }) = &decision {
                    if !repair_available {
                        return Ok(Outcome::Failed(
                            "protocol",
                            "Repair exceeds the configured attempt budget".into(),
                        ));
                    }
                    if service.tried_recovery(
                        run,
                        &recovery_key(&tree, &ctx.tools.fingerprint, &reason, plan),
                    )? {
                        let why = "This plan already failed on the same evidence. A different plan or an escalation is required.";
                        instructions.push_str(&format!("\nRejected plan: {plan}\n{why}"));
                        return Ok(Outcome::Failed("protocol", why.into()));
                    }
                }
                Ok(decision)
            },
        )?;
        match result {
            Outcome::Diagnosis(diagnosis::Judgment::Escalate { reason: why }) => {
                service.note(run, &format!("Remediator cannot resolve this issue: {why}"))?;
                return Ok(match outcome {
                    Outcome::Failed(category, _) => {
                        Outcome::Failed(category, format!("{reason}\nRemediator: {why}"))
                    }
                    Outcome::WorkerQuestion(question) => Outcome::Question(question),
                    _ => Outcome::DeliveryBlocked(format!("{reason}\nRemediator: {why}")),
                });
            }
            Outcome::Diagnosis(diagnosis::Judgment::Repair { plan, reason: why }) => {
                previous = format!("Repair: {why}\n{plan}");
                service.note(run, &previous)?;
                let prompt = format!(
                    "Resolve this execution blocker: {reason}\n{previous}\nPreserve the destination, hooks and all existing work. The supervisor owns Git delivery and rechecks the tree."
                );
                match turn::ai(service, run, ctx, "repair", &prompt)? {
                    Outcome::Passed(report) => previous.push_str(&format!("\nWorker: {report}")),
                    Outcome::Failed(_, message) | Outcome::WorkerQuestion(message) => {
                        previous.push_str(&format!("\nWorker could not finish: {message}"));
                    }
                    other => return Ok(other),
                }
                service.record_tried_recovery(
                    run,
                    &recovery_key(&tree, &ctx.tools.fingerprint, &reason, &plan),
                )?;
            }
            Outcome::Diagnosis(diagnosis::Judgment::Extend { reason: why }) => {
                previous = format!("Retry: {why}");
                service.note(run, &previous)?;
                if let Some(outcome) =
                    backoff::operational_backoff(service, run, ctx, failures.min(6))?
                {
                    return Ok(outcome);
                }
            }
            Outcome::Failed(category, message) => {
                let report =
                    format!("{reason}\nSupervisor diagnosis unavailable ({category}): {message}");
                return Ok(match outcome {
                    Outcome::Failed(original_category, _) => {
                        Outcome::Failed(original_category, report)
                    }
                    _ => Outcome::DeliveryBlocked(report),
                });
            }
            other => return Ok(other),
        }
        failures = failures.saturating_add(1);
        pinned = attempt::pin_delivery(service, ctx.workspace, run);
    }
}

/// Evaluate the same stop policy used after finishing, while the live attempt
/// can still run its remediator. Already adjudicated timeouts retain their verdict.
fn would_stop(service: &mut Service, run: &Run, category: &str, message: &str) -> Result<bool> {
    if category == "check_timeout" {
        return Ok(false);
    }
    let mut task = service
        .task(run.task)?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    let attempt = task
        .attempts
        .last_mut()
        .filter(|a| a.id == run.attempt)
        .ok_or_else(|| Error::Conflict("Current attempt disappeared".into()))?;
    attempt.state = "failed".into();
    attempt.failure_category = Some(category.into());
    attempt.outcome = Some(message.into());
    if matches!(
        category,
        "check" | "review" | "validation" | "agent" | "unknown"
    ) {
        task.attempts_used += 1;
    }
    Ok(unresolvable(&task).is_some())
}

fn recovery_key(tree: &str, environment: &str, reason: &str, plan: &str) -> String {
    use sha2::{Digest, Sha256};
    let evidence = serde_json::json!([tree, environment, reason.trim(), plan.trim()]);
    format!("{:x}", Sha256::digest(evidence.to_string()))
}
