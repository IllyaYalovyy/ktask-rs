//! Diagnose a storage-limit breach before the queue pauses for the owner.
//!
//! The producer is already stopped and its scratch deleted. A read-only remediation turn
//! reads what filled the budget, as it reads a timed-out check, and decides to repair the
//! cause, extend that one limit once or escalate; only an escalation pauses.
use super::{
    Context, Outcome,
    backoff::Notice,
    diagnosis::{self, Diagnosed, Inquiry, Judgment},
    test_summary,
};
use crate::{
    application::{
        Service,
        execution::Run,
        progress::{Progress, REPAIR_ROUNDS},
        prompt::Turn,
        resource_breach::{BreachDecision, Breached},
    },
    storage::Result,
};

/// Output the judge reads inline; archiving it could itself breach the artifact budget.
const TRANSCRIPT_BYTES: usize = 16 * 1024;

/// None continues the active attempt from the now-persisted workflow state.
pub(super) fn resolve(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    progress: &mut Progress,
    breach: &Breached,
) -> Result<Option<Outcome>> {
    if service.breach_repeats(run, breach)? {
        let reason = format!(
            "{} was breached again for the same cause ({}) after an earlier repair or extension.",
            breach.limit, breach.cause
        );
        return decide(service, run, progress, breach, &Decision::Escalate(reason));
    }
    // The owner may have raised the limit while the breach waited for its diagnosis.
    let raised = service.raised_limit(run, breach.limit)?;
    if raised >= breach.measured {
        let reason = format!(
            "The {} limit is now {}, above the {} measured; no diagnosis needed.",
            breach.limit,
            breach.limit.show(raised),
            breach.limit.show(breach.measured)
        );
        return decide(service, run, progress, breach, &Decision::Moot(reason));
    }
    let repair_available = progress.round < REPAIR_ROUNDS;
    let extension = service.extension_for(run, breach.limit, &ctx.scratch_root())?;
    let context = context(service, run, progress, breach, repair_available, extension)?;
    let judgment = match diagnosis::diagnose(
        service,
        run,
        ctx,
        &Inquiry {
            turn: Turn::ResourceBreach,
            context: &context,
            names: (
                "read-only-resource-breach",
                "Diagnosing resource limit breach",
            ),
            notice: Notice {
                failed: "Resource limit diagnosis failed",
                retained: "work and evidence retained",
            },
            subject: "Resource limit",
            report: &breach.report,
            repair_available,
            extension_available: extension.is_some(),
        },
    )? {
        Diagnosed::Judgment(judgment) => judgment,
        // The owner reads the breach itself beside the reason the diagnosis could not run.
        Diagnosed::Stop(Outcome::DiagnosisUnavailable(subject, category, message)) => {
            return Ok(Some(Outcome::DiagnosisUnavailable(
                subject,
                category,
                format!("{message}\n{}", breach.report),
            )));
        }
        Diagnosed::Stop(outcome) => return Ok(Some(outcome)),
    };
    let decision = match judgment {
        Judgment::Extend { .. } if extension.is_none() => Decision::Escalate(
            "A larger limit is unavailable: this attempt used its extensions, the owner's ceiling is reached or the storage reserve would be entered."
                .into(),
        ),
        Judgment::Repair { .. } if !repair_available => Decision::Escalate(
            "No further repair round is available in this attempt.".into(),
        ),
        Judgment::Extend { reason } => Decision::Extend(
            reason.trim().to_owned(),
            extension.expect("extension availability checked"),
        ),
        Judgment::Repair { reason, plan } => {
            Decision::Repair(reason.trim().to_owned(), plan.trim().to_owned())
        }
        Judgment::Escalate { reason } => Decision::Escalate(reason.trim().to_owned()),
    };
    decide(service, run, progress, breach, &decision)
}

/// What the judge is told: the breach, what filled the budget and what it may decide.
fn context(
    service: &Service,
    run: &Run,
    progress: &Progress,
    breach: &Breached,
    repair_available: bool,
    extension: Option<u64>,
) -> Result<String> {
    Ok(format!(
        "{}\nResource limit breach: {} was {} against a limit of {}. The producer was stopped at once and its scratch deleted. Cause key: {}.\n{}\n{}\n{}\n{}\nRecorded owner answers:\n{}",
        progress.context,
        breach.limit,
        breach.limit.show(breach.measured),
        breach.limit.show(breach.configured),
        breach.cause,
        if breach.summary.is_empty() {
            "No summary of what filled the budget could be taken."
        } else {
            &breach.summary
        },
        transcript(service, run, breach)?,
        if repair_available {
            "A repair round is available: the executing agent resumes its session with your plan."
        } else {
            "All ten repair rounds have been used."
        },
        extension.map_or_else(
            || "No larger limit is available for this attempt.".to_owned(),
            |limit| format!(
                "The supervisor can raise this one limit to {} once for this attempt, within the owner's ceiling and the storage reserve.",
                breach.limit.show(limit)
            ),
        ),
        service.answers(run)?,
    ))
}

/// The end of the stopped work's output, which shows what it was doing when the limit broke.
fn transcript(service: &Service, run: &Run, breach: &Breached) -> Result<String> {
    let tail = service.output_tail(run, breach.output_end, TRANSCRIPT_BYTES)?;
    let plain: Vec<_> = tail.lines().map(test_summary::plain).collect();
    Ok(format!(
        "Transcript of the stopped work (last {TRANSCRIPT_BYTES} bytes at most):\n{}",
        if plain.is_empty() {
            "(no output captured)".to_owned()
        } else {
            plain.join("\n")
        }
    ))
}

/// What the attempt does next, with the reason the judgment gave.
enum Decision {
    Repair(String, String),
    Extend(String, u64),
    Escalate(String),
    /// The limit was raised by the owner before the breach was diagnosed.
    Moot(String),
}

/// Records the decision with the workflow state it leads to. Some outcome pauses the
/// attempt; None continues it.
fn decide(
    service: &mut Service,
    run: &Run,
    progress: &mut Progress,
    breach: &Breached,
    decision: &Decision,
) -> Result<Option<Outcome>> {
    let (action, reason, extended_to, stop) = match decision {
        Decision::Repair(reason, plan) => {
            diagnosis::apply_repair(
                service,
                run,
                progress,
                reason,
                &format!(
                    "{reason}\nAuthorized repair plan for the {} limit breach:\n{plan}\n{}\n{}",
                    breach.limit, breach.summary, breach.report
                ),
            )?;
            ("repair", reason, None, None)
        }
        Decision::Extend(reason, limit) => {
            let message = format!(
                "{} raised from {} to {} for this attempt. {reason}",
                breach.limit,
                breach.limit.show(breach.configured),
                breach.limit.show(*limit)
            );
            service.output(run, "system", &message)?;
            progress.why = message.lines().next().unwrap_or_default().to_owned();
            ("extend", reason, Some(*limit), None)
        }
        Decision::Moot(reason) => ("escalate", reason, None, None),
        Decision::Escalate(reason) => (
            "escalate",
            reason,
            None,
            Some(Outcome::DeliveryBlocked(format!(
                "{}\nDiagnosis: {reason}",
                breach.report
            ))),
        ),
    };
    service.save_decision(
        run,
        progress,
        &BreachDecision {
            id: breach.id,
            action,
            reason,
            extended_to,
        },
    )?;
    Ok(stop)
}
