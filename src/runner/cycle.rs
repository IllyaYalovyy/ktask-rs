//! Findings are normal work within one durable attempt, not failed attempts.
use super::{Context, Outcome, ai, checks, judge, judge_attempt};
use crate::{
    application::{
        Service,
        execution::{Check, Run},
        progress::{Progress, REPAIR_ROUNDS},
    },
    storage::{Error, Result},
};
use std::sync::atomic::Ordering;

pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[Check],
    repair: bool,
) -> Result<Outcome> {
    let mut progress = match service.progress(run)? {
        Some(saved) => saved,
        None => {
            let prior = service
                .task(run.task)?
                .and_then(|t| t.attempts.iter().rev().find_map(|a| a.outcome.clone()))
                .unwrap_or_default();
            let previous = service.prior_progress(run)?;
            let round = previous.as_ref().map_or(0, |p| p.round);
            let mut progress = Progress {
                round,
                phase: if repair { "judge" } else { "implement" }.into(),
                context: format!(
                    "Prior failure context (scope only; not proof of current success): {prior}"
                ),
                evidence: String::new(),
                verified_tree: String::new(),
            };
            if let Some(previous) = previous.filter(|p| p.round > 0) {
                progress.phase = match previous.phase.as_str() {
                    "replanned" => "fix",
                    "judge" | "fix" => "judge",
                    _ => "check",
                }
                .into();
                progress.context.push_str(&format!(
                    "\nRetained current findings and authorized scope:\n{}",
                    previous.context
                ));
                progress.evidence = format!(
                    "{}\nRechecking retained work after supervisor handoff; previous passes are not current proof.",
                    progress.context
                );
            }
            progress
        }
    };
    service.save_progress(run, &progress)?;
    loop {
        if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
            return Ok(Outcome::Paused);
        }
        if matches!(
            progress.phase.as_str(),
            "review" | "validate" | "decide" | "complete"
        ) && ctx
            .workspace
            .fingerprint(&ctx.paths.cache.join("workers"))?
            != progress.verified_tree
        {
            progress.phase = "check".into();
            progress.evidence = format!(
                "{}\nWorktree changed since verification; regenerate all check, review and validation evidence.",
                progress.context
            );
            progress.verified_tree.clear();
            service.output(run, "system", "Worktree changed since verification · rerunning checks, review and validation; repair count retained")?;
            service.save_progress(run, &progress)?;
        }
        let answers = service.answers(run)?;
        let outcome = match progress.phase.as_str() {
            "implement" | "fix" => ai(
                service,
                run,
                ctx,
                if progress.phase == "fix" {
                    "remediation"
                } else {
                    "execution"
                },
                &format!("{}\nRecorded answers:\n{answers}", progress.context),
            )?,
            "judge" if progress.round == 0 => judge(service, run, ctx)?,
            "judge" => judge_attempt(service, run, ctx, run.attempt)?,
            "check" => {
                let before = ctx
                    .workspace
                    .fingerprint(&ctx.paths.cache.join("workers"))?;
                let outcome = checks::run(service, run, ctx, checks)?;
                if matches!(outcome, Outcome::Passed(_)) {
                    let after = ctx
                        .workspace
                        .fingerprint(&ctx.paths.cache.join("workers"))?;
                    if before != after {
                        Outcome::Changes { report: "Supervisor checks changed the worktree; their results do not verify a stable source state. Make verification read-only with respect to project source.".into(), critical: true }
                    } else {
                        progress.verified_tree = after;
                        outcome
                    }
                } else {
                    outcome
                }
            }
            "review" => ai(service, run, ctx, "review", &progress.evidence)?,
            "validate" => ai(service, run, ctx, "validation", &progress.evidence)?,
            "decide" => ai(
                service,
                run,
                ctx,
                "validation",
                &format!(
                    "{}\nFinal decision after {REPAIR_ROUNDS} repair rounds. Independently revalidate each remaining finding against current code and recorded checks. Critical means a concrete correctness, safety, data-loss or mandatory acceptance defect. Minor polish may remain only if every mandatory criterion and supervisor check passes. Record minor findings explicitly. Do not request another repair or relax the goal. Recorded owner answers:\n{answers}",
                    progress.evidence,
                ),
            )?,
            "complete" => {
                return Ok(Outcome::Passed(format!(
                    "Checks, independent review and validation passed after {} repair rounds; any accepted minor findings remain in the review records",
                    progress.round,
                )));
            }
            _ => return Err(Error::Conflict("Unknown workflow phase".into())),
        };
        match outcome {
            Outcome::Passed(report) => match progress.phase.as_str() {
                "implement" | "fix" => {
                    progress.evidence = format!(
                        "{}\nRecorded answers:\n{answers}\nCurrent attempt executor assessment (not independent evidence):\n{report}",
                        progress.context
                    );
                    progress.verified_tree.clear();
                    progress.phase = "check".into();
                }
                "judge" => {
                    progress
                        .context
                        .push_str(&format!("\nRepair plan from the failure judge:\n{report}"));
                    progress.phase = "fix".into();
                }
                "check" => {
                    progress.evidence.push_str(&report);
                    progress.phase = "review".into();
                }
                "review" => {
                    progress.evidence.push_str(&format!("\nreview: {report}"));
                    progress.phase = "validate".into();
                }
                "validate" | "decide" => {
                    progress
                        .evidence
                        .push_str(&format!("\nvalidation: {report}"));
                    progress.phase = "complete".into();
                }
                _ => unreachable!("phase checked above"),
            },
            Outcome::Changes { report, critical } => {
                if progress.round == REPAIR_ROUNDS {
                    if !critical
                        && matches!(progress.phase.as_str(), "review" | "validate" | "decide")
                    {
                        service.output(run, "system", "Repair limit reached; only minor findings remain. Retaining them for independent acceptance.")?;
                        progress.evidence.push_str(&format!("\n{}: {report}\nOnly minor findings remain; all mandatory criteria passed.", progress.phase));
                        progress.phase = if progress.phase == "review" {
                            "validate"
                        } else {
                            "complete"
                        }
                        .into();
                    } else if progress.phase == "decide" {
                        return Ok(escalation(&report));
                    } else if matches!(progress.phase.as_str(), "review" | "validate") {
                        progress
                            .evidence
                            .push_str(&format!("\nLatest critical findings:\n{report}"));
                        progress.phase = "decide".into();
                    } else {
                        return Ok(escalation(&report));
                    }
                } else {
                    request_fixes(service, run, &mut progress, &report)?;
                }
            }
            Outcome::Failed("check", report) => {
                if progress.round == REPAIR_ROUNDS {
                    return Ok(escalation(&format!(
                        "Required supervisor check still fails: {report}"
                    )));
                }
                request_fixes(service, run, &mut progress, &report)?;
            }
            other => return Ok(other),
        }
        service.save_progress(run, &progress)?;
    }
}

fn request_fixes(
    service: &Service,
    run: &Run,
    progress: &mut Progress,
    report: &str,
) -> Result<()> {
    progress.round += 1;
    progress.context = format!(
        "Normal review/fix cycle, repair round {}/{REPAIR_ROUNDS}. Preserve useful work and the full goal.\nPrior failure context (scope only; not proof of current success):\n{report}",
        progress.round,
    );
    progress.phase = "judge".into();
    service.output(
        run,
        "system",
        &format!(
            "Changes requested · planning fix round {}/{REPAIR_ROUNDS} · task remains in progress",
            progress.round,
        ),
    )
}

fn escalation(report: &str) -> Outcome {
    let readable = serde_json::from_str::<serde_json::Value>(report)
        .ok()
        .map(|value| {
            let assessment = &value["assessment"];
            let mut text = assessment["summary"].as_str().unwrap_or(report).to_owned();
            if let Some(findings) = assessment["findings"].as_array() {
                for finding in findings {
                    text.push_str(&format!(
                        "\n- {}",
                        finding["description"]
                            .as_str()
                            .unwrap_or("Recorded finding")
                    ));
                }
            }
            if let Some(criteria) = assessment["criteria"].as_array() {
                for criterion in criteria.iter().filter(|c| c["passed"] == false) {
                    text.push_str(&format!(
                        "\nCriterion {}: {}",
                        criterion["ordinal"],
                        criterion["evidence"]
                            .as_str()
                            .unwrap_or("Missing required proof")
                    ));
                }
            }
            text
        })
        .unwrap_or_else(|| report.to_owned());
    Outcome::Question(format!(
        "Critical findings remain after {REPAIR_ROUNDS} repair rounds. Work and evidence are preserved; no additional repair worker will start automatically. Which scope change or missing product decision should guide the next work?\n{readable}"
    ))
}
