//! Findings are normal work within one durable attempt, not failed attempts.
use super::{
    Context, Outcome, checks,
    judge::{judge, judge_attempt},
    turn::ai,
};
use crate::{
    application::{
        Service,
        delivery::Stage,
        execution::{Check, Run},
        progress::{Phase, Progress, REPAIR_ROUNDS},
        stop::{BASELINE_RED, CHECKS_STILL_FAIL},
    },
    storage::{Error, Result},
};

/// How a new attempt begins. Resume never opens a new attempt; it continues the saved one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Opening {
    /// Run from Pending: nothing carries over. The project checks run first when enabled.
    Fresh { verify: bool },
    /// Retry after a failure: the kept work is checked first, then reviewed as usual.
    Retry,
    /// The supervisor's own repair after a failure or replan: the failure judge goes first.
    Repair,
}

pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[Check],
    opening: Opening,
) -> Result<Outcome> {
    let mut progress = match service.progress(run)? {
        Some(saved) => saved,
        None => opening_progress(service, run, ctx, opening)?,
    };
    service.save_progress(run, &progress)?;
    loop {
        if ctx.interrupted(service, run)? {
            return Ok(Outcome::Paused);
        }
        if matches!(
            progress.phase,
            Phase::Review | Phase::Validate | Phase::Decide | Phase::Complete
        ) {
            let observed = ctx.workspace.fingerprint(&ctx.scratch_root())?;
            if observed != progress.verified_tree {
                let paths = if progress.verified_tree.is_empty() {
                    "unavailable (no verified tree recorded)".into()
                } else {
                    ctx.workspace
                        .tree_difference(&progress.verified_tree, &observed)?
                };
                let change = format!(
                    "Worktree changed since verification · expected tree {}; observed tree {observed}; differing paths: {paths} · rerunning checks, review and validation; repair count retained",
                    progress.verified_tree
                );
                progress.phase = Phase::Check;
                progress.evidence = format!("{}\n{change}", progress.context);
                progress.verified_tree.clear();
                service.output(run, "system", &change)?;
                service.save_progress(run, &progress)?;
            }
        }
        let answers = service.answers(run)?;
        let attempts_used = service
            .task(run.task)?
            .ok_or_else(|| Error::Conflict("Current task disappeared during execution".into()))?
            .attempts_used;
        let outcome = match progress.phase {
            Phase::Fix if run.definition.attempt_limit.is_some_and(|limit| attempts_used >= limit) => Outcome::Failed("protocol", "Coding attempt limit exhausted after the read-only failure judge; work and evidence retained. c Models raises the limit; r Retry restarts it.".into()),
            Phase::Implement | Phase::Fix => ai(
                service,
                run,
                ctx,
                if progress.phase == Phase::Fix {
                    "repair"
                } else {
                    "execution"
                },
                &format!("{}\nRecorded answers:\n{answers}{}", progress.context,
                    service.artifact_manifest(run.attempt, &ctx.paths.state)?),
            )?,
            Phase::Integrate => super::delivery::continue_integration(service, run, ctx)?,
            Phase::Baseline => {
                let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
                checks::run(service, run, ctx, checks, &tree)?
            }
            Phase::Judge if progress.round == 0 => judge(service, run, ctx)?,
            Phase::Judge => judge_attempt(service, run, ctx, run.attempt)?,
            Phase::Check if !run.definition.kind.runs_checks() => {
                let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
                let scope = review_scope(service, run, ctx, &tree)?;
                progress.verified_tree = tree;
                Outcome::Passed(scope)
            }
            Phase::Check => {
                let workers = ctx.scratch_root();
                let before = ctx.workspace.fingerprint(&workers)?;
                match checks::run(service, run, ctx, checks, &before)? {
                    Outcome::Passed(evidence) => {
                        let after = ctx.workspace.fingerprint(&workers)?;
                        if before != after {
                            let paths = ctx.workspace.tree_difference(&before, &after)?;
                            Outcome::Changes { report: format!("Worktree changed during supervisor checks; their results do not verify a stable source state. Expected tree {before}; observed tree {after}; differing paths: {paths}. Make verification read-only with respect to project source."), critical: true }
                        } else {
                            let scope = review_scope(service, run, ctx, &after)?;
                            progress.verified_tree = after;
                            Outcome::Passed(format!("{evidence}{scope}"))
                        }
                    }
                    other => other,
                }
            }
            Phase::Review => ai(service, run, ctx, "review", &progress.evidence)?,
            Phase::Validate => ai(service, run, ctx, "validation", &progress.evidence)?,
            Phase::Decide => ai(
                service,
                run,
                ctx,
                "validation",
                &format!(
                    "{}\nFinal decision after {REPAIR_ROUNDS} repair rounds. Independently revalidate each remaining finding against current code and recorded checks. Critical means a concrete correctness, safety, data-loss or mandatory acceptance defect. Minor polish may remain only if every mandatory criterion and supervisor check passes. Record minor findings explicitly. Do not request another repair or relax the goal. Recorded owner answers:\n{answers}",
                    progress.evidence,
                ),
            )?,
            Phase::Complete => {
                let minor = progress.evidence.matches("\nMinor: ").count();
                let gates = if run.definition.kind.runs_checks() {
                    "Checks, independent review"
                } else {
                    "Independent review"
                };
                return Ok(Outcome::Passed(format!(
                    "{gates} and validation passed after {} repair rounds{}",
                    progress.round,
                    match minor {
                        0 => String::new(),
                        n => format!("; {n} minor finding(s) recorded with the reviews"),
                    }
                )));
            }
            // A replanned attempt has ended; its successor opens with the judge.
            Phase::Replanned => return Err(Error::Conflict("Unknown workflow phase".into())),
        };
        if progress.phase == Phase::Baseline {
            match outcome {
                Outcome::Passed(_) => {
                    service.output(
                        run,
                        "system",
                        "Starting state verified: every check passes before work begins",
                    )?;
                    progress.phase = Phase::Implement;
                    service.save_progress(run, &progress)?;
                    continue;
                }
                Outcome::Failed("check", report) | Outcome::Changes { report, .. } => {
                    return Ok(Outcome::Failed(
                        "environment",
                        format!("{BASELINE_RED}\n{report}"),
                    ));
                }
                other => return Ok(other),
            }
        }
        match outcome {
            Outcome::Passed(report) => match progress.phase {
                Phase::Implement | Phase::Fix => {
                    progress.evidence = format!(
                        "{}\nRecorded answers:\n{answers}\nCurrent attempt executor assessment (not independent evidence):\n{report}",
                        progress.context
                    );
                    progress.verified_tree.clear();
                    progress.phase = if service
                        .attempt_delivery(run)
                        .is_ok_and(|d| d.stage == Stage::Integrating)
                    {
                        Phase::Integrate
                    } else {
                        Phase::Check
                    };
                }
                Phase::Integrate => {
                    progress.phase = Phase::Check;
                    progress.verified_tree.clear();
                }
                Phase::Judge => {
                    let (reason, plan) = report.split_once('\n').unwrap_or((&report, ""));
                    progress.why = reason.to_owned();
                    progress
                        .context
                        .push_str(&format!("\nRepair plan from the failure judge:\n{plan}"));
                    progress.phase = Phase::Fix;
                }
                Phase::Check => {
                    progress.evidence.push_str(&report);
                    progress.why.clear();
                    progress.phase = Phase::Review;
                }
                Phase::Review => {
                    progress.evidence.push_str(&format!("\nreview: {report}"));
                    progress.phase = Phase::Validate;
                }
                Phase::Validate | Phase::Decide => {
                    progress
                        .evidence
                        .push_str(&format!("\nvalidation: {report}"));
                    progress.phase = Phase::Complete;
                }
                Phase::Baseline | Phase::Replanned | Phase::Complete => {
                    unreachable!("phase checked above")
                }
            },
            Outcome::Changes { report, critical } => {
                if progress.round == REPAIR_ROUNDS {
                    if !critical
                        && matches!(
                            progress.phase,
                            Phase::Review | Phase::Validate | Phase::Decide
                        )
                    {
                        service.output(run, "system", "Repair limit reached; only minor findings remain. Retaining them for independent acceptance.")?;
                        progress.evidence.push_str(&format!("\n{}: {report}\nOnly minor findings remain; all mandatory criteria passed.", progress.phase));
                        progress.phase = if progress.phase == Phase::Review {
                            Phase::Validate
                        } else {
                            Phase::Complete
                        };
                    } else if progress.phase == Phase::Decide {
                        return Ok(unresolved(&report));
                    } else if matches!(progress.phase, Phase::Review | Phase::Validate) {
                        progress
                            .evidence
                            .push_str(&format!("\nLatest critical findings:\n{report}"));
                        progress.phase = Phase::Decide;
                    } else {
                        return Ok(unresolved(&report));
                    }
                } else {
                    request_fixes(service, run, &mut progress, &report)?;
                }
            }
            // A failing check needs no judgment: its summary goes straight to the worker.
            // After ten rounds the attempt fails and the failure judge decides what next.
            Outcome::Failed("check", report) => {
                if progress.round == REPAIR_ROUNDS {
                    return Ok(Outcome::Failed(
                        "check",
                        format!(
                            "{CHECKS_STILL_FAIL} after {REPAIR_ROUNDS} repair rounds\n{report}"
                        ),
                    ));
                }
                fix_checks(service, run, &mut progress, &report)?;
            }
            other => return Ok(other),
        }
        service.save_progress(run, &progress)?;
    }
}

/// What the reviewers examine: the attempt's change from its baseline to the verified tree.
fn review_scope(service: &Service, run: &Run, ctx: &Context<'_>, tree: &str) -> Result<String> {
    let original_base = service
        .attempt_base(run.task)?
        .ok_or_else(|| Error::Conflict("Attempt has no recorded baseline".into()))?;
    let delivery = service.attempt_delivery(run).ok();
    let base = delivery
        .as_ref()
        .and_then(|d| {
            (d.stage != Stage::Work)
                .then_some(d.fetched_base.as_deref())
                .flatten()
        })
        .unwrap_or(&original_base);
    Ok(ctx.workspace.review_scope(base, tree)?.describe())
}

pub(super) fn request_fixes(
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
    progress.phase = Phase::Judge;
    progress.why = finding_headline(report);
    service.output(
        run,
        "system",
        &format!(
            "Changes requested · planning fix round {}/{REPAIR_ROUNDS} · {}",
            progress.round, progress.why,
        ),
    )
}

/// The reviewer's own summary of a recorded assessment, or the report's first line.
fn finding_headline(report: &str) -> String {
    serde_json::from_str::<serde_json::Value>(report)
        .ok()
        .and_then(|value| value["assessment"]["summary"].as_str().map(str::to_owned))
        .unwrap_or_else(|| report.to_owned())
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn fix_checks(service: &Service, run: &Run, progress: &mut Progress, report: &str) -> Result<()> {
    progress.round += 1;
    progress.context = format!(
        "Supervisor checks failed, repair round {}/{REPAIR_ROUNDS}. Make every check pass by fixing the cause; never skip, weaken or delete a test. Preserve useful work and the full goal.\nPrior failure context (scope only; not proof of current success):\n{report}",
        progress.round,
    );
    progress.phase = Phase::Fix;
    progress.why = report.lines().take(2).collect::<Vec<_>>().join(" · ");
    let headline = report.lines().next().unwrap_or(report);
    service.output(
        run,
        "system",
        &format!(
            "{headline} · worker fixing, round {}/{REPAIR_ROUNDS}",
            progress.round
        ),
    )
}

pub(super) fn unresolved(report: &str) -> Outcome {
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
    Outcome::Failed(
        "review",
        format!(
            "Critical findings remain after {REPAIR_ROUNDS} repair rounds. Work and evidence are preserved; no additional repair worker will start automatically. Inspect the findings, change the task or its models if needed, then retry.\n{readable}"
        ),
    )
}

fn opening_progress(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    opening: Opening,
) -> Result<Progress> {
    let fresh = |phase| Progress {
        round: 0,
        phase,
        context: String::new(),
        evidence: String::new(),
        verified_tree: String::new(),
        why: String::new(),
    };
    let task = service
        .task(run.task)?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    let prior = task
        .attempts
        .iter()
        .rev()
        .skip(1)
        .find_map(|a| a.outcome.clone())
        .unwrap_or_default();
    let mut progress = match opening {
        Opening::Fresh { verify } => fresh(if verify {
            Phase::Baseline
        } else {
            Phase::Implement
        }),
        Opening::Retry if ctx.workspace.clean()? => fresh(Phase::Implement),
        Opening::Retry => {
            let mut progress = fresh(Phase::Check);
            progress.context = format!(
                "Retry of the failed attempt, continuing from its kept work. It ended: {prior}"
            );
            if let Some(previous) = service.prior_progress(run)? {
                progress.context.push_str(&format!(
                    "\nIts findings and authorized scope:\n{}",
                    previous.context
                ));
            }
            progress.evidence = progress.context.clone();
            progress
        }
        Opening::Repair => {
            let previous = service.prior_progress(run)?;
            let mut progress = fresh(Phase::Judge);
            progress.round = previous.as_ref().map_or(0, |p| p.round);
            progress.context = format!(
                "Prior failure context (scope only; not proof of current success): {prior}"
            );
            if let Some(previous) = previous {
                progress.context.push_str(&format!(
                    "\nRetained current findings and authorized scope:\n{}",
                    previous.context
                ));
                progress.evidence = format!(
                    "{}\nRechecking retained work after supervisor handoff; previous passes need a matching verified tree and check definition before reuse.",
                    progress.context
                );
            }
            progress
        }
    };
    // Later rounds and judges see the previous attempt's archived evidence files.
    if let Some(prior) = task.attempts.iter().rev().find(|a| a.id != run.attempt) {
        progress
            .context
            .push_str(&service.artifact_manifest(prior.id, &ctx.paths.state)?);
    }
    Ok(progress)
}
