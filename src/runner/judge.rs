//! The failure judge: a read-only turn of the remediation model that decides whether the
//! next worker repairs, the task is rewritten, prerequisite work is inserted, or the owner
//! must decide. It never runs anything; its plan is applied immediately and nothing runs twice.
use super::{
    Context, Outcome, Workspace, agent,
    backoff::{self, Notice},
    provider::{self, Access, Provider},
    remediation,
    turn::{operational_failure, provider_invocation, provider_turn, role_model},
};
use crate::{
    application::{
        Actor, ActorKind, Definition, Service, VersionedTask,
        delivery::Stage,
        execution::{Run, Step},
        preferences::PlannerApproval,
        progress::REPAIR_ROUNDS,
    },
    instance::Paths,
    storage::{Error, Result},
};

/// Judges the task's latest failed attempt.
pub(super) fn judge(service: &mut Service, run: &Run, ctx: &mut Context<'_>) -> Result<Outcome> {
    let failed = service
        .task(run.task)?
        .and_then(|t| {
            t.attempts
                .iter()
                .rev()
                .find(|a| a.state == "failed")
                .map(|a| a.id)
        })
        .ok_or_else(|| Error::Conflict("No failed attempt to judge".into()))?;
    judge_attempt(service, run, ctx, failed)
}

/// Judges attempt `failed`, retried automatically after an operational provider failure.
pub(super) fn judge_attempt(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    failed: i64,
) -> Result<Outcome> {
    backoff::bounded(
        service,
        run,
        ctx,
        Notice {
            failed: "Failure judge failed",
            retained: "same attempt, evidence retained",
        },
        operational_failure,
        |service, ctx| judge_once(service, run, ctx, failed),
    )
}

fn judge_once(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    failed: i64,
) -> Result<Outcome> {
    let (evidence, judging) = evidence(service, run, ctx, failed)?;
    let packet = crate::scratch::Scratch::new(&ctx.paths.state.join("review-packets"))?;
    let tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    let evidence = service.review_bundle(
        failed,
        &ctx.paths.state,
        packet.path(),
        &tree,
        &evidence,
        false,
    )?;
    let evidence = format!(
        "{evidence}\nResolved project tools: {}. Environment fingerprint: {}.\nRead/search with '{}' evidence FILE --offset 0 --limit 4096 [--search TEXT]; follow next_offset.\n",
        ctx.tools.description,
        ctx.tools.fingerprint,
        ctx.paths.executable.display()
    );
    let (prompt, schema) = request(run, &evidence, judging)?;
    let (step, stream, outcome) = read_only_turn(
        service,
        run,
        ctx,
        ("read-only-judge", "Fix planning"),
        (prompt, schema),
    )?;
    if let Some(outcome) = outcome {
        return Ok(outcome);
    }
    if !stream.completed || stream.failed {
        let message = "Failure judge ended without a complete turn".to_owned();
        service.finish_step(&step, false, &message)?;
        return Ok(Outcome::Failed("protocol", message));
    }
    let judgment = match agent::parse_judgment(stream.last_message.as_deref().unwrap_or("")) {
        Ok(j) => j,
        Err(e) => {
            service.finish_step(&step, false, &e.to_string())?;
            return Ok(Outcome::Failed("protocol", e.to_string()));
        }
    };
    if judging == Judging::Round && matches!(judgment, agent::Judgment::Ask { .. }) {
        let message = "Fix planner must repair, revise or insert during the normal ten-round cycle; owner questions wait for the final critical decision";
        service.finish_step(&step, false, message)?;
        return Ok(Outcome::Failed("protocol", message.into()));
    }
    let (summary, outcome) = match judgment {
        // The reason's first line is the owner's one-line why for the round.
        agent::Judgment::Repair { plan, reason } => (
            format!("Repair: {reason}\nAuthorized repair plan:\n{plan}"),
            Outcome::Passed(format!(
                "{}\n{plan}",
                reason
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
            )),
        ),
        agent::Judgment::Ask { question, reason } => {
            service.finish_step(&step, false, &format!("Owner decision needed: {reason}"))?;
            return Ok(Outcome::Question(question));
        }
        other => (describe(&other), Outcome::Replanned(other)),
    };
    service.finish_step(&step, true, &summary)?;
    Ok(outcome)
}

/// Run a read-only remediation turn using the configured provider and protected tools.
pub(super) fn read_only_turn(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    (scope, label): (&str, &str),
    (prompt, schema): (String, serde_json::Value),
) -> Result<(Step, provider::Stream, Option<Outcome>)> {
    let model = role_model(run, "remediation").expect("preflight models");
    let provider = Provider::of(model)?;
    let step = service.start_ai_step(
        run,
        "remediation",
        model,
        &format!("{}#{scope}", provider.auth_scope()?),
        false,
    )?;
    service.output(
        run,
        "system",
        &format!("{label}: {}/{} · read-only", model.provider, model.model),
    )?;
    let launch = provider.launch(
        model,
        step.provider_session.as_deref(),
        Access::ReadOnly,
        false,
        &schema,
        prompt,
    );
    let mut invocation = provider_invocation(
        provider,
        launch,
        Access::ReadOnly,
        &ctx.workspace.root,
        ctx.paths,
        ctx.scratch_root(),
        ctx.workspace.worker_lock.clone(),
    )?;
    if let Some(sandbox) = invocation.sandbox.as_mut() {
        for path in &ctx.tools.installations {
            sandbox.protect(path)?;
        }
    }
    let mut stream = provider::Stream::new(provider);
    let outcome = provider_turn(
        service,
        run,
        &step,
        ctx,
        &invocation,
        &mut stream,
        "remediation",
    )?;
    Ok((step, stream, outcome))
}

/// Which decision the judge makes, and so which judgments it may return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Judging {
    /// A normal review/fix round of the live attempt: the owner is not asked yet.
    Round,
    /// Recovery from an actual failure before any repair round: all four judgments.
    Failure,
    /// Recovery after all ten check-repair rounds failed: all four judgments, and the
    /// task's repair count stays exhausted.
    ExhaustedChecks,
}

/// What the judge reads about attempt `failed`, and which decision it makes. Judging the
/// live attempt plans its next fix round; judging an earlier failed attempt recovers it.
fn evidence(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    failed: i64,
) -> Result<(String, Judging)> {
    let mut evidence = format!(
        "{}\nRecorded answers for this attempt:\n{}\n{}",
        service.attempt_evidence(failed, &ctx.paths.state)?,
        service.answers(run)?,
        service.failure_decisions(run.task)?,
    );
    let progress = service.progress(run)?;
    let round = progress.as_ref().map_or(0, |p| p.round);
    let judging = if failed == run.attempt {
        Judging::Round
    } else if round == REPAIR_ROUNDS {
        Judging::ExhaustedChecks
    } else {
        Judging::Failure
    };
    if let Some(progress) = progress {
        evidence.push_str(&format!(
            "\nCurrent workflow state: {}\n{}\n",
            progress.label(),
            progress.context
        ));
    }
    Ok((evidence, judging))
}

/// The judge's prompt and response schema for this decision.
fn request(run: &Run, evidence: &str, judging: Judging) -> Result<(String, serde_json::Value)> {
    let mut prompt = agent::judge_prompt(run, evidence)?;
    let mut schema = agent::judge_schema();
    restrict(&mut prompt, &mut schema, judging);
    Ok((prompt, schema))
}

/// During normal rounds the owner is not asked; after exhausted check repair only a product
/// decision is, and the repair count is neither reset nor extended.
fn restrict(prompt: &mut String, schema: &mut serde_json::Value, judging: Judging) {
    match judging {
        Judging::Round => {
            prompt.push_str("\nThis is a normal review/fix round within the same active task, not a failed task. Revalidate the current findings; preserve the goal and useful work. Plan fixes only for critical findings and drop minor ones. Cost to result matters most, then time: prefer repair while the worker is converging. You may revise the task or insert prerequisites only if needed, retaining the full goal and acceptance bar. The repair counter follows the original task across automatic revisions and prerequisite handoffs. Do not ask the owner during these rounds.");
            schema["properties"]["judgment"]["anyOf"]
                .as_array_mut()
                .expect("judge alternatives")
                .retain(|choice| choice["properties"]["action"]["enum"][0] != "ask");
        }
        Judging::ExhaustedChecks => prompt.push_str(&format!("\nAutomatic check repair is exhausted: the judged attempt failed because the supervisor checks still failed after all {REPAIR_ROUNDS} repair rounds. This is recovery of an actual failure. The repair count stays at {REPAIR_ROUNDS}; it is neither reset nor extended, and a repair plan gets one worker turn before the checks decide again. Choose repair, revise or insert when the cause is within reach. Ask the owner only when a genuine product decision blocks progress; permissions and sandbox limits are never questions, and missing local prerequisites use the dependency wait.")),
        Judging::Failure => {}
    }
}

pub(super) fn describe(judgment: &agent::Judgment) -> String {
    match judgment {
        agent::Judgment::Repair { reason, .. } => format!("Repair: {reason}"),
        agent::Judgment::Revise { reason, .. } => {
            format!("Task revised by the failure judge: {reason}")
        }
        agent::Judgment::Insert { tasks, reason } => format!(
            "{} prerequisite task(s) inserted by the failure judge: {reason}",
            tasks.len()
        ),
        agent::Judgment::Ask { reason, .. } => format!("Owner decision needed: {reason}"),
    }
}

/// Applied once the judged attempt has ended, so ordinary revision and insertion rules hold.
pub(super) fn apply_judgment(
    service: &mut Service,
    run: &Run,
    paths: &Paths,
    workspace: &Workspace,
    judgment: agent::Judgment,
) -> Result<()> {
    let task = service
        .task(run.task)?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    let target = VersionedTask {
        id: task.id,
        version: task.version,
    };
    let ready = service
        .preferences(&paths.config)
        .is_ok_and(|p| p.planner_approval != PlannerApproval::ReviewRequired);
    let judge = Actor {
        kind: ActorKind::Ai,
        name: "judge".into(),
    };
    match judgment {
        agent::Judgment::Revise {
            title,
            goal,
            criteria,
            models,
            reason,
        } => {
            let mut definition = run.definition.clone();
            if let Some(limit) = definition.attempt_limit {
                let used = task.attempts_used;
                if limit <= used {
                    definition.attempt_limit = Some(used.saturating_add(1));
                }
            }
            definition.title = title;
            definition.goal = goal;
            definition.criteria = criteria;
            remediation::apply_role_models(&mut definition, &models)?;
            service.validate_definition_models(&definition)?;
            service.revise_as(target, &definition, ready, &judge, Some(&reason))?;
        }
        agent::Judgment::Insert { tasks, reason } => {
            let defaults = service.defaults()?;
            let repairs: Vec<_> = tasks
                .iter()
                .map(|t| Definition {
                    title: t.title.clone(),
                    goal: t.goal.clone(),
                    kind: run.definition.kind,
                    category: run.definition.category.clone(),
                    lenses: run.definition.lenses.clone(),
                    criteria: t.criteria.clone(),
                    models: defaults.clone(),
                    attempt_limit: Some(3),
                    timeout_ms: run.definition.timeout_ms,
                    stage_timeout_ms: run.definition.stage_timeout_ms,
                })
                .collect();
            if service.attempt_delivery(run)?.stage == Stage::Work {
                remediation::preserve(service, workspace, target)?;
            }
            service.apply_remediation(target, &repairs, &reason, ready)?;
        }
        agent::Judgment::Repair { .. } | agent::Judgment::Ask { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Judging, REPAIR_ROUNDS, agent, restrict};

    fn actions(judging: Judging) -> (String, Vec<String>) {
        let mut prompt = String::new();
        let mut schema = agent::judge_schema();
        restrict(&mut prompt, &mut schema, judging);
        let actions = schema["properties"]["judgment"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["properties"]["action"]["enum"][0].to_string())
            .collect();
        (prompt, actions)
    }

    #[test]
    fn only_normal_rounds_withhold_the_owner_question() {
        let (prompt, round) = actions(Judging::Round);
        assert_eq!(round, [r#""repair""#, r#""revise""#, r#""insert""#]);
        assert!(prompt.contains("Do not ask the owner during these rounds"));
        let (prompt, exhausted) = actions(Judging::ExhaustedChecks);
        assert_eq!(exhausted.last().map(String::as_str), Some(r#""ask""#));
        assert_eq!(exhausted.len(), 4);
        assert!(!prompt.contains("Do not ask the owner"));
        assert!(prompt.contains(&format!(
            "The repair count stays at {REPAIR_ROUNDS}; it is neither reset nor extended"
        )));
        assert!(
            prompt.contains(
                "permissions and sandbox limits are never questions, and missing local prerequisites use the dependency wait"
            )
        );
        let (prompt, failure) = actions(Judging::Failure);
        assert_eq!(failure, exhausted);
        assert!(prompt.is_empty());
    }
}
