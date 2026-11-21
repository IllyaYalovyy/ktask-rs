//! Diagnose a timed-out check before the supervisor can run it again.
use super::{
    Context, Outcome,
    backoff::{self, Notice},
    checks::{Transcript, archive_transcript},
    failure_summary,
    judge::read_only_turn,
    turn::operational_failure,
};
use crate::{
    application::{
        Service,
        check_timeout::{CheckTimeout, TimeoutDecision},
        execution::Run,
        progress::{Phase, Progress, REPAIR_ROUNDS},
        prompt::{self, Turn},
        stop::{CHECK_TIMEOUT_DIAGNOSIS_LIMIT, CHECK_TIMEOUT_STOP},
    },
    storage::Result,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Judgment {
    Repair { reason: String, plan: String },
    Extend { reason: String },
    Escalate { reason: String },
}

impl Judgment {
    fn reason(&self) -> &str {
        match self {
            Self::Repair { reason, .. } | Self::Extend { reason } | Self::Escalate { reason } => {
                reason.trim()
            }
        }
    }
}

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
    let packet = crate::scratch::Scratch::new(&ctx.paths.state.join("review-packets"))?;
    let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    let evidence = service.review_bundle(
        run.attempt,
        &ctx.paths.state,
        packet.path(),
        &tree,
        &context,
    )?;
    let evidence = format!(
        "{context}\n{evidence}\nResolved project tools: {}. Environment fingerprint: {}.\nRead/search with '{}' evidence FILE --offset 0 --limit 4096 [--search TEXT]; follow next_offset.",
        ctx.tools.description,
        ctx.tools.fingerprint,
        ctx.paths.executable.display()
    );
    let category = run.effective_category();
    let prompt = prompt::compose(
        Turn::CheckTimeout,
        &category,
        &prompt::Task {
            title: &run.definition.title,
            goal: &run.definition.goal,
            criteria: &run.definition.criteria,
        },
        &evidence,
        None,
    )?
    .text();
    let schema = schema(repair_available, max_extension.is_some());
    let outcome = backoff::bounded(
        service,
        run,
        ctx,
        Notice {
            failed: "Check timeout diagnosis failed",
            retained: "timed-out check and evidence retained",
        },
        operational_failure,
        |service, ctx| judge_once(service, run, ctx, &prompt, &schema),
    )?;
    if let Some(outcome) = ctx.deadline_outcome(run) {
        return Ok(Some(outcome));
    }
    let judgment = match outcome {
        Outcome::CheckTimeoutDecision(judgment) => judgment,
        Outcome::Failed("timeout", message) => {
            return Ok(Some(Outcome::Failed(
                "timeout",
                format!("{CHECK_TIMEOUT_DIAGNOSIS_LIMIT}: {message}\n{report}"),
            )));
        }
        Outcome::Failed("configuration" | "agent", _) => return Ok(Some(outcome)),
        Outcome::Failed(category, message) => {
            return Ok(Some(Outcome::CheckTimeoutDiagnosisUnavailable(
                category, message,
            )));
        }
        other => return Ok(Some(other)),
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
            let baseline = progress.phase == Phase::Baseline;
            super::cycle::fix_checks(
                service,
                run,
                progress,
                &format!("{reason}\nAuthorized timeout repair plan:\n{plan}\n{report}"),
            )?;
            if baseline {
                progress.context.push('\n');
                progress
                    .context
                    .push_str(crate::application::progress::BASELINE_FAILURE_NOTE);
            }
            progress.why = reason.lines().next().unwrap_or_default().to_owned();
            ("repair", None, None)
        }
        Judgment::Extend { .. } => {
            let limit = extension.expect("extension availability checked");
            let report = format!(
                "Check {} timeout extended from {} ms to {limit} ms for this attempt. {reason}",
                timeout.name, timeout.timeout_ms,
            );
            service.output(run, "system", &report)?;
            progress.why = report.lines().next().unwrap_or_default().to_owned();
            ("extend", Some(limit), None)
        }
        Judgment::Escalate { .. } => ("escalate", None, Some(stopped(timeout, reason, &report))),
    };
    service.save_timeout_decision(
        run,
        progress,
        TimeoutDecision {
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
        service.output(run, "system", &report)?;
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

fn schema(repair_available: bool, extension_available: bool) -> Value {
    let choice = |action| {
        json!({
            "type":"object","additionalProperties":false,"required":["action","reason"],
            "properties":{"action":{"type":"string","enum":[action]},"reason":{"type":"string"}}
        })
    };
    let mut alternatives = vec![choice("escalate")];
    if extension_available {
        alternatives.push(choice("extend"));
    }
    if repair_available {
        alternatives.push(json!({
            "type":"object","additionalProperties":false,"required":["action","reason","plan"],
            "properties":{"action":{"type":"string","enum":["repair"]},"reason":{"type":"string"},"plan":{"type":"string"}}
        }));
    }
    // A lone alternative is already an object schema.
    let judgment = if alternatives.len() == 1 {
        alternatives.remove(0)
    } else {
        json!({"anyOf":alternatives})
    };
    json!({"type":"object","additionalProperties":false,"required":["judgment"],"properties":{"judgment":judgment}})
}

fn judge_once(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    prompt: &str,
    schema: &Value,
) -> Result<Outcome> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        judgment: Judgment,
    }

    let (step, stream, outcome) = read_only_turn(
        service,
        run,
        ctx,
        ("read-only-check-timeout", "Diagnosing check timeout"),
        (prompt.to_owned(), schema.clone()),
    )?;
    if let Some(outcome) = outcome {
        return Ok(outcome);
    }
    if stream.completed
        && !stream.failed
        && let Ok(response) =
            serde_json::from_str::<Response>(stream.last_message.as_deref().unwrap_or(""))
        && !response.judgment.reason().is_empty()
        && !matches!(&response.judgment, Judgment::Repair { plan, .. } if plan.trim().is_empty())
    {
        service.finish_step(&step, true, response.judgment.reason())?;
        return Ok(Outcome::CheckTimeoutDecision(response.judgment));
    }
    let message = "Check timeout judge did not return a complete decision";
    service.finish_step(&step, false, message)?;
    Ok(Outcome::Failed("protocol", message.into()))
}
