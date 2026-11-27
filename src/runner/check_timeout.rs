//! Diagnose a timed-out check before the supervisor can run it again.
use super::{
    Context, Outcome,
    backoff::Notice,
    checks::{Transcript, archive_transcript},
    diagnosis::{self, Diagnosed, Inquiry, Judgment},
    failure_summary,
};
use crate::{
    application::{
        Service,
        check_timeout::{CheckTimeout, TimeoutDecision},
        execution::Run,
        progress::{Progress, REPAIR_ROUNDS},
        prompt::Turn,
        stop::CHECK_TIMEOUT_STOP,
    },
    storage::Result,
};

/// None continues the active attempt from the now-persisted workflow state.
pub(super) fn resolve(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    progress: &mut Progress,
    timeout: &CheckTimeout,
) -> Result<Option<Outcome>> {
    if timeout.decision.as_deref() == Some("escalate") {
        return Ok(Some(stopped(
            timeout,
            timeout.reason.as_deref().unwrap_or_default(),
            &timeout.report,
        )));
    }

    let report = match archive_timeout(service, run, ctx, timeout) {
        Ok(report) => report,
        Err(error) => {
            if let Some(outcome) = ctx.deadline_outcome(run) {
                return Ok(Some(outcome));
            }
            return Ok(Some(Outcome::DeliveryBlocked(format!(
                "Cannot prepare the timed-out check's transcript: {error}; captured output retained"
            ))));
        }
    };
    if let Some(outcome) = ctx.deadline_outcome(run) {
        return Ok(Some(outcome));
    }

    let unchanged = timeout.decision.as_deref() == Some("reconsider");
    let repair_available = !unchanged && progress.round < REPAIR_ROUNDS;
    let max_extension = if service
        .check_extension_for(run, timeout.check_id)?
        .is_some()
    {
        None
    } else {
        extension(ctx, run, timeout)
    };
    let context = format!(
        "{}\n{}\n{}\n{}\nRecorded owner answers:\n{}",
        progress.context,
        report,
        if unchanged {
            "The previous timeout repair left the check's source tree, command, tool environment and effective timeout unchanged. A worker cannot retry this repair."
        } else if !repair_available {
            "All ten repair rounds have been used."
        } else {
            "A focused repair worker is available if the evidence identifies a concrete cause."
        },
        max_extension.map_or_else(
            || "No additional check time is available in this attempt.".into(),
            |limit| format!("The supervisor can raise the effective timeout to at most {limit} ms once for this check in this attempt."),
        ),
        service.answers(run)?,
    );
    let judgment = match diagnosis::diagnose(
        service,
        run,
        ctx,
        &Inquiry {
            turn: Turn::CheckTimeout,
            context: &context,
            names: ("read-only-check-timeout", "Diagnosing check timeout"),
            notice: Notice {
                failed: "Check timeout diagnosis failed",
                retained: "timed-out check and evidence retained",
            },
            subject: "Check timeout",
            report: &report,
            repair_available,
            extension_available: max_extension.is_some(),
        },
    )? {
        Diagnosed::Judgment(judgment) => judgment,
        Diagnosed::Stop(outcome) => return Ok(Some(outcome)),
    };

    let extension =
        max_extension.and_then(|cap| extension(ctx, run, timeout).map(|ms| ms.min(cap)));
    let judgment = match judgment {
        Judgment::Extend { .. } if extension.is_none() => Judgment::Escalate {
            reason: "A larger check timeout is unavailable within this attempt's limits.".into(),
        },
        Judgment::Repair { .. } if !repair_available => Judgment::Escalate {
            reason: "No further repair can be applied to this timeout in the current workflow."
                .into(),
        },
        judgment => judgment,
    };
    let reason = judgment.reason();
    let (action, extension_ms, stop) = match &judgment {
        Judgment::Repair { plan, .. } => {
            diagnosis::apply_repair(
                service,
                run,
                progress,
                reason,
                &format!("{reason}\nAuthorized timeout repair plan:\n{plan}\n{report}"),
            )?;
            ("repair", None, None)
        }
        Judgment::Extend { .. } => {
            let limit = extension.expect("extension availability checked");
            let report = format!(
                "Check {} timeout extended from {} ms to {limit} ms for this attempt. {reason}",
                timeout.name, timeout.timeout_ms,
            );
            service.note(run, &report)?;
            progress.why = report.lines().next().unwrap_or_default().to_owned();
            ("extend", Some(limit), None)
        }
        Judgment::Escalate { .. } => ("escalate", None, Some(stopped(timeout, reason, &report))),
    };
    service.save_decision(
        run,
        progress,
        &TimeoutDecision {
            step: timeout.step,
            action,
            reason,
            extension_ms,
        },
    )?;
    Ok(stop)
}

fn archive_timeout(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    timeout: &CheckTimeout,
) -> Result<String> {
    let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, timeout.step);
    let path = match service.retained_timeout_transcript(run, &ctx.paths.state, &relative)? {
        Some(path) => path,
        None => archive_transcript(
            service,
            run,
            ctx,
            Transcript {
                relative: &relative,
                name: &timeout.name,
                tree: &timeout.tree,
                output: timeout.output,
            },
        )?,
    };
    let bytes = std::fs::read(&path)?;
    let transcript = String::from_utf8_lossy(&bytes);
    let summary = failure_summary::summarize(transcript.as_ref());
    let partial = if summary.is_empty() {
        "(no output captured)".to_owned()
    } else {
        summary.join("\n")
    };
    let report = format!(
        "Check {} timed out after {} ms.\nCommand: {}\nPartial output:\n{partial}\nFull log: {}",
        timeout.name,
        timeout.timeout_ms,
        timeout.command,
        path.display(),
    );
    if report != timeout.report {
        service.complete_check_timeout_report(run, timeout, &report)?;
        service.note(run, &report)?;
    }
    Ok(report)
}

fn extension(ctx: &Context<'_>, run: &Run, timeout: &CheckTimeout) -> Option<i64> {
    let doubled = timeout.timeout_ms.saturating_mul(2);
    let cap = ctx.remaining_task_ms(run).map_or(doubled, |remaining| {
        doubled.min(remaining.saturating_sub(1))
    });
    (cap > timeout.timeout_ms).then_some(cap)
}

fn stopped(timeout: &CheckTimeout, reason: &str, report: &str) -> Outcome {
    let (headline, detail) = reason.split_once('\n').unwrap_or((reason, ""));
    Outcome::Failed(
        "check_timeout",
        format!(
            "{CHECK_TIMEOUT_STOP}: {} ({} ms). {headline}\n{detail}\n{report}",
            timeout.name, timeout.timeout_ms,
        ),
    )
}
